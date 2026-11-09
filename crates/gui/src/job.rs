//! G2: фоновое задание синтеза из GUI — напрямую `app_core::pipeline::
//! synthesize_stream` в рабочем потоке (НЕ subprocess CLI). Здесь:
//! резолв модели fast/slow/custom, переменные окружения ORT (без изменения
//! системного PATH — только процесс GUI), сборка `PipelineOptions` с паритетом
//! к CLI, события прогресса для UI и итоговый `JobResult` со всеми путями
//! (итог/черновик/диагностический журнал/лог чанков).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use app_core::pipeline::{
    log_path_for, synthesize_stream, write_chunk_log, write_diag_log, ChunkLogMeta, DiagMeta,
    PipelineOptions, RunStatus, StreamError,
};
use app_core::sink::{make_sink, OutputFormat};
use shtorm_engine::Device;
use text_processing::CharRules;

use crate::settings::{DeviceChoice, GuiSettings, ModelChoice, OutputFormatChoice};

/// Полный вход задания: снимок настроек (уже clamped), текст и итоговый путь.
#[derive(Debug, Clone)]
pub struct JobConfig {
    pub settings: GuiSettings,
    pub text: String,
    pub out_path: PathBuf,
}

/// События задания для UI-потока (канал + `ctx.request_repaint`).
#[derive(Debug, Clone)]
pub enum JobEvent {
    Progress {
        done: usize,
        total: usize,
    },
    Log(String),
    /// Финальный итог (последнее событие потока задания).
    Finished(Box<JobResult>),
}

/// Итог задания: статус + человекочитаемая сводка + пути для UI.
#[derive(Debug, Clone)]
pub struct JobResult {
    pub status: JobStatus,
    pub summary: String,
    pub final_path: Option<PathBuf>,
    pub partial_path: Option<PathBuf>,
    pub diag_path: Option<PathBuf>,
    pub chunk_log_path: Option<PathBuf>,
    pub rtf: Option<f32>,
    pub progress: (usize, usize),
}

impl JobResult {
    /// Ошибка до старта конвейера (чтение текста и т.п.) — без путей.
    pub fn failed(summary: String) -> Self {
        Self {
            status: JobStatus::Failed,
            summary,
            final_path: None,
            partial_path: None,
            diag_path: None,
            chunk_log_path: None,
            rtf: None,
            progress: (0, 0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Completed,
    Cancelled,
    Failed,
}

impl JobStatus {
    pub fn label(&self) -> &'static str {
        match self {
            JobStatus::Completed => "Завершено",
            JobStatus::Cancelled => "Отменено",
            JobStatus::Failed => "Ошибка",
        }
    }
}

/// Каталог модели по выбору Fast/Slow/Custom; проверка обязательных файлов
/// движка — как валидация G1, но тем же кодом для реального запуска.
pub fn resolve_model_dir(s: &GuiSettings) -> Result<PathBuf> {
    let dir = match s.model {
        ModelChoice::Fast => Path::new(&s.onnx_base).join("ru-v2-fast"),
        ModelChoice::Slow => Path::new(&s.onnx_base).join("ru-v2-slow"),
        ModelChoice::Custom => PathBuf::from(s.model_dir.trim()),
    };
    anyhow::ensure!(
        dir.join("flow_lm_main.onnx").is_file(),
        "в каталоге модели нет flow_lm_main.onnx: {}",
        dir.display()
    );
    Ok(dir)
}

/// char_rules из runtime.json модели (та же логика, что CLI --char-rules:
/// файла нет — правила не применяются; секции нет — предупреждение).
pub fn load_char_rules(model_dir: &Path) -> Option<CharRules> {
    let p = model_dir.join("runtime.json");
    let raw = std::fs::read_to_string(&p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    match v.get("char_rules") {
        Some(cr) => match serde_json::from_value::<CharRules>(cr.clone()) {
            Ok(rules) => Some(rules),
            Err(e) => {
                eprintln!(
                    "ВНИМАНИЕ: секция char_rules в {} не разобрана: {e}",
                    p.display()
                );
                None
            }
        },
        None => {
            eprintln!(
                "ВНИМАНИЕ: в {} нет секции char_rules — правила не применяются.",
                p.display()
            );
            None
        }
    }
}

/// GuiSettings → PipelineOptions без потери параметров (паритет CLI:
/// значения по умолчанию CLI = значения по умолчанию GuiSettings).
pub fn pipeline_options(
    s: &GuiSettings,
    model_dir: PathBuf,
    char_rules: Option<CharRules>,
) -> PipelineOptions {
    PipelineOptions {
        model_dir,
        voice: PathBuf::from(s.voice_wav.trim()),
        device: match s.device {
            DeviceChoice::Cpu => Device::Cpu,
            DeviceChoice::Cuda => Device::cuda(s.device_id),
        },
        threads: s.threads as usize,
        seed: if s.seed < 0 {
            None
        } else {
            Some(s.seed as u64)
        },
        temperature: s.temperature,
        decode_steps: s.decode_steps as usize,
        eos_threshold: s.eos_threshold,
        brightness_db: s.brightness_db,
        denoise: s.denoise,
        pause_scale: s.pause_scale,
        paragraph_pause_s: s.paragraph_pause_s,
        max_tokens: s.max_tokens,
        char_rules,
        anti_repeat: s.anti_repeat,
    }
}

/// Формат вывода GUI → формат синка.
pub fn output_format(f: OutputFormatChoice) -> OutputFormat {
    match f {
        OutputFormatChoice::Wav => OutputFormat::Wav,
        OutputFormatChoice::Mp3 => OutputFormat::Mp3,
        OutputFormatChoice::Opus => OutputFormat::Opus,
    }
}

/// Имя итогового файла: {stem текстового файла}.{ext} в папке результата.
pub fn derive_out_path(s: &GuiSettings) -> PathBuf {
    let stem = Path::new(&s.text_file)
        .file_stem()
        .map(|st| st.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".into());
    Path::new(&s.out_dir).join(format!("{stem}.{}", s.format.extension_str()))
}

/// Найти корень проекта (каталог с Cargo.toml и deps/) — от exe и от cwd.
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

/// Каталоги torch-venv с CUDA/cuDNN DLL (резерв, если deps/portable пуст).
const TORCH_LIB_FALLBACK: &str = r"C:\PocketTTS-GUI\venv\Lib\site-packages\torch\lib";

/// Разрешение onnxruntime.dll для устройства: явный путь из настроек →
/// deps проекта (CPU: ort-cpu; CUDA: deps/portable — полный комплект
/// providers+CUDA+cuDNN, иначе ort-gpu-wheel + torch-venv lib).
/// Возвращает (dll, каталоги для prepended PATH — только процесс GUI).
pub fn resolve_ort_dylib_in(root: &Path, s: &GuiSettings) -> Result<(PathBuf, Vec<PathBuf>)> {
    let cuda = s.device == DeviceChoice::Cuda;
    let explicit = if cuda {
        s.ort_gpu_dll.trim()
    } else {
        s.ort_cpu_dll.trim()
    };
    if !explicit.is_empty() {
        let p = PathBuf::from(explicit);
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
    if cuda {
        let portable = root.join("deps").join("portable");
        let dll = portable.join("onnxruntime.dll");
        if dll.is_file() {
            return Ok((dll, vec![portable]));
        }
        let capi = root
            .join("deps")
            .join("ort-gpu-wheel")
            .join("x")
            .join("onnxruntime")
            .join("capi");
        let dll = capi.join("onnxruntime.dll");
        if dll.is_file() {
            let mut prepend = vec![capi.clone()];
            let torch = PathBuf::from(TORCH_LIB_FALLBACK);
            if torch.is_dir() {
                prepend.push(torch);
            }
            return Ok((dll, prepend));
        }
        bail!(
            "CUDA onnxruntime.dll не найден: {} или {} (иначе укажите путь в настройках)",
            portable.display(),
            capi.display()
        );
    }
    let dll = root
        .join("deps")
        .join("ort-cpu")
        .join("onnxruntime-win-x64-1.23.2")
        .join("lib")
        .join("onnxruntime.dll");
    anyhow::ensure!(
        dll.is_file(),
        "CPU onnxruntime.dll не найден: {} (укажите путь в настройках)",
        dll.display()
    );
    Ok((dll, vec![]))
}

pub fn resolve_ort_dylib(s: &GuiSettings) -> Result<(PathBuf, Vec<PathBuf>)> {
    let root = find_project_root().context("не найден корень проекта (Cargo.toml + deps/)")?;
    resolve_ort_dylib_in(&root, s)
}

/// Уже инициализированная DLL (ORT грузится в процесс один раз — смена
/// CPU/CUDA требует перезапуска GUI; честная ошибка вместо тихого фолбэка).
static ORT_LOADED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Подготовить окружение и инициализировать ORT (один раз за процесс).
/// PATH/ORТ_DYLIB_PATH меняются ТОЛЬКО в процессе GUI (система не трогается).
pub fn setup_ort_environment(s: &GuiSettings) -> Result<()> {
    let (dll, prepend) = resolve_ort_dylib(s)?;
    if !prepend.is_empty() {
        let old = std::env::var("PATH").unwrap_or_default();
        let joined: String = prepend
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(";");
        std::env::set_var("PATH", format!("{joined};{old}"));
    }
    std::env::set_var("ORT_DYLIB_PATH", &dll);
    if let Some(prev) = ORT_LOADED.get() {
        if prev == &dll {
            return Ok(());
        }
        bail!(
            "onnxruntime уже загружен с {} — для смены CPU/CUDA перезапустите приложение",
            prev.display()
        );
    }
    if !ort::init().with_name("shtorm-gui").commit() {
        bail!("ort::init() не удалось (DLL: {})", dll.display());
    }
    let _ = ORT_LOADED.set(dll);
    Ok(())
}

/// Устройство строкой для журналов (как CLI).
fn device_label(s: &GuiSettings) -> String {
    match s.device {
        DeviceChoice::Cpu => "cpu".into(),
        DeviceChoice::Cuda => format!("cuda {}", s.device_id),
    }
}

/// Выполнить задание синтеза (рабочий поток; UI получает события через
/// `emit`). Любые сбои — Failed с понятной сводкой; пути итога/черновика/
/// журналов возвращаются для отображения. Никаких subprocess-вызовов CLI.
pub fn execute_job(
    cfg: &JobConfig,
    cancel: &Arc<AtomicBool>,
    emit: &dyn Fn(JobEvent),
) -> JobResult {
    let t0 = Instant::now();
    let s = &cfg.settings;
    let mut progress = (0usize, 0usize);

    let fail = |summary: String| {
        let mut r = JobResult::failed(summary);
        r.progress = progress;
        r
    };

    // --- подготовка (ошибки здесь не создают черновиков) ---
    let model_dir = match resolve_model_dir(s) {
        Ok(d) => d,
        Err(e) => return fail(format!("{e:#}")),
    };
    let char_rules = load_char_rules(&model_dir);
    if let Err(e) = setup_ort_environment(s) {
        return fail(format!("{e:#}"));
    }
    // папка результата может не существовать — создаём (безопасно, без удалений)
    if let Some(parent) = cfg.out_path.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return fail(format!(
                    "создать папку результата {}: {e}",
                    parent.display()
                ));
            }
        }
    }
    if cfg.out_path.exists() {
        return fail(format!(
            "итоговый файл уже существует: {} — не перезаписываем (переименуйте или смените папку)",
            cfg.out_path.display()
        ));
    }
    let ffmpeg = if s.ffmpeg_path.trim().is_empty() {
        None
    } else {
        Some(Path::new(s.ffmpeg_path.trim()))
    };
    let sink = match make_sink(
        &cfg.out_path,
        24_000,
        output_format(s.format),
        &s.mp3_preset,
        ffmpeg,
    ) {
        Ok(sink) => sink,
        Err(e) => return fail(format!("{e:#}")),
    };
    let opts = pipeline_options(s, model_dir.clone(), char_rules);

    emit(JobEvent::Log(format!(
        "запуск: {} | {} | воркеров {} | {} | {}",
        cfg.out_path.display(),
        device_label(s),
        s.effective_workers(),
        s.format.label(),
        s.mp3_preset
    )));

    // --- синтез (прогресс → UI; отмена → Cancelled без публикации) ---
    let result = synthesize_stream(
        &cfg.text,
        &opts,
        s.effective_workers() as usize,
        sink,
        |done, total| {
            progress = (done, total);
            emit(JobEvent::Progress { done, total });
        },
        cancel,
    );
    let elapsed = t0.elapsed().as_secs_f32();

    let mut res = match result {
        Ok(outcome) => finish_from_outcome(cfg, outcome, elapsed, cancel),
        Err(e) => finish_from_error(cfg, &e, elapsed),
    };
    res.progress = progress;
    res
}

/// Успешный возврат synthesize_stream: Completed (итог + RTF + лог чанков)
/// или Cancelled (черновик + диагностический журнал, итог не публикуется).
fn finish_from_outcome(
    cfg: &JobConfig,
    o: app_core::pipeline::StreamOutcome,
    elapsed: f32,
    cancel: &AtomicBool,
) -> JobResult {
    let total_s = (o.speech_samples + o.pause_samples) as f64 / o.sample_rate as f64;
    let rtf = if total_s > 1e-9 {
        Some((elapsed as f64 / total_s) as f32)
    } else {
        None
    };
    if o.status == RunStatus::Cancelled {
        let reason = if cancel.load(Ordering::SeqCst) {
            "отменено пользователем"
        } else {
            "отменено"
        };
        let diag = write_diag_log(
            &cfg.out_path,
            "Cancelled",
            &format!("{reason}; черновик сохранён: {}", o.partial_path.display()),
            &DiagMeta::from_outcome(&o),
        );
        return JobResult {
            status: JobStatus::Cancelled,
            summary: format!(
                "ОТМЕНЕНО на {}/{} чанках; итоговый файл не создан; черновик: {}",
                o.chunks_done,
                o.total_chunks,
                o.partial_path.display()
            ),
            final_path: None,
            partial_path: Some(o.partial_path),
            diag_path: diag,
            chunk_log_path: None,
            rtf,
            progress: (o.chunks_done, o.total_chunks),
        };
    }
    // Completed: RTF/паузы/денойз в сводку; лог чанков опционален (сбой
    // журнала не маскирует успешное аудио)
    let speech_s = o.speech_samples as f64 / o.sample_rate as f64;
    let mut summary = format!(
        "ГОТОВО: {} ({:.2} с: речь {:.2} с + паузы {:.2} с; чанков {}; RTF {:.3})",
        cfg.out_path.display(),
        total_s,
        speech_s,
        total_s - speech_s,
        o.chunks.len(),
        rtf.unwrap_or(0.0)
    );
    summary.push_str(&format!(
        "; воркеров {}, base_seed {}, денойз: {}",
        o.workers,
        o.base_seed,
        o.denoise_report.summary()
    ));
    let mut chunk_log_path = None;
    if cfg.settings.chunk_log {
        let path = log_path_for(&cfg.out_path);
        let meta = ChunkLogMeta {
            device: device_label(&cfg.settings),
            format: cfg.settings.format.label().to_string(),
            preset: cfg.settings.mp3_preset.clone(),
        };
        match write_chunk_log(&path, &o, &meta) {
            Ok(()) => chunk_log_path = Some(path),
            Err(e) => summary.push_str(&format!("; ВНИМАНИЕ: лог чанков не создан: {e:#}")),
        }
    }
    JobResult {
        status: JobStatus::Completed,
        summary,
        final_path: o.path,
        partial_path: None,
        diag_path: None,
        chunk_log_path,
        rtf,
        progress: (o.chunks_done, o.total_chunks),
    }
}

/// Ошибка synthesize_stream: типизированный StreamError (прогресс/seed/
/// черновик уже внутри) + диагностический журнал рядом с итогом.
fn finish_from_error(cfg: &JobConfig, e: &anyhow::Error, elapsed: f32) -> JobResult {
    let _ = elapsed;
    if let Some(se) = e.downcast_ref::<StreamError>() {
        let diag = write_diag_log(
            &cfg.out_path,
            "Failed",
            &format!("{}\nпричина: ошибка выполнения задания", se.message),
            &DiagMeta::from_error(se),
        );
        return JobResult {
            status: JobStatus::Failed,
            summary: format!("ОШИБКА: {se}"),
            final_path: None,
            partial_path: Some(se.partial.clone()),
            diag_path: diag,
            chunk_log_path: None,
            rtf: None,
            progress: (se.chunks_done, se.total),
        };
    }
    JobResult {
        status: JobStatus::Failed,
        summary: format!("ОШИБКА: {e:#}"),
        final_path: None,
        partial_path: None,
        diag_path: None,
        chunk_log_path: None,
        rtf: None,
        progress: (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{ModelChoice, OutputFormatChoice};

    fn tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "shtorm_job_{}_{}_{}",
            std::process::id(),
            tag,
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_model(base: &Path, name: &str) -> PathBuf {
        let d = base.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("flow_lm_main.onnx"), b"fake").unwrap();
        d
    }

    /// GuiSettings → PipelineOptions: КАЖДЫЙ параметр переносится без потерь
    /// (регрессия «конверсия без потери параметров»).
    #[test]
    fn pipeline_options_carries_all_settings() {
        let s = GuiSettings {
            voice_wav: r"C:\v\voice.wav".into(),
            device: DeviceChoice::Cuda,
            device_id: 1,
            threads: 3,
            seed: 123456,
            temperature: 0.55,
            decode_steps: 2,
            eos_threshold: -3.5,
            brightness_db: 6.0,
            denoise: true,
            pause_scale: 0.75,
            paragraph_pause_s: 1.25,
            max_tokens: 80,
            anti_repeat: false,
            ..Default::default()
        };
        let dir = PathBuf::from(r"C:\m");
        let rules = CharRules::default();
        let o = pipeline_options(&s, dir.clone(), Some(rules.clone()));
        assert_eq!(o.model_dir, dir);
        assert_eq!(o.voice, PathBuf::from(r"C:\v\voice.wav"));
        assert!(o.device.is_cuda());
        assert_eq!(o.threads, 3);
        assert_eq!(o.seed, Some(123456));
        assert_eq!(o.temperature, 0.55);
        assert_eq!(o.decode_steps, 2);
        assert_eq!(o.eos_threshold, -3.5);
        assert_eq!(o.brightness_db, 6.0);
        assert!(o.denoise);
        assert_eq!(o.pause_scale, 0.75);
        assert_eq!(o.paragraph_pause_s, 1.25);
        assert_eq!(o.max_tokens, 80);
        // char_rules передаются по ссылке в задачи — проверяем содержимое
        assert_eq!(
            o.char_rules.map(|r| r.char_map.len()),
            Some(rules.char_map.len())
        );
        assert!(!o.anti_repeat);
        // seed -1 -> случайный
        let s = GuiSettings { seed: -1, ..s };
        assert_eq!(pipeline_options(&s, dir, None).seed, None);
    }

    /// Fast/Slow резолвятся от onnx_base, Custom — по своему пути; отсутствие
    /// flow_lm_main.onnx — явная ошибка.
    #[test]
    fn resolve_model_dir_fast_slow_custom() {
        let base = tmpdir("models");
        fake_model(&base, "ru-v2-fast");
        fake_model(&base, "ru-v2-slow");
        let mut s = GuiSettings {
            onnx_base: base.display().to_string(),
            ..Default::default()
        };
        s.model = ModelChoice::Fast;
        assert_eq!(resolve_model_dir(&s).unwrap(), base.join("ru-v2-fast"));
        s.model = ModelChoice::Slow;
        assert_eq!(resolve_model_dir(&s).unwrap(), base.join("ru-v2-slow"));
        let custom = fake_model(&base, "custom-m");
        s.model = ModelChoice::Custom;
        s.model_dir = custom.display().to_string();
        assert_eq!(resolve_model_dir(&s).unwrap(), custom);
        // пустой каталог — ошибка
        s.model_dir = base.join("nope").display().to_string();
        let err = resolve_model_dir(&s).unwrap_err();
        assert!(format!("{err:#}").contains("flow_lm_main.onnx"), "{err:#}");
    }

    #[test]
    fn out_path_uses_text_stem_and_format_ext() {
        let s = GuiSettings {
            text_file: r"C:\books\Глава 12.md".into(),
            out_dir: r"C:\out".into(),
            format: OutputFormatChoice::Opus,
            ..Default::default()
        };
        assert_eq!(derive_out_path(&s), PathBuf::from(r"C:\out\Глава 12.opus"));
    }

    /// Явный DLL из настроек приоритетен (и ошибки понятные); авто-поиск CPU
    /// находит deps/ort-cpu относительно корня.
    #[test]
    fn resolve_ort_dylib_explicit_and_auto() {
        let d = tmpdir("ort");
        let dll = d.join("onnxruntime.dll");
        std::fs::write(&dll, b"dll").unwrap();
        let s = GuiSettings {
            ort_cpu_dll: dll.display().to_string(),
            ..Default::default() // CPU
        };
        let (got, prepend) = resolve_ort_dylib_in(Path::new("C:/no-such-root"), &s).unwrap();
        assert_eq!(got, dll);
        assert!(prepend.is_empty(), "CPU не требует PATH");
        // CUDA: явный DLL -> его каталог в PATH (providers рядом)
        let gpu = d.join("onnxruntime-gpu.dll");
        std::fs::write(&gpu, b"dll").unwrap();
        let s = GuiSettings {
            device: DeviceChoice::Cuda,
            ort_gpu_dll: gpu.display().to_string(),
            ..s
        };
        let (got, prepend) = resolve_ort_dylib_in(Path::new("C:/no-such-root"), &s).unwrap();
        assert_eq!(got, gpu);
        assert_eq!(prepend, vec![d.clone()]);
        // отсутствующий явный путь — ошибка без фолбэка
        let s = GuiSettings {
            device: DeviceChoice::Cuda,
            ort_gpu_dll: d.join("missing.dll").display().to_string(),
            ..Default::default()
        };
        assert!(resolve_ort_dylib_in(Path::new("C:/no-such-root"), &s).is_err());
        // авто CPU: структура deps/ort-cpu в корне
        let root = tmpdir("root");
        let cpu_auto = root
            .join("deps")
            .join("ort-cpu")
            .join("onnxruntime-win-x64-1.23.2")
            .join("lib")
            .join("onnxruntime.dll");
        std::fs::create_dir_all(cpu_auto.parent().unwrap()).unwrap();
        std::fs::write(&cpu_auto, b"dll").unwrap();
        std::fs::write(root.join("Cargo.toml"), b"").unwrap();
        let s = GuiSettings::default();
        let (got, _) = resolve_ort_dylib_in(&root, &s).unwrap();
        assert_eq!(got, cpu_auto);
    }

    /// char_rules читаются из runtime.json модели; нет секции/файла — None.
    #[test]
    fn char_rules_from_runtime_json() {
        let d = tmpdir("crules");
        assert!(load_char_rules(&d).is_none(), "нет файла — нет правил");
        std::fs::write(d.join("runtime.json"), r#"{"other": 1}"#).unwrap();
        assert!(load_char_rules(&d).is_none(), "нет секции — нет правил");
        std::fs::write(
            d.join("runtime.json"),
            r#"{"char_rules": {"char_map": {"ё": "е"}}}"#,
        )
        .unwrap();
        let rules = load_char_rules(&d).expect("секция есть");
        assert_eq!(
            rules.char_map.get("ё").and_then(|v| v.as_str()),
            Some("е"),
            "правила разобраны: {:?}",
            rules.char_map
        );
    }

    /// execute_job до ORT: битый каталог модели — Failed с понятной сводкой,
    /// без черновиков; существующий итог — отказ без перезаписи.
    #[test]
    fn execute_job_fails_cleanly_before_ort() {
        let d = tmpdir("preort");
        let text_file = d.join("book.txt");
        std::fs::write(&text_file, "Текст.").unwrap();
        let mut s = GuiSettings {
            text_file: text_file.display().to_string(),
            out_dir: d.join("out").display().to_string(),
            ..Default::default()
        };
        s.onnx_base = d.join("no-models").display().to_string();
        let cfg = JobConfig {
            settings: s.clone(),
            text: "Текст.".into(),
            out_path: d.join("out").join("book.wav"),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let res = execute_job(&cfg, &cancel, &|_| {});
        assert_eq!(res.status, JobStatus::Failed);
        assert!(res.summary.contains("flow_lm_main.onnx"), "{}", res.summary);
        assert!(res.partial_path.is_none());
        assert!(res.diag_path.is_none());
        assert!(!cfg.out_path.exists());

        // существующий итог — отказ, содержимое не трогаем
        let base = tmpdir("preort2");
        fake_model(&base, "ru-v2-slow");
        s.onnx_base = base.display().to_string();
        std::fs::create_dir_all(cfg.out_path.parent().unwrap()).unwrap();
        std::fs::write(&cfg.out_path, b"keep").unwrap();
        let res = execute_job(
            &JobConfig {
                settings: s,
                text: "Текст.".into(),
                out_path: cfg.out_path.clone(),
            },
            &cancel,
            &|_| {},
        );
        assert_eq!(res.status, JobStatus::Failed);
        assert!(res.summary.contains("уже существует"), "{}", res.summary);
        assert_eq!(std::fs::read(&cfg.out_path).unwrap(), b"keep");
    }

    // ============ реальные прогоны (нужны модели + ORT DLL + ffmpeg) ============
    // Запускать ПО ОДНОМУ устройству на процесс (ORT грузится в процесс
    // один раз; смена CPU/CUDA внутри процесса невозможна — честная ошибка):
    //   cargo test -p shtorm-gui --release --lib real_matrix -- --ignored --nocapture
    //   SHTORM_GUI_TEST_DEVICE=cuda cargo test -p shtorm-gui --release --lib real_matrix -- --ignored --nocapture

    const REAL_ONNX_BASE: &str = r"C:\Users\alexa\Downloads\pocket_tts_android\build\onnx";
    const REAL_VOICE: &str = r"C:\PocketTTS-GUI\examples\Microsoft Brian Speed 1.wav";
    const REAL_TEXT: &str = "Стр+елки час+ов показ+ывали п+олночь, а стрелк+и заст+ыли. Вт+орое предложение для над+ёжности прогона. Тр+етий хв+остик текста.";
    const REAL_FFMPEG: &str = r"C:\PocketTTS-GUI\ffmpeg\ffmpeg.exe";

    fn test_device() -> DeviceChoice {
        if std::env::var("SHTORM_GUI_TEST_DEVICE").as_deref() == Ok("cuda") {
            DeviceChoice::Cuda
        } else {
            DeviceChoice::Cpu
        }
    }

    fn real_settings(fmt: OutputFormatChoice, out_dir: &Path) -> GuiSettings {
        let mut s = GuiSettings {
            model: ModelChoice::Slow,
            onnx_base: REAL_ONNX_BASE.into(),
            voice_wav: REAL_VOICE.into(),
            device: test_device(),
            device_id: 0,
            threads: 4,
            seed: 42,
            temperature: 0.3,
            decode_steps: 1,
            eos_threshold: -4.0,
            brightness_db: 8.0,
            denoise: false,
            anti_repeat: true,
            pause_scale: 1.0,
            paragraph_pause_s: 0.9,
            max_tokens: 40,
            workers: Some(match test_device() {
                DeviceChoice::Cpu => 2,
                DeviceChoice::Cuda => 1,
            }),
            format: fmt,
            out_dir: out_dir.display().to_string(),
            ..Default::default()
        };
        if fmt != OutputFormatChoice::Wav {
            s.ffmpeg_path = REAL_FFMPEG.into();
            s.mp3_preset = match fmt {
                OutputFormatChoice::Opus => "opus 64k".into(),
                _ => "-V2 (Высокое)".into(),
            };
        }
        s.clamped();
        s
    }

    fn run_real(tag: &str, fmt: OutputFormatChoice) -> JobResult {
        let d = tmpdir(tag);
        let s = real_settings(fmt, &d);
        let out = derive_out_path(&s);
        let cfg = JobConfig {
            settings: s,
            text: REAL_TEXT.into(),
            out_path: out,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let res = execute_job(&cfg, &cancel, &|_| {});
        eprintln!("[{tag}] {} | {}", res.status.label(), res.summary);
        res
    }

    /// Матрица одного устройства: WAV, MP3/Opus, отмена после первого чанка,
    /// повторный запуск после отмены; для CPU — побитовый паритет с CLI.
    #[test]
    #[ignore = "реальный синтез: модели + ORT DLL; запускать отдельными командами (см. комментарий выше)"]
    fn real_matrix_device() {
        // 1) WAV
        let res = run_real("g2_wav", OutputFormatChoice::Wav);
        assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
        assert!(res.final_path.expect("итог создан").is_file());
        assert!(res.rtf.expect("RTF посчитан") > 0.0);

        // 2) MP3 (CPU) / Opus (CUDA)
        let is_cuda = test_device() == DeviceChoice::Cuda;
        let fmt = if is_cuda {
            OutputFormatChoice::Opus
        } else {
            OutputFormatChoice::Mp3
        };
        let res = run_real("g2_enc", fmt);
        assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
        assert!(res.final_path.expect("итог").is_file());

        // 3) отмена после первого готового чанка
        let d = tmpdir("g2_cancel");
        let mut s = real_settings(OutputFormatChoice::Wav, &d);
        s.max_tokens = 20; // гарантированно несколько чанков
        let out = derive_out_path(&s);
        let cfg = JobConfig {
            settings: s,
            text: REAL_TEXT.into(),
            out_path: out.clone(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        let res = execute_job(&cfg, &cancel, &|ev| {
            if let JobEvent::Progress { done, .. } = &ev {
                if *done >= 1 {
                    c2.store(true, Ordering::SeqCst);
                }
            }
        });
        assert_eq!(res.status, JobStatus::Cancelled, "{}", res.summary);
        assert!(!out.exists(), "итог при отмене не публикуется");
        let partial = res.partial_path.expect("черновик сообщён");
        assert!(partial.is_file(), "черновик остаётся: {partial:?}");
        assert!(res.diag_path.expect("диагностический журнал").is_file());
        assert!(res.progress.0 >= 1, "отмена после готового чанка");

        // 4) повторный запуск после отмены — Completed на том же пути
        let res = execute_job(
            &JobConfig {
                settings: cfg.settings.clone(),
                text: REAL_TEXT.into(),
                out_path: out.clone(),
            },
            &Arc::new(AtomicBool::new(false)),
            &|_| {},
        );
        assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
        assert!(out.is_file(), "итог создан повторным запуском");

        // 5) побитовый паритет GUI == CLI (только CPU: отдельный CLI-процесс)
        if !is_cuda {
            let d = tmpdir("g2_parity");
            let s = real_settings(OutputFormatChoice::Wav, &d);
            let out = derive_out_path(&s);
            let res = execute_job(
                &JobConfig {
                    settings: s.clone(),
                    text: REAL_TEXT.into(),
                    out_path: out.clone(),
                },
                &Arc::new(AtomicBool::new(false)),
                &|_| {},
            );
            assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);

            let cli = crate::job::find_project_root()
                .and_then(|r| {
                    let p = r.join("target").join("release").join("synthesize.exe");
                    p.is_file().then_some(p)
                })
                .expect("target/release/synthesize.exe (cargo build --release --bin synthesize)");
            let cli_out = d.join("cli_book.wav");
            let text_file = d.join("parity_text.txt");
            std::fs::write(&text_file, REAL_TEXT).unwrap();
            let (dll, _) = resolve_ort_dylib(&s).expect("dll");
            let model = resolve_model_dir(&s).unwrap().display().to_string();
            let st = std::process::Command::new(&cli)
                .args([
                    "--model-dir",
                    &model,
                    "--voice",
                    &s.voice_wav,
                    "--text-file",
                    &text_file.display().to_string(),
                    "--out",
                    &cli_out.display().to_string(),
                    "--format",
                    "wav",
                    "--device",
                    "cpu",
                    "--threads",
                    "4",
                    "--workers",
                    "2",
                    "--temperature",
                    "0.3",
                    "--decode-steps",
                    "1",
                    "--eos-threshold",
                    "-4.0",
                    "--seed",
                    "42",
                    "--brightness-db",
                    "8.0",
                    "--pause-scale",
                    "1.0",
                    "--paragraph-pause-s",
                    "0.9",
                    "--max-tokens",
                    "40",
                ])
                .env("ORT_DYLIB_PATH", &dll)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("запуск CLI");
            assert!(st.success(), "CLI завершился ошибкой: {st:?}");
            let gui_bytes = std::fs::read(&out).unwrap();
            let cli_bytes = std::fs::read(&cli_out).unwrap();
            assert_eq!(
                gui_bytes, cli_bytes,
                "GUI и CLI WAV обязаны совпадать побитово при одинаковых настройках"
            );
            eprintln!(
                "PARITY OK: {} байт, GUI == CLI ({})",
                gui_bytes.len(),
                out.display()
            );
        }
    }
}
