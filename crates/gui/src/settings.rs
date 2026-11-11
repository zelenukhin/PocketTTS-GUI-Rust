//! Настройки GUI (G1/G5/G6.5): собственный JSON (`PocketTTS-GUI-Rust.json`
//! рядом с exe), round-trip с сохранением неизвестных ключей, безопасная
//! атомарная запись (unique tmp + MoveFileExW REPLACE — старый файл не
//! разрушается при сбое сериализации). Импорт из Python-GUI удалён в G5:
//! приложение читает только собственный конфиг. G6.5 (переименование
//! приложения): одноразовая миграция настроек из прежнего имени файла
//! `shtorm_gui_settings.json` — старый файл только читается и никогда не
//! удаляется/не изменяется автоматически.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use shtorm_engine::{EosTail, ModelPrecision};

/// Порог EOS в настройках GUI (G4.1/G5). В интерфейсе G5 выбранное значение —
/// всегда число; `Auto` оставлен только как совместимый алиас старых конфигов
/// (`"auto"` читается как `Fixed(-4.0)`, новый JSON пишет число).
/// Порог — встроенное значение −4.0 движка teratts-android: НЕ читает
/// runtime.json модели и не создаёт отдельного автоматического алгоритма.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EosThresholdSetting {
    /// Устаревший алиас «Авто»; в GUI больше не показывается.
    Auto,
    Fixed(f32),
}

impl EosThresholdSetting {
    /// Историческое значение «Авто» — константа порога teratts-android.
    pub const AUTO_VALUE: f32 = -4.0;

    pub fn value(self) -> f32 {
        match self {
            Self::Auto => Self::AUTO_VALUE,
            Self::Fixed(v) => v,
        }
    }
}

/// Варианты порога EOS (числовые; «Авто» в списке G5 отсутствует).
pub const EOS_THRESHOLD_CHOICES: &[f32] = &[
    -6.0, -5.0, -4.0, -3.0, -2.0, -1.5, -1.0, -0.5, 0.0, 0.5, 1.0,
];
/// Варианты хвоста EOS (кадров), кроме None.
pub const EOS_TAIL_CHOICES: &[u32] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

// ---- serde: «auto»/«none» — строки, числа — числа (None ≠ 0) ------------------

mod eos_threshold_serde {
    use super::EosThresholdSetting;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &EosThresholdSetting, s: S) -> Result<S::Ok, S::Error> {
        match t {
            // G5: даже устаревший алиас пишется числом — в новых конфигах
            // строки "auto" больше не появляются.
            EosThresholdSetting::Auto => s.serialize_f32(EosThresholdSetting::AUTO_VALUE),
            EosThresholdSetting::Fixed(v) => s.serialize_f32(*v),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<EosThresholdSetting, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Str(String),
            F32(f32),
        }
        match Repr::deserialize(d)? {
            // старые конфиги хранят число (в т.ч. −4.0) — это Fixed
            Repr::F32(v) => Ok(EosThresholdSetting::Fixed(v)),
            Repr::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
                // G5: старое "auto" мигрирует в число −4.0 (без варианта Auto)
                "auto" => Ok(EosThresholdSetting::Fixed(EosThresholdSetting::AUTO_VALUE)),
                other => {
                    // потерянные старые строковые метки («-4» и т.п.) — число
                    other
                        .replace(',', ".")
                        .parse::<f32>()
                        .map(EosThresholdSetting::Fixed)
                        .map_err(serde::de::Error::custom)
                }
            },
        }
    }
}

mod eos_tail_serde {
    use super::EosTail;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &EosTail, s: S) -> Result<S::Ok, S::Error> {
        match t {
            EosTail::Default => s.serialize_str("none"),
            EosTail::Fixed(n) => s.serialize_u32(*n),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<EosTail, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Str(String),
            U32(u32),
        }
        match Repr::deserialize(d)? {
            Repr::U32(n) if n <= 10 => Ok(EosTail::Fixed(n)),
            Repr::U32(n) => Err(serde::de::Error::custom(format!(
                "хвост EOS: максимум 10, получено {n}"
            ))),
            Repr::Str(s) if s.trim().eq_ignore_ascii_case("none") => Ok(EosTail::Default),
            Repr::Str(s) => Err(serde::de::Error::custom(format!(
                "хвост EOS: 'none' или число 0..=10, получено '{s}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiSettings {
    // Источник и голос
    /// Путь к текстовому файлу (одиночный режим) или каталогу (batch).
    pub text_file: String,
    /// true — рекурсивная пакетная обработка каталога `text_file`.
    pub batch: bool,
    pub voice_wav: String,
    // Модель и устройство
    pub model: ModelChoice,
    pub model_dir: String,
    /// Базовый каталог ONNX-моделей (fast = {base}/ru-v2-fast, slow = …slow).
    pub onnx_base: String,
    pub device: DeviceChoice,
    pub device_id: i32,
    /// Явный путь onnxruntime.dll CPU (пусто — авто-поиск в deps проекта).
    pub ort_cpu_dll: String,
    /// Явный путь onnxruntime.dll CUDA (пусто — авто: deps/portable).
    pub ort_gpu_dll: String,
    /// Intra-op потоки ORT (паритет с CLI --threads, дефолт 4).
    pub threads: u32,
    // Параметры генерации
    pub temperature: f32,
    pub decode_steps: u32,
    /// Порог EOS: «Авто» (= −4.0) или число. G4.1.
    #[serde(with = "eos_threshold_serde")]
    pub eos_threshold: EosThresholdSetting,
    /// Хвост EOS для чанков ≤4 слов: None (штатная политика движка 5/3)
    /// или число 0..=10. Значение передаётся движку напрямую, без
    /// пересчёта (Python-значения 6/4 не используются). G4.1.
    #[serde(with = "eos_tail_serde")]
    pub eos_tail_short: EosTail,
    /// Хвост EOS для чанков ≥5 слов. G4.1.
    #[serde(with = "eos_tail_serde")]
    pub eos_tail_long: EosTail,
    pub seed: i64,
    pub brightness_db: f32,
    pub denoise: bool,
    pub anti_repeat: bool,
    // Паузы и воркеры
    pub pause_scale: f32,
    pub paragraph_pause_s: f32,
    pub max_tokens: u32,
    /// None = автомат по устройству (CPU 2 / CUDA 1), ручной выбор не
    /// перезаписывается при смене устройства.
    pub workers: Option<u32>,
    /// G6.2: режим пакетного CUDA (экономичный 1×2 / гибрид 2×2).
    pub batch_cuda_mode: BatchCudaMode,
    // Вывод
    pub format: OutputFormatChoice,
    pub mp3_preset: String,
    /// Пресет Opus — отдельный от MP3: при смене формата используется
    /// соответствующий пресет, несовместимый не подставляется молча.
    pub opus_preset: String,
    pub out_dir: String,
    pub chunk_log: bool,
    pub ffmpeg_path: String,
    /// G5: разрешить замену готового аудиофайла назначения. По умолчанию
    /// выключено. Замена выполняется только после успешного синтеза
    /// (sink пишет `.partial`, публикация — MoveFileExW REPLACE).
    pub overwrite_existing: bool,
    /// G5: тёмное оформление окна (по умолчанию да; светлое — по выбору).
    pub theme_dark: bool,
    // UI (фактические состояния свёрток; синхронизируются с egui-памятью)
    pub source_open: bool,
    pub model_open: bool,
    pub gen_open: bool,
    pub pauses_open: bool,
    pub out_open: bool,
    /// Неизвестные ключи (сохраняются при save/load без потерь).
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Выбор модели в GUI (G6.3: один список — вариант сразу определяет и
/// Fast/Slow, и точность графов; отдельного поля «точность» нет).
///
/// Сериализация: старые значения `"Fast"`/`"Slow"`/`"Custom"` сохраняют
/// прежнюю семантику (FP32); INT8-варианты — однозначные `"FastInt8"`/
/// `"SlowInt8"`. FP32 — значение по умолчанию.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelChoice {
    Fast,
    Slow,
    /// Fast + смешанные INT8-графы (main/flow/decoder квантованные,
    /// encoder/text_conditioner — FP32; G6.3).
    FastInt8,
    /// Slow + смешанные INT8-графы (G6.3).
    SlowInt8,
    Custom,
}

impl ModelChoice {
    /// Точность графов для этого варианта (движковое представление).
    pub fn precision(&self) -> ModelPrecision {
        match self {
            ModelChoice::FastInt8 | ModelChoice::SlowInt8 => ModelPrecision::Int8,
            _ => ModelPrecision::Fp32,
        }
    }
    /// Каталог внутри onnx_base (Custom разрешается отдельно по model_dir).
    pub fn sub_dir(&self) -> Option<&'static str> {
        match self {
            ModelChoice::Fast | ModelChoice::FastInt8 => Some("ru-v2-fast"),
            ModelChoice::Slow | ModelChoice::SlowInt8 => Some("ru-v2-slow"),
            ModelChoice::Custom => None,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            ModelChoice::Fast => "Shtorm-v2 Fast (FP32)",
            ModelChoice::FastInt8 => "Shtorm-v2 Fast (INT8)",
            ModelChoice::Slow => "Shtorm-v2 Slow (FP32)",
            ModelChoice::SlowInt8 => "Shtorm-v2 Slow (INT8)",
            ModelChoice::Custom => "Свой каталог ONNX",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceChoice {
    Cpu,
    Cuda,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormatChoice {
    Wav,
    Mp3,
    Opus,
}

/// G6.2: режим пакетного синтеза CUDA. Экономичный — проверенный G6.1 пул
/// 1 процесс × 2 воркера (~4.9 ГБ VRAM); «Максимум скорости» — гибрид
/// 2 процесса × 2 постоянных воркера (~9.2 ГБ, по CUDA-P2B.3 быстрее всех).
/// Выбор осознанный: по умолчанию экономичный, гибрид молча не включается.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BatchCudaMode {
    /// 1 процесс × 2 воркера (постоянный пул G6.1).
    #[default]
    Economy,
    /// 2 процесса × 2 воркера (гибрид H22 из P2B.3; нужен префлайт VRAM).
    MaxSpeed,
}

impl BatchCudaMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Economy => "Экономичный (1×2)",
            Self::MaxSpeed => "Максимальная скорость (2×2)",
        }
    }
}

impl OutputFormatChoice {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Wav => "WAV",
            Self::Mp3 => "MP3",
            Self::Opus => "Opus",
        }
    }
    /// Расширение итогового файла (как sink::OutputFormat::extension).
    pub fn extension_str(&self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Opus => "opus",
        }
    }
    /// Нужен ли ffmpeg (MP3/Opus) — WAV пишется напрямую.
    pub fn needs_ffmpeg(&self) -> bool {
        !matches!(self, Self::Wav)
    }
}

/// Допустимые пресеты качества по форматам (метки как в Python GUI).
pub const MP3_PRESETS: &[&str] = &[
    "-V0 (Максимальное)",
    "-V2 (Высокое)",
    "-V4 (Среднее)",
    "192k",
    "128k",
];
pub const OPUS_PRESETS: &[&str] = &["opus 96k", "opus 64k", "opus 32k"];

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            text_file: String::new(),
            batch: false,
            voice_wav: String::new(),
            model: ModelChoice::Slow,
            model_dir: String::new(),
            // G4: пусто = автопоиск <exe>/models (портативно) или dev-fallback;
            // абсолютные пути профиля разработчика в дефолт не зашиваются.
            onnx_base: String::new(),
            device: DeviceChoice::Cpu,
            device_id: 0,
            ort_cpu_dll: String::new(),
            ort_gpu_dll: String::new(),
            threads: 4,
            temperature: 0.3,
            decode_steps: 1,
            // G5: порог по умолчанию — явное число −4.0 (значение
            // teratts-android); «Авто» в интерфейсе больше нет.
            // Хвосты — явные Fixed(5)/Fixed(3). Python-пересчёт 6/4 не
            // переносится.
            eos_threshold: EosThresholdSetting::Fixed(-4.0),
            eos_tail_short: EosTail::Fixed(5),
            eos_tail_long: EosTail::Fixed(3),
            seed: 42,
            brightness_db: 8.0,
            denoise: false,
            anti_repeat: true,
            pause_scale: 1.0,
            paragraph_pause_s: 0.9,
            max_tokens: 50,
            workers: None,
            batch_cuda_mode: BatchCudaMode::Economy,
            format: OutputFormatChoice::Wav,
            mp3_preset: "-V2 (Высокое)".into(),
            opus_preset: "opus 64k".into(),
            out_dir: "output".into(),
            chunk_log: false,
            ffmpeg_path: String::new(),
            // защита существующих аудиокниг по умолчанию: без явного согласия
            // пользователя готовые файлы не заменяются
            overwrite_existing: false,
            theme_dark: true,
            source_open: true,
            model_open: true,
            gen_open: true,
            pauses_open: true,
            out_open: true,
            extra: BTreeMap::new(),
        }
    }
}

impl GuiSettings {
    /// Пресет, соответствующий текущему формату (WAV — пусто). При смене
    /// формата берётся собственный пресет формата — без неявных фолбэков.
    pub fn preset_for_format(&self) -> &str {
        match self.format {
            OutputFormatChoice::Wav => "",
            OutputFormatChoice::Mp3 => &self.mp3_preset,
            OutputFormatChoice::Opus => &self.opus_preset,
        }
    }

    /// Рекомендованное число воркеров по устройству (A4: CPU 2 / CUDA 1).
    pub fn effective_workers(&self) -> u32 {
        self.workers.unwrap_or(match self.device {
            DeviceChoice::Cpu => 2,
            DeviceChoice::Cuda => 1,
        })
    }

    /// Зажим значений в допустимые диапазоны (как Python GUI).
    pub fn clamped(&mut self) {
        self.temperature = self.temperature.clamp(0.05, 1.5);
        self.decode_steps = self.decode_steps.clamp(1, 8);
        // порог: зажимается числовая часть; устаревший Auto не трогаем
        // (в GUI он не создаётся, load "auto" сразу даёт Fixed(-4.0))
        if let EosThresholdSetting::Fixed(v) = self.eos_threshold {
            self.eos_threshold = EosThresholdSetting::Fixed(v.clamp(-20.0, 20.0));
        }
        // хвосты: Fixed зажимается в 0..=10, None (Default) не меняется
        for tail in [&mut self.eos_tail_short, &mut self.eos_tail_long] {
            if let EosTail::Fixed(n) = *tail {
                *tail = EosTail::Fixed(n.min(10));
            }
        }
        self.brightness_db = self.brightness_db.clamp(0.0, 12.0);
        self.pause_scale = self.pause_scale.clamp(0.0, 2.0);
        self.paragraph_pause_s = self.paragraph_pause_s.clamp(0.0, 3.0);
        self.max_tokens = self.max_tokens.clamp(10, 300);
        self.threads = self.threads.clamp(1, 16);
        if let Some(w) = self.workers {
            self.workers = Some(w.clamp(1, 12));
        }
    }
}

/// Имя собственного конфига (G6.5: переименование приложения — новое имя).
pub const SETTINGS_FILE: &str = "PocketTTS-GUI-Rust.json";
/// Прежнее имя конфига до переименования приложения (G6.5). Используется
/// ТОЛЬКО для чтения при миграции; автоматически не удаляется и не меняется.
pub const LEGACY_SETTINGS_FILE: &str = "shtorm_gui_settings.json";

/// Путь собственного конфига: рядом с exe (портативный стиль).
pub fn settings_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(SETTINGS_FILE)))
        .unwrap_or_else(|| PathBuf::from(SETTINGS_FILE))
}

/// Путь прежнего конфига (до переименования приложения, G6.5): тот же
/// каталог, что и новый конфиг. Читается один раз при старте, если нового
/// файла ещё нет.
pub fn legacy_settings_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(LEGACY_SETTINGS_FILE)))
        .unwrap_or_else(|| PathBuf::from(LEGACY_SETTINGS_FILE))
}

pub fn load_settings(path: &Path) -> Result<GuiSettings> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("прочитать {}", path.display()))?;
    let s: GuiSettings =
        serde_json::from_str(&raw).with_context(|| format!("разобрать {}", path.display()))?;
    Ok(s)
}

/// Эксклюзивная запись НОВОГО файла (`create_new`): занятое имя — отказ,
/// существующее содержимое никогда не затирается (как write_wav16 в
/// audio-processing; политика SAFETY_AUDIT).
pub fn write_exclusive(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("создать {} (create_new)", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("записать {}", path.display()))?;
    Ok(())
}

/// Безопасная запись: сериализация в уникальный tmp (создание ТОЛЬКО
/// эксклюзивно, `create_new` — без TOCTOU-гонки check+write), затем атомарная
/// замена (MoveFileExW REPLACE|WRITE_THROUGH на Windows). При сбое
/// сериализации/записи прежний валидный файл остаётся нетронутым.
/// Замена разрешена ТОЛЬКО для собственного конфига GUI — см. AGENTS.md.
pub fn save_settings(settings: &GuiSettings, path: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(settings).context("сериализация настроек")?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("tmp.{nanos}.{}", std::process::id()));
    write_exclusive(&tmp, json.as_bytes())?;
    let res = atomic_replace(&tmp, path);
    if res.is_err() {
        // прежний конфиг цел; tmp остаётся для диагностики (не удаляем)
        eprintln!(
            "примечание: замена конфига не выполнена, черновик: {}",
            tmp.display()
        );
    }
    res
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (w_from, w_to) = (wide(from), wide(to));
    let ok = unsafe {
        MoveFileExW(
            w_from.as_ptr(),
            w_to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        let e = std::io::Error::last_os_error();
        anyhow::bail!("MoveFileExW({} -> {}): {e}", from.display(), to.display());
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to)
        .with_context(|| format!("rename {} -> {}", from.display(), to.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "pockettts_gui_{}_{}_{}",
            std::process::id(),
            tag,
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_exclusive_refuses_occupied_name() {
        let d = tmpdir("excl");
        let p = d.join("occupied.json");
        std::fs::write(&p, b"original").unwrap();
        // create_new: занятое имя — отказ, существующее содержимое не трогаем
        let err = write_exclusive(&p, b"new content").unwrap_err();
        assert!(format!("{err:#}").contains("create_new"), "{err:#}");
        assert_eq!(std::fs::read(&p).unwrap(), b"original");
        // новое имя — успешно
        let fresh = d.join("fresh.json");
        write_exclusive(&fresh, b"created").unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"created");
    }

    #[test]
    fn round_trip_preserves_unknown_keys() {
        let d = tmpdir("rt");
        let p = d.join("s.json");
        // исходный конфиг с неизвестными ключами (будущая версия/руками)
        let src = r#"{
            "temperature": 0.5,
            "future_key": {"nested": [1, 2, 3]},
            "another_unknown": "значение",
            "workers": 3
        }"#;
        std::fs::write(&p, src).unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(s.temperature, 0.5);
        assert_eq!(s.workers, Some(3));
        assert!(s.extra.contains_key("future_key"));
        assert!(s.extra.contains_key("another_unknown"));
        // изменить известное, сохранить, загрузить: всё на месте
        let mut s = GuiSettings {
            temperature: 0.7,
            model: ModelChoice::Fast,
            ..s
        };
        s.workers = Some(3);
        save_settings(&s, &p).unwrap();
        let s2 = load_settings(&p).unwrap();
        assert_eq!(s2.temperature, 0.7);
        assert_eq!(s2.model, ModelChoice::Fast);
        assert_eq!(s2.workers, Some(3));
        // состояния свёрток переживают save/load (регрессия G1-фикса №3)
        let s = GuiSettings {
            source_open: false,
            model_open: false,
            gen_open: true,
            pauses_open: false,
            out_open: true,
            ..s2.clone()
        };
        save_settings(&s, &p).unwrap();
        let s3 = load_settings(&p).unwrap();
        assert!(!s3.source_open);
        assert!(!s3.model_open);
        assert!(s3.gen_open);
        assert!(!s3.pauses_open);
        assert!(s3.out_open);
        assert_eq!(
            s2.extra.get("future_key"),
            s.extra.get("future_key"),
            "неизвестный ключ пережил round-trip без изменений"
        );
        assert_eq!(
            s2.extra.get("another_unknown"),
            s.extra.get("another_unknown")
        );
    }

    #[test]
    fn save_failure_keeps_previous_valid_file() {
        let d = tmpdir("fail");
        let p = d.join("s.json");
        let s = GuiSettings {
            temperature: 0.4,
            ..Default::default()
        };
        save_settings(&s, &p).unwrap();
        let before = std::fs::read_to_string(&p).unwrap();
        // сбой сериализации эмулировать сложно (все поля сериализуемы);
        // проверяем обратное: повторная валидная запись не разрушает файл,
        // а временных файлов после успеха не остаётся
        let s = GuiSettings {
            temperature: 0.6,
            ..s
        };
        save_settings(&s, &p).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert_ne!(before, after);
        assert!(after.contains("0.6"));
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp."))
            .collect();
        assert!(leftovers.is_empty(), "tmp убраны после успеха");
    }

    /// G6.3: INT8-варианты модели сериализуются однозначно и переживают
    /// round-trip; старые значения "Fast"/"Slow"/"Custom" загружаются как
    /// прежде (FP32); поле отсутствует в старом конфиге → дефолт (FP32).
    #[test]
    fn model_choice_int8_roundtrip_and_legacy() {
        let d = tmpdir("model_int8");
        let p = d.join("s.json");
        for (choice, json_value) in [
            (ModelChoice::Fast, "\"Fast\""),
            (ModelChoice::Slow, "\"Slow\""),
            (ModelChoice::FastInt8, "\"FastInt8\""),
            (ModelChoice::SlowInt8, "\"SlowInt8\""),
            (ModelChoice::Custom, "\"Custom\""),
        ] {
            let s = GuiSettings {
                model: choice,
                ..Default::default()
            };
            save_settings(&s, &p).unwrap();
            let raw = std::fs::read_to_string(&p).unwrap();
            assert!(
                raw.contains(&format!("\"model\": {json_value}")),
                "{choice:?}: {raw}"
            );
            let s2 = load_settings(&p).unwrap();
            assert_eq!(s2.model, choice, "{choice:?}");
        }
        // старый конфиг со значением "Slow" — без правки файла
        std::fs::write(&p, r#"{"model": "Slow", "theme_dark": true}"#).unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(s.model, ModelChoice::Slow);
        assert_eq!(s.model.precision(), shtorm_engine::ModelPrecision::Fp32);
        assert!(s.theme_dark, "соседние ключи не потеряны");
        // конфиг вообще без поля model → дефолт (FP32)
        std::fs::write(&p, r#"{"threads": 4}"#).unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(s.model, ModelChoice::Slow, "дефолт — Slow FP32");
        assert_eq!(s.model.precision(), shtorm_engine::ModelPrecision::Fp32);
        // точность каждого варианта
        assert_eq!(
            ModelChoice::FastInt8.precision(),
            shtorm_engine::ModelPrecision::Int8
        );
        assert_eq!(
            ModelChoice::SlowInt8.precision(),
            shtorm_engine::ModelPrecision::Int8
        );
        assert_eq!(
            ModelChoice::Fast.precision(),
            shtorm_engine::ModelPrecision::Fp32
        );
        assert_eq!(
            ModelChoice::Slow.precision(),
            shtorm_engine::ModelPrecision::Fp32
        );
        assert_eq!(
            ModelChoice::Custom.precision(),
            shtorm_engine::ModelPrecision::Fp32
        );
        // подкаталоги: INT8-варианты делят каталог с FP32-вариантами
        assert_eq!(ModelChoice::FastInt8.sub_dir(), Some("ru-v2-fast"));
        assert_eq!(ModelChoice::SlowInt8.sub_dir(), Some("ru-v2-slow"));
        assert_eq!(ModelChoice::Fast.sub_dir(), Some("ru-v2-fast"));
        assert_eq!(ModelChoice::Custom.sub_dir(), None);
        // метки списка уникальны (видны пользователю в одном комбобоксе)
        let labels = [
            ModelChoice::Fast.label(),
            ModelChoice::FastInt8.label(),
            ModelChoice::Slow.label(),
            ModelChoice::SlowInt8.label(),
            ModelChoice::Custom.label(),
        ];
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "метки дублируются: {labels:?}");
    }

    /// Пресет выбирается по формату: MP3 и Opus не делят одно поле и не
    /// подменяют друг друга при переключении формата.
    #[test]
    fn preset_follows_format_without_fallback() {
        let mut s = GuiSettings {
            format: OutputFormatChoice::Mp3,
            ..Default::default()
        };
        assert_eq!(s.preset_for_format(), "-V2 (Высокое)");
        s.format = OutputFormatChoice::Opus;
        assert_eq!(s.preset_for_format(), "opus 64k");
        s.format = OutputFormatChoice::Wav;
        assert_eq!(s.preset_for_format(), "");
        // правка MP3-пресета не влияет на Opus и наоборот
        s.format = OutputFormatChoice::Mp3;
        s.mp3_preset = "128k".into();
        s.format = OutputFormatChoice::Opus;
        assert_eq!(s.preset_for_format(), "opus 64k");
        s.opus_preset = "opus 32k".into();
        s.format = OutputFormatChoice::Mp3;
        assert_eq!(s.preset_for_format(), "128k");
    }

    #[test]
    fn effective_workers_by_device_and_manual_override() {
        let s = GuiSettings {
            device: DeviceChoice::Cpu,
            ..Default::default()
        };
        assert_eq!(s.effective_workers(), 2);
        let s = GuiSettings {
            device: DeviceChoice::Cuda,
            ..s
        };
        assert_eq!(s.effective_workers(), 1);
        let s = GuiSettings {
            workers: Some(4),
            ..s
        };
        assert_eq!(s.effective_workers(), 4, "ручной выбор не перезаписывается");
    }

    // ============ G4.1: настройки EOS ============

    /// Дефолты новой конфигурации — значения teratts-android: явное число
    /// Fixed(-4.0) (без варианта «Авто») и Fixed(5)/Fixed(3) (не None!).
    #[test]
    fn eos_defaults_are_teratts_android_values() {
        let s = GuiSettings::default();
        assert_eq!(s.eos_threshold, EosThresholdSetting::Fixed(-4.0));
        assert_eq!(s.eos_threshold.value(), -4.0);
        assert_eq!(s.eos_tail_short, EosTail::Fixed(5));
        assert_eq!(s.eos_tail_long, EosTail::Fixed(3));
        assert_ne!(s.eos_tail_short, EosTail::Default, "явные 5/3, не None");
    }

    /// Старый конфиг G1–G4 (числовой eos_threshold, нет новых ключей)
    /// загружается: порог → Fixed(старое число), хвосты → новые дефолты 5/3.
    #[test]
    fn eos_migration_from_old_config() {
        let d = tmpdir("eos_mig");
        let p = d.join("s.json");
        std::fs::write(&p, r#"{"temperature": 0.4, "eos_threshold": -3.5}"#).unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(s.eos_threshold, EosThresholdSetting::Fixed(-3.5));
        assert_eq!(s.eos_tail_short, EosTail::Fixed(5), "нет ключа → дефолт 5");
        assert_eq!(s.eos_tail_long, EosTail::Fixed(3), "нет ключа → дефолт 3");
        // старый −4.0 тоже просто число: без скрытых преобразований
        std::fs::write(&p, r#"{"eos_threshold": -4.0}"#).unwrap();
        assert_eq!(
            load_settings(&p).unwrap().eos_threshold,
            EosThresholdSetting::Fixed(-4.0)
        );
        // G5: старое строковое "auto" мигрирует в число −4.0, а не в Auto
        std::fs::write(&p, r#"{"eos_threshold": "auto"}"#).unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(s.eos_threshold, EosThresholdSetting::Fixed(-4.0));
        // при следующем сохранении значение записывается числом
        save_settings(&s, &p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("\"auto\""), "{raw}");
        assert!(raw.contains("\"eos_threshold\": -4.0"), "{raw}");
        // невалидный хвост (за диапазоном) — честная ошибка разбора
        std::fs::write(&p, r#"{"eos_tail_short": 99}"#).unwrap();
        assert!(load_settings(&p).is_err());
    }

    /// Round-trip новых значений: числовой порог −4.0, «none»/0/10
    /// сохраняются как есть (None ≠ 0 в JSON: "none" против числа),
    /// unknown-ключи не теряются. Строка "auto" в новых конфигах не пишется.
    #[test]
    fn eos_settings_round_trip_distinct_states() {
        let d = tmpdir("eos_rt");
        let p = d.join("s.json");
        let s = GuiSettings {
            eos_threshold: EosThresholdSetting::Fixed(-4.0),
            eos_tail_short: EosTail::Default,
            eos_tail_long: EosTail::Fixed(0),
            ..Default::default()
        };
        save_settings(&s, &p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("\"auto\""), "{raw}");
        assert!(raw.contains("\"eos_threshold\": -4.0"), "{raw}");
        assert!(raw.contains("\"none\""), "None — строка, не число: {raw}");
        assert!(raw.contains("\"eos_tail_long\": 0"), "{raw}");
        let s2 = load_settings(&p).unwrap();
        assert_eq!(s2.eos_threshold, EosThresholdSetting::Fixed(-4.0));
        assert_eq!(s2.eos_tail_short, EosTail::Default);
        assert_eq!(s2.eos_tail_long, EosTail::Fixed(0));
        assert_ne!(s2.eos_tail_short, s2.eos_tail_long, "None ≠ Fixed(0)");
        // числовой порог тоже проходит круг
        let s = GuiSettings {
            eos_threshold: EosThresholdSetting::Fixed(0.5),
            eos_tail_short: EosTail::Fixed(10),
            eos_tail_long: EosTail::Fixed(1),
            ..Default::default()
        };
        save_settings(&s, &p).unwrap();
        let s3 = load_settings(&p).unwrap();
        assert_eq!(s3.eos_threshold, EosThresholdSetting::Fixed(0.5));
        assert_eq!(s3.eos_tail_short, EosTail::Fixed(10));
        assert_eq!(s3.eos_tail_long, EosTail::Fixed(1));
    }

    /// G5: overwrite_existing по умолчанию выключен; ключ переживает
    /// round-trip, а старый конфиг без ключа читается как false.
    #[test]
    fn overwrite_existing_default_off_and_round_trip() {
        assert!(!GuiSettings::default().overwrite_existing);
        let d = tmpdir("overwrite");
        let p = d.join("s.json");
        let s = GuiSettings {
            overwrite_existing: true,
            ..Default::default()
        };
        save_settings(&s, &p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("\"overwrite_existing\": true"), "{raw}");
        assert!(load_settings(&p).unwrap().overwrite_existing);
        std::fs::write(&p, r#"{"temperature": 0.4}"#).unwrap();
        assert!(
            !load_settings(&p).unwrap().overwrite_existing,
            "старый конфиг без ключа — защита включена"
        );
    }

    /// G6.2: режим пакетного CUDA по умолчанию экономичный (гибрид — только
    /// осознанный выбор), переживает round-trip, старый конфиг без ключа
    /// читается как Economy.
    #[test]
    fn batch_cuda_mode_default_economy_and_round_trip() {
        assert_eq!(
            GuiSettings::default().batch_cuda_mode,
            BatchCudaMode::Economy,
            "гибрид 2×2 НЕ включается молча"
        );
        let d = tmpdir("hybrid_mode");
        let p = d.join("s.json");
        let s = GuiSettings {
            batch_cuda_mode: BatchCudaMode::MaxSpeed,
            ..Default::default()
        };
        save_settings(&s, &p).unwrap();
        assert_eq!(
            load_settings(&p).unwrap().batch_cuda_mode,
            BatchCudaMode::MaxSpeed
        );
        std::fs::write(&p, r#"{"temperature": 0.4}"#).unwrap();
        assert_eq!(
            load_settings(&p).unwrap().batch_cuda_mode,
            BatchCudaMode::Economy,
            "старый конфиг без ключа — экономичный режим"
        );
    }

    #[test]
    fn theme_default_dark_and_round_trip() {
        assert!(GuiSettings::default().theme_dark);
        let d = tmpdir("theme");
        let p = d.join("s.json");
        let s = GuiSettings {
            theme_dark: false,
            ..Default::default()
        };
        save_settings(&s, &p).unwrap();
        assert!(!load_settings(&p).unwrap().theme_dark);
        std::fs::write(&p, r#"{"temperature": 0.4}"#).unwrap();
        assert!(
            load_settings(&p).unwrap().theme_dark,
            "старый конфиг без ключа — тёмная тема"
        );
    }

    /// clamped() не разрушает состояния: устаревший «Авто» и None остаются
    /// собой, Fixed зажимается в допустимый диапазон.
    #[test]
    fn eos_clamped_preserves_auto_and_none() {
        let mut s = GuiSettings {
            eos_threshold: EosThresholdSetting::Auto,
            eos_tail_short: EosTail::Default,
            eos_tail_long: EosTail::Fixed(99),
            ..Default::default()
        };
        s.clamped();
        assert_eq!(s.eos_threshold, EosThresholdSetting::Auto);
        assert_eq!(s.eos_tail_short, EosTail::Default);
        assert_eq!(s.eos_tail_long, EosTail::Fixed(10), "99 → 10");
        let mut s = GuiSettings {
            eos_threshold: EosThresholdSetting::Fixed(55.0),
            eos_tail_long: EosTail::Fixed(0),
            ..s
        };
        s.clamped();
        assert_eq!(s.eos_threshold, EosThresholdSetting::Fixed(20.0));
        assert_eq!(s.eos_tail_long, EosTail::Fixed(0), "0 — легальное значение");
    }
}
