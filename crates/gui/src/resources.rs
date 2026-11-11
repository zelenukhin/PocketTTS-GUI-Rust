//! G4: единый резолвер ресурсов (портативная сборка + режим разработки).
//!
//! Приоритет:
//! 1. явный путь пользователя (абсолютный; относительный трактуется от
//!    каталога exe, а не от текущего рабочего каталога/ярлыка);
//! 2. подкаталоги портативного дистрибутива рядом с exe:
//!    `models/`, `onnxruntime/cpu/`, `onnxruntime/cuda/`, `ffmpeg/`;
//! 3. каталоги проекта (`Cargo.toml` + `deps/`) — только режим разработки;
//! 4. существующий документированный fallback (dev-модели).
//!
//! Портативный режим НЕ требует `Cargo.toml` и не зависит от CWD.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

use crate::settings::{DeviceChoice, GuiSettings, ModelChoice};

/// Каталог установленного exe. Все относительные пользовательские пути
/// считаются от него — запуск по ярлыку с другим CWD ничего не ломает.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Путь из настроек: абсолютный — как есть, относительный — от каталога exe.
/// Пустая строка остаётся пустой (означает «не задано/авто»).
pub fn resolve_user_path(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw.is_empty() {
        return PathBuf::new();
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        exe_dir().join(p)
    }
}

/// Корень проекта разработки (Cargo.toml + deps/) — от exe и от cwd.
/// В портативном дистрибутиве не находится, и это нормально.
pub fn find_project_root() -> Option<PathBuf> {
    let is_root = |d: &Path| d.join("Cargo.toml").is_file() && d.join("deps").is_dir();
    if let Ok(exe) = std::env::current_exe() {
        for a in exe.ancestors() {
            if is_root(a) {
                return Some(a.to_path_buf());
            }
        }
    }
    std::env::current_dir().ok().and_then(|cwd| {
        cwd.ancestors()
            .find(|a| is_root(a))
            .map(|a| a.to_path_buf())
    })
}

/// Документированный dev-fallback: модели проекта на машине разработчика.
/// Используется ТОЛЬКО если рядом с exe и в проекте моделей нет; на чистой
/// машине путь отсутствует и на портативность не влияет.
const DEV_ONNX_FALLBACK: &str = r"C:\Users\alexa\Downloads\pocket_tts_android\build\onnx";

/// Каталоги, где может лежать база ONNX-моделей (`ru-v2-fast|slow` внутри).
fn model_base_candidates_for(exe: &Path) -> Vec<PathBuf> {
    let mut v = vec![exe.join("models"), exe.join("onnx")];
    if let Some(parent) = exe.parent() {
        v.push(parent.join("models"));
        v.push(parent.join("onnx"));
    }
    if let Some(dev) = find_project_root() {
        v.push(dev.join("models"));
        v.push(dev.join("deps").join("models"));
        v.push(dev.join("onnx"));
    }
    v.push(PathBuf::from(DEV_ONNX_FALLBACK));
    v
}

pub fn model_base_candidates() -> Vec<PathBuf> {
    model_base_candidates_for(&exe_dir())
}

fn resolve_model_base_at(exe: &Path, s: &GuiSettings) -> Result<PathBuf> {
    if !s.onnx_base.trim().is_empty() {
        let p = resolve_user_path(&s.onnx_base);
        anyhow::ensure!(
            p.is_dir(),
            "каталог ONNX (onnx_base) не найден: {}",
            p.display()
        );
        return Ok(p);
    }
    // G6.3: INT8-варианты живут в тех же каталогах (mixed-графы).
    let wanted = match s.model {
        ModelChoice::Fast | ModelChoice::FastInt8 => Some("ru-v2-fast"),
        ModelChoice::Slow | ModelChoice::SlowInt8 => Some("ru-v2-slow"),
        ModelChoice::Custom => None,
    };
    let mut tried = Vec::new();
    for cand in model_base_candidates_for(exe) {
        if !cand.is_dir() {
            continue;
        }
        tried.push(cand.display().to_string());
        let has_model = match wanted {
            Some(name) => cand.join(name).join("flow_lm_main.onnx").is_file(),
            None => {
                cand.join("ru-v2-fast").join("flow_lm_main.onnx").is_file()
                    || cand.join("ru-v2-slow").join("flow_lm_main.onnx").is_file()
            }
        };
        if has_model {
            return Ok(cand);
        }
    }
    bail!(
        "каталог ONNX-моделей не найден: положите ru-v2-fast/ru-v2-slow в {}\\models или укажите onnx_base (проверены: {})",
        exe.display(),
        if tried.is_empty() {
            model_base_candidates_for(exe)
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            tried.join(", ")
        }
    )
}

/// База моделей: явный `onnx_base` (без тихого фолбэка при ошибке) или
/// автопоиск портативного `models/`, затем проектные каталоги.
pub fn resolve_model_base(s: &GuiSettings) -> Result<PathBuf> {
    resolve_model_base_at(&exe_dir(), s)
}

/// torch-venv с CUDA/cuDNN DLL — только dev-fallback для ort-gpu-wheel.
const TORCH_LIB_FALLBACK: &str = r"C:\PocketTTS-GUI\venv\Lib\site-packages\torch\lib";

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_string()
    };
    let (a, b) = (norm(a), norm(b));
    if cfg!(windows) {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

/// Кандидаты ORT для одного корня: (dll, каталоги для prepended PATH).
/// CPU и CUDA разрешаются независимо — разные каталоги и наборы DLL.
fn ort_candidates(root: &Path, cuda: bool) -> Vec<(PathBuf, Vec<PathBuf>)> {
    if cuda {
        let dirs = [
            root.join("onnxruntime").join("cuda"),
            root.join("onnxruntime").join("gpu"),
            root.join("deps").join("portable"),
        ];
        let mut v: Vec<(PathBuf, Vec<PathBuf>)> = dirs
            .into_iter()
            .map(|d| (d.join("onnxruntime.dll"), vec![d]))
            .collect();
        let capi = root
            .join("deps")
            .join("ort-gpu-wheel")
            .join("x")
            .join("onnxruntime")
            .join("capi");
        let mut prepend = vec![capi.clone()];
        let torch = PathBuf::from(TORCH_LIB_FALLBACK);
        if torch.is_dir() {
            prepend.push(torch);
        }
        v.push((capi.join("onnxruntime.dll"), prepend));
        v
    } else {
        vec![
            (
                root.join("onnxruntime").join("cpu").join("onnxruntime.dll"),
                vec![],
            ),
            (root.join("onnxruntime").join("onnxruntime.dll"), vec![]),
            (
                root.join("deps")
                    .join("ort-cpu")
                    .join("onnxruntime-win-x64-1.23.2")
                    .join("lib")
                    .join("onnxruntime.dll"),
                vec![],
            ),
            (
                root.join("deps")
                    .join("ort-cpu")
                    .join("lib")
                    .join("onnxruntime.dll"),
                vec![],
            ),
        ]
    }
}

/// ORT для явно заданного корня ресурсов (используется тестами/CLI-логикой).
/// Явный DLL из настроек имеет приоритет и не заменяется фолбэком.
pub fn resolve_ort_dylib_in(root: &Path, s: &GuiSettings) -> Result<(PathBuf, Vec<PathBuf>)> {
    let cuda = s.device == DeviceChoice::Cuda;
    let explicit = if cuda {
        s.ort_gpu_dll.trim()
    } else {
        s.ort_cpu_dll.trim()
    };
    if !explicit.is_empty() {
        let p = resolve_user_path(explicit);
        anyhow::ensure!(
            p.is_file(),
            "onnxruntime.dll ({}) не найден: {}",
            if cuda { "CUDA" } else { "CPU" },
            p.display()
        );
        let prepend = if cuda {
            vec![p.parent().unwrap_or(Path::new(".")).to_path_buf()]
        } else {
            vec![]
        };
        return Ok((p, prepend));
    }
    let candidates = ort_candidates(root, cuda);
    for (dll, prepend) in &candidates {
        if dll.is_file() {
            return Ok((dll.clone(), prepend.clone()));
        }
    }
    bail!(
        "{} onnxruntime.dll не найден (проверены: {}); иначе укажите путь в настройках",
        if cuda { "CUDA" } else { "CPU" },
        candidates
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// ORT для запуска: exe-портативные каталоги, затем корень проекта (dev).
/// `Cargo.toml`/CWD для портативного режима не обязательны.
pub fn resolve_ort_dylib(s: &GuiSettings) -> Result<(PathBuf, Vec<PathBuf>)> {
    let cuda = s.device == DeviceChoice::Cuda;
    let explicit = if cuda {
        s.ort_gpu_dll.trim()
    } else {
        s.ort_cpu_dll.trim()
    };
    if !explicit.is_empty() {
        return resolve_ort_dylib_in(&exe_dir(), s);
    }

    let mut roots = vec![exe_dir()];
    if let Some(dev) = find_project_root() {
        if !roots.iter().any(|r| same_path(r, &dev)) {
            roots.push(dev);
        }
    }
    let mut candidates = Vec::new();
    for root in &roots {
        candidates.extend(ort_candidates(root, cuda));
    }
    for (dll, prepend) in &candidates {
        if dll.is_file() {
            return Ok((dll.clone(), prepend.clone()));
        }
    }
    bail!(
        "{} onnxruntime.dll не найден (проверены: {}); положите DLL в {}\\onnxruntime\\{} или укажите путь в настройках",
        if cuda { "CUDA" } else { "CPU" },
        candidates
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        exe_dir().display(),
        if cuda { "cuda" } else { "cpu" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{DeviceChoice, GuiSettings, ModelChoice};

    fn tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "shtorm_res_{}_{}_{}",
            std::process::id(),
            tag,
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(path: &Path) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn relative_user_path_is_exe_relative() {
        assert_eq!(resolve_user_path("models"), exe_dir().join("models"));
        assert_eq!(resolve_user_path(""), PathBuf::new());
        let abs = tmpdir("abs").join("x");
        assert_eq!(resolve_user_path(&abs.display().to_string()), abs);
    }

    #[test]
    fn model_base_prefers_portable_models_dir() {
        let exe = tmpdir("portable_exe");
        touch(
            &exe.join("models")
                .join("ru-v2-fast")
                .join("flow_lm_main.onnx"),
        );
        let s = GuiSettings {
            model: ModelChoice::Fast,
            ..Default::default()
        };
        let base = resolve_model_base_at(&exe, &s).unwrap();
        assert_eq!(base, exe.join("models"));
    }

    #[test]
    fn portable_ort_cpu_and_cuda_are_independent() {
        let root = tmpdir("portable_ort");
        let cpu_dir = root.join("onnxruntime").join("cpu");
        let cuda_dir = root.join("onnxruntime").join("cuda");
        touch(&cpu_dir.join("onnxruntime.dll"));
        touch(&cuda_dir.join("onnxruntime.dll"));
        touch(&cuda_dir.join("onnxruntime_providers_cuda.dll"));
        let mut s = GuiSettings {
            device: DeviceChoice::Cpu,
            ..Default::default()
        };
        let (dll, prepend) = resolve_ort_dylib_in(&root, &s).unwrap();
        assert_eq!(dll, cpu_dir.join("onnxruntime.dll"));
        assert!(prepend.is_empty(), "CPU не требует PATH");
        s.device = DeviceChoice::Cuda;
        let (dll, prepend) = resolve_ort_dylib_in(&root, &s).unwrap();
        assert_eq!(dll, cuda_dir.join("onnxruntime.dll"));
        assert_eq!(prepend, vec![cuda_dir], "providers рядом с CUDA DLL");
    }

    #[test]
    fn missing_explicit_dll_has_clear_error() {
        let s = GuiSettings {
            ort_cpu_dll: "onnxruntime/cpu/onnxruntime.dll".into(),
            ..Default::default()
        };
        let err = resolve_ort_dylib_in(&tmpdir("no_root"), &s).unwrap_err();
        assert!(format!("{err:#}").contains("не найден"), "{err:#}");
    }
}
