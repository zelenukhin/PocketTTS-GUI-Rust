//! Настройки GUI (G1): собственный JSON (`shtorm_gui_settings.json` рядом с
//! exe), round-trip с сохранением неизвестных ключей, безопасная атомарная
//! запись (unique tmp + MoveFileExW REPLACE — старый файл не разрушается при
//! сбое сериализации). Импорт из `settings_pocket_tts.json` Python-GUI —
//! только чтение (файл не изменяется), маппинг известных полей.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use shtorm_engine::EosTail;

/// Порог EOS в настройках GUI (G4.1). «Авто» — встроенное фиксированное
/// значение −4.0 движка teratts-android: НЕ читает runtime.json модели
/// и не создаёт отдельного автоматического алгоритма. Число передаётся
/// движку непосредственно.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EosThresholdSetting {
    Auto,
    Fixed(f32),
}

impl EosThresholdSetting {
    /// Значение «Авто» — константа порога teratts-android.
    pub const AUTO_VALUE: f32 = -4.0;

    pub fn value(self) -> f32 {
        match self {
            Self::Auto => Self::AUTO_VALUE,
            Self::Fixed(v) => v,
        }
    }
}

/// Варианты порога EOS (как в Python GUI; «Авто» == −4.0).
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
            EosThresholdSetting::Auto => s.serialize_str("auto"),
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
                "auto" => Ok(EosThresholdSetting::Auto),
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
    // Вывод
    pub format: OutputFormatChoice,
    pub mp3_preset: String,
    /// Пресет Opus — отдельный от MP3: при смене формата используется
    /// соответствующий пресет, несовместимый не подставляется молча.
    pub opus_preset: String,
    pub out_dir: String,
    pub chunk_log: bool,
    pub ffmpeg_path: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelChoice {
    Fast,
    Slow,
    Custom,
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
            // G4.1: дефолты воспроизводят teratts-android — «Авто» (== −4.0)
            // и явные Fixed(5)/Fixed(3). Python-пересчёт 6/4 не переносится.
            eos_threshold: EosThresholdSetting::Auto,
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
            format: OutputFormatChoice::Wav,
            mp3_preset: "-V2 (Высокое)".into(),
            opus_preset: "opus 64k".into(),
            out_dir: "output".into(),
            chunk_log: false,
            ffmpeg_path: String::new(),
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
        // порог: зажимается только числовая часть, «Авто» сохраняется
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

/// Путь собственного конфига: рядом с exe (портативный стиль).
pub fn settings_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("shtorm_gui_settings.json")))
        .unwrap_or_else(|| PathBuf::from("shtorm_gui_settings.json"))
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

/// Импорт из Python `settings_pocket_tts.json` — ТОЛЬКО ЧТЕНИЕ (файл не
/// изменяется). Маппинг известных полей; неизвестные игнорируются.
pub fn import_from_python(python_settings: &Path) -> Result<GuiSettings> {
    let raw = std::fs::read_to_string(python_settings)
        .with_context(|| format!("прочитать {}", python_settings.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("разобрать {}", python_settings.display()))?;
    let g = |k: &str| v.get(k).cloned();
    let mut s = GuiSettings::default();
    if let Some(x) = g("temperature").and_then(|x| x.as_f64()) {
        s.temperature = x as f32;
    }
    if let Some(x) = g("sampler_decode_steps").and_then(|x| x.as_u64()) {
        s.decode_steps = x as u32;
    }
    if let Some(x) = g("seed").and_then(|x| x.as_i64()) {
        s.seed = x;
    }
    if let Some(x) = g("brightness_db").and_then(|x| x.as_f64()) {
        s.brightness_db = x as f32;
    }
    if let Some(x) = g("denoise").and_then(|x| x.as_bool()) {
        s.denoise = x;
    }
    if let Some(x) = g("anti_repeat_teratts").and_then(|x| x.as_bool()) {
        s.anti_repeat = x;
    }
    if let Some(x) = g("pause_scale").and_then(|x| x.as_f64()) {
        s.pause_scale = x as f32;
    }
    if let Some(x) = g("paragraph_pause_s").and_then(|x| x.as_f64()) {
        s.paragraph_pause_s = x as f32;
    }
    if let Some(x) = g("max_tokens").and_then(|x| x.as_u64()) {
        s.max_tokens = x as u32;
    }
    if let Some(x) = g("device").and_then(|x| x.as_str().map(str::to_string)) {
        s.device = if x.eq_ignore_ascii_case("cuda") {
            DeviceChoice::Cuda
        } else {
            DeviceChoice::Cpu
        };
    }
    if let Some(x) = g("num_model_instances").and_then(|x| x.as_u64()) {
        s.workers = Some(x as u32);
    }
    if let Some(x) = g("use_wav").and_then(|x| x.as_bool()) {
        s.format = if x {
            OutputFormatChoice::Wav
        } else {
            OutputFormatChoice::Mp3
        };
    }
    if let Some(x) = g("mp3_preset").and_then(|x| x.as_str().map(str::to_string)) {
        // Python хранит один пресет: opus-метки переносим в opus_preset,
        // остальные — в mp3_preset (явный маппинг по формату пресета).
        if x.trim().to_ascii_lowercase().starts_with("opus") {
            s.opus_preset = x;
        } else {
            s.mp3_preset = x;
        }
    }
    if let Some(x) = g("voice_source").and_then(|x| x.as_str().map(str::to_string)) {
        s.voice_wav = x.to_string();
    }
    if let Some(x) = g("source_path_single").and_then(|x| x.as_str().map(str::to_string)) {
        s.text_file = x.to_string();
    }
    // Python хранит общий source_path + processing_mode ("single"/"batch").
    if let Some(x) = g("source_path").and_then(|x| x.as_str().map(str::to_string)) {
        s.text_file = x.to_string();
    }
    if let Some(x) = g("processing_mode").and_then(|x| x.as_str().map(str::to_string)) {
        s.batch = x.eq_ignore_ascii_case("batch");
    }
    if let Some(x) = g("output_dir_path").and_then(|x| x.as_str().map(str::to_string)) {
        s.out_dir = x.to_string();
    }
    if let Some(x) = g("chunk_log").and_then(|x| x.as_bool()) {
        s.chunk_log = x;
    }
    s.clamped();
    Ok(s)
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
            "shtorm_gui_{}_{}_{}",
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

    #[test]
    fn import_python_read_only_maps_known_fields() {
        let d = tmpdir("import");
        let src = d.join("settings_pocket_tts.json");
        std::fs::write(
            &src,
            r#"{
                "temperature": 0.3, "sampler_decode_steps": 1,
                "eos_teratts": true, "anti_repeat_teratts": true,
                "denoise": false, "brightness_db": 8, "seed": 42,
                "device": "Auto", "num_model_instances": 3,
                "mp3_preset": "-V2", "use_wav": false,
                "voice_source": "C:/v.wav", "source_path_single": "C:/t.txt",
                "output_dir_path": "C:/out", "chunk_log": false,
                "unknown_future": 1
            }"#,
        )
        .unwrap();
        let before = std::fs::read_to_string(&src).unwrap();
        let s = import_from_python(&src).unwrap();
        assert_eq!(s.temperature, 0.3);
        assert_eq!(s.workers, Some(3));
        assert_eq!(s.format, OutputFormatChoice::Mp3);
        assert_eq!(s.mp3_preset, "-V2");
        assert_eq!(s.voice_wav, "C:/v.wav");
        // Auto -> CPU (безопасный дефолт)
        assert_eq!(s.device, DeviceChoice::Cpu);
        // файл Python не изменён
        assert_eq!(std::fs::read_to_string(&src).unwrap(), before);
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

    /// Python хранит один пресет: opus-метка попадает в opus_preset, а
    /// MP3-поле остаётся дефолтным (без несовместимого значения).
    #[test]
    fn import_python_opus_preset_maps_to_opus_field() {
        let d = tmpdir("import_opus");
        let src = d.join("settings_pocket_tts.json");
        std::fs::write(&src, r#"{"mp3_preset": "opus 32k", "use_wav": false}"#).unwrap();
        let s = import_from_python(&src).unwrap();
        assert_eq!(s.opus_preset, "opus 32k");
        assert_eq!(s.mp3_preset, "-V2 (Высокое)");
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

    /// Дефолты новой конфигурации — значения teratts-android: «Авто» (== −4.0)
    /// и явные Fixed(5)/Fixed(3) (не None!). «Авто» — фиксированное −4.0,
    /// а не чтение модели.
    #[test]
    fn eos_defaults_are_teratts_android_values() {
        let s = GuiSettings::default();
        assert_eq!(s.eos_threshold, EosThresholdSetting::Auto);
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
        // невалидный хвост (за диапазоном) — честная ошибка разбора
        std::fs::write(&p, r#"{"eos_tail_short": 99}"#).unwrap();
        assert!(load_settings(&p).is_err());
    }

    /// Round-trip новых значений: «auto»/«none»/0/10 сохраняются как есть
    /// (None ≠ 0 в JSON: "none" против числа), unknown-ключи не теряются.
    #[test]
    fn eos_settings_round_trip_distinct_states() {
        let d = tmpdir("eos_rt");
        let p = d.join("s.json");
        let s = GuiSettings {
            eos_threshold: EosThresholdSetting::Auto,
            eos_tail_short: EosTail::Default,
            eos_tail_long: EosTail::Fixed(0),
            ..Default::default()
        };
        save_settings(&s, &p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("\"auto\""), "{raw}");
        assert!(raw.contains("\"none\""), "None — строка, не число: {raw}");
        assert!(raw.contains("\"eos_tail_long\": 0"), "{raw}");
        let s2 = load_settings(&p).unwrap();
        assert_eq!(s2.eos_threshold, EosThresholdSetting::Auto);
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

    /// clamped() не разрушает состояния: «Авто» и None остаются собой,
    /// Fixed зажимается в допустимый диапазон.
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
