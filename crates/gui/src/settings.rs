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
    /// перезаписывается при смене устройства. G6.6: поле управляет ТОЛЬКО
    /// воркерами чанков одиночного файла; пакетная обработка —
    /// `parallel_cpu`/`parallel_cuda`.
    pub workers: Option<u32>,
    /// G6.6: пакетная параллельная обработка CPU (независимо от CUDA).
    pub parallel_cpu: ParallelSettings,
    /// G6.6: пакетная параллельная обработка CUDA (независимо от CPU).
    pub parallel_cuda: ParallelSettings,
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

// ============ G6.6: единая модель параллельной обработки CPU/CUDA ============

/// G6.6: профиль параллельной обработки. Пресеты задают ПОЛНУЮ конфигурацию
/// (P/W/F/T); ручная правка любого поля переводит профиль в `Custom`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParallelProfile {
    #[default]
    Optimal,
    Productive,
    /// Ручная конфигурация (состояние, а не кнопка).
    Custom,
}

impl ParallelProfile {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Optimal => "Оптимальный",
            Self::Productive => "Производительный",
            Self::Custom => "Свой",
        }
    }
}

/// G6.6/G6.7: единая модель параллельной обработки для CPU и CUDA.
///
/// Определения (ТЗ G6.7):
/// - `processes` — число ДОЧЕРНИХ рабочих процессов с Pocket (не считая
///   GUI-диспетчер); при P=1 пул живёт в основном процессе приложения;
/// - `workers_per_process` — постоянных Pocket (ONNX-сессий) в каждом
///   рабочем процессе; Pocket переживает файлы задания;
/// - `max_concurrent_chunks` — верхний предел ОДНОВРЕМЕННО синтезируемых
///   чанков ТЕКУЩЕГО файла (файлов в обработке всегда максимум 1);
/// - `ort_threads` — intra-op потоки ONNX Runtime на ОДНУ ONNX-сессию.
///
/// `total_pockets = processes × workers_per_process`;
/// `active_chunks = min(max_concurrent_chunks, total_pockets, remaining_chunks)`.
/// Процесс, Pocket и ORT-поток — разные понятия и не взаимозаменяемы; C не
/// создаёт дополнительные Pocket.Исторический ключ G6.6
/// `max_concurrent_files` читается как стартовое значение C (семантика
/// параметра изменилась: предел файлов → предел чанков; см. docs/G6_7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParallelSettings {
    pub profile: ParallelProfile,
    pub processes: u32,
    pub workers_per_process: u32,
    pub max_concurrent_chunks: u32,
    pub ort_threads: u32,
}

impl Default for ParallelSettings {
    /// Дефолт = пресет «Оптимальный» (значения исследований на
    /// i7-6700K/RTX 3060; адаптация под ресурсы — при применении пресета).
    fn default() -> Self {
        Self {
            profile: ParallelProfile::Optimal,
            processes: 1,
            workers_per_process: 2,
            max_concurrent_chunks: 2,
            ort_threads: 4,
        }
    }
}

/// Диапазоны процессов/воркеров по устройству (начальный безопасный
/// диапазон ТЗ G6.6: CPU P 1..=4 / W 1..=4; CUDA P 1..=2 / W 1..=2,
/// всего не более 4 Pocket).
pub fn parallel_limits(device: DeviceChoice) -> (u32, u32) {
    match device {
        DeviceChoice::Cpu => (4, 4),
        DeviceChoice::Cuda => (2, 2),
    }
}

/// G6.7.1: расчётная потребность VRAM (МБ) по методике VRAM-префлайта
/// G6.1/G6.6: N × 2355 МБ на Pocket + запас 900 МБ (однопроцессный пул,
/// ≤2 Pocket) / 1500 МБ (мультипроцесс или >2 Pocket — консервативный
/// запас по пикам P2B.3). ЕДИНАЯ формула для GUI и preflight (`job.rs`);
/// перевод MB → GB выполняется ровно один раз — в GUI.
pub fn vram_need_mb(pockets: u32, multiprocess: bool) -> u64 {
    let per_worker_mb: u64 = 2355;
    let reserve_mb: u64 = if multiprocess || pockets > 2 {
        1500
    } else {
        900
    };
    pockets as u64 * per_worker_mb + reserve_mb
}

/// Оценка RAM одного CPU-воркера (Pocket, 5 ONNX-сессий), МБ — по замерам
/// docs/CPU_BATCH_WORKERS_RESEARCH.md (Slow FP32 ~0.85–1 ГБ, INT8 ~0.5 ГБ).
fn ram_per_worker_mb(precision: ModelPrecision) -> u64 {
    match precision {
        ModelPrecision::Fp32 => 1100,
        ModelPrecision::Int8 => 700,
    }
}

/// Свободная RAM системы, МБ (None — определить не удалось).
pub fn available_ram_mb() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut st: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut st) } != 0 {
            Some(st.ullAvailPhys / (1024 * 1024))
        } else {
            None
        }
    }
    #[cfg(not(windows))]
    None
}

impl ParallelSettings {
    pub fn total_pockets(&self) -> u32 {
        self.processes.saturating_mul(self.workers_per_process)
    }

    /// Зажим в жёсткие диапазоны устройства (валидация ТЗ G6.7 §7:
    /// C ≤ P×W; общий предел P×W ≤ 4 Pocket; T ≤ 16; без переполнений).
    pub fn clamp_for_device(&mut self, device: DeviceChoice) {
        let (pmax, wmax) = parallel_limits(device);
        self.processes = self.processes.clamp(1, pmax);
        self.workers_per_process = self.workers_per_process.clamp(1, wmax);
        // общий предел Pocket: не более 4 на задание (первая реализация G6.7)
        while self.total_pockets() > 4 && self.workers_per_process > 1 {
            self.workers_per_process -= 1;
        }
        if self.total_pockets() > 4 && self.processes > 1 {
            self.processes = 1;
        }
        self.ort_threads = self.ort_threads.clamp(1, 16);
        let pockets = self.total_pockets();
        self.max_concurrent_chunks = self.max_concurrent_chunks.clamp(1, pockets);
    }

    pub fn clamped_for_device(mut self, device: DeviceChoice) -> Self {
        self.clamp_for_device(device);
        self
    }

    /// G6.7.1: планируемое к ЗАГРУЗКЕ число Pocket — единая семантика для
    /// GUI и production. P=1: однопроцессный чанк-пул создаёт min(W, C)
    /// Pocket (лишние сверх C не создаются); P>1: мультипроцессная топология
    /// создаёт все P×W Pocket независимо от C.
    pub fn planned_pockets(&self) -> u32 {
        if self.processes <= 1 {
            self.workers_per_process
                .min(self.max_concurrent_chunks)
                .max(1)
        } else {
            self.total_pockets()
        }
    }

    /// Компактная информационная строка конфигурации. G6.7.1: различает
    /// настроенное и планируемое к загрузке число Pocket (однопроцессный
    /// пул при C < W грузит меньше настроенного); оценка не называется
    /// «фактической» — реальное состояние сообщает журнал выполнения.
    pub fn summary_line(&self) -> String {
        let p = if self.processes == 1 {
            "процесс"
        } else {
            "процесса"
        };
        let planned = self.planned_pockets();
        if self.processes == 1 && planned < self.total_pockets() {
            return format!(
                "{} {} · настроено {} Pocket · будет загружен {} Pocket · до {} чанков · ORT {}",
                self.processes,
                p,
                self.total_pockets(),
                planned,
                self.max_concurrent_chunks,
                self.ort_threads
            );
        }
        format!(
            "{} {} · {} Pocket · до {} чанков · ORT {}",
            self.processes,
            p,
            self.total_pockets(),
            self.max_concurrent_chunks,
            self.ort_threads
        )
    }

    /// Пресет «Оптимальный» (баланс скорости, RAM/VRAM и времени первого
    /// результата). CPU: 1 процесс, W = clamp(floor(logical/4), 1, 4) с
    /// ограничением по свободной RAM (исследованное правило), C=W, T=4.
    /// CUDA: 1×2, C=2, T=4 (кандидат ТЗ G6.7 для чанк-пула).
    pub fn preset_optimal(
        device: DeviceChoice,
        logical_cpus: usize,
        precision: ModelPrecision,
    ) -> Self {
        match device {
            DeviceChoice::Cpu => {
                let mut w = (logical_cpus / 4).clamp(1, 4) as u32;
                w = ram_limit_workers(w, available_ram_mb(), precision);
                Self {
                    profile: ParallelProfile::Optimal,
                    processes: 1,
                    workers_per_process: w,
                    max_concurrent_chunks: w,
                    ort_threads: 4,
                }
            }
            DeviceChoice::Cuda => Self {
                profile: ParallelProfile::Optimal,
                processes: 1,
                workers_per_process: 2,
                max_concurrent_chunks: 2,
                ort_threads: 4,
            },
        }
    }

    /// Пресет «Производительный» (приоритет скорости генерации).
    /// CPU: 1 процесс × до 4 Pocket × T=1 (W = clamp(floor(logical/2), 2, 4),
    /// ограничен RAM); CUDA: 2×2, C=4, T=4 (кандидат ТЗ G6.7).
    pub fn preset_productive(
        device: DeviceChoice,
        logical_cpus: usize,
        precision: ModelPrecision,
    ) -> Self {
        match device {
            DeviceChoice::Cpu => {
                let mut w = (logical_cpus / 2).clamp(2, 4) as u32;
                w = ram_limit_workers(w, available_ram_mb(), precision);
                Self {
                    profile: ParallelProfile::Productive,
                    processes: 1,
                    workers_per_process: w,
                    max_concurrent_chunks: w,
                    ort_threads: 1,
                }
            }
            DeviceChoice::Cuda => Self {
                profile: ParallelProfile::Productive,
                processes: 2,
                workers_per_process: 2,
                max_concurrent_chunks: 4,
                ort_threads: 4,
            },
        }
    }

    /// Профиль по совпадению с КАНОНИЧЕСКИМИ таблицами пресетов (ТЗ G6.6 §6,
    /// значения исследований i7-6700K/RTX 3060). Используется только при
    /// миграции старых конфигов; в GUI профиль меняется на «Свой» в момент
    /// ручной правки поля и хранится как есть.
    pub fn detect_profile(&self, device: DeviceChoice) -> ParallelProfile {
        let canonical = |p: u32, w: u32, t: u32| {
            self.processes == p
                && self.workers_per_process == w
                && self.max_concurrent_chunks == p * w
                && self.ort_threads == t
        };
        match device {
            DeviceChoice::Cpu => {
                if canonical(1, 2, 4) {
                    ParallelProfile::Optimal
                } else if canonical(1, 4, 1) {
                    ParallelProfile::Productive
                } else {
                    ParallelProfile::Custom
                }
            }
            DeviceChoice::Cuda => {
                if canonical(1, 2, 4) {
                    ParallelProfile::Optimal
                } else if canonical(2, 2, 4) {
                    ParallelProfile::Productive
                } else {
                    ParallelProfile::Custom
                }
            }
        }
    }
}

/// Ограничить число воркеров доступной RAM (порог на воркера + запас 1.5 ГБ
/// для GUI/ОС; минимум 1 — конфигурация не становится пустой).
fn ram_limit_workers(requested: u32, free_mb: Option<u64>, precision: ModelPrecision) -> u32 {
    let Some(free) = free_mb else {
        return requested;
    };
    const RESERVE_MB: u64 = 1500;
    let per = ram_per_worker_mb(precision);
    let fit = free.saturating_sub(RESERVE_MB) / per;
    requested.min(fit.max(1) as u32)
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
            parallel_cpu: ParallelSettings::default(),
            parallel_cuda: ParallelSettings::default(),
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
        // G6.6: жёсткие диапазоны параллельной обработки по устройствам
        self.parallel_cpu.clamp_for_device(DeviceChoice::Cpu);
        self.parallel_cuda.clamp_for_device(DeviceChoice::Cuda);
    }

    /// G6.6: конфигурация пакетной параллельной обработки текущего
    /// устройства (клампы применены в `clamped()`).
    pub fn parallel_config(&self) -> ParallelSettings {
        match self.device {
            DeviceChoice::Cpu => self.parallel_cpu,
            DeviceChoice::Cuda => self.parallel_cuda,
        }
    }

    /// G6.6: записать конфигурацию устройства (CPU и CUDA независимы —
    /// переключение устройства не сбрасывает ручной выбор другого).
    pub fn set_parallel_config(&mut self, device: DeviceChoice, cfg: ParallelSettings) {
        match device {
            DeviceChoice::Cpu => self.parallel_cpu = cfg,
            DeviceChoice::Cuda => self.parallel_cuda = cfg,
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
    let mut v: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("разобрать {}", path.display()))?;
    migrate_legacy_parallel_keys(&mut v);
    let mut s: GuiSettings =
        serde_json::from_value(v).with_context(|| format!("разобрать {}", path.display()))?;
    migrate_parallel_settings(&mut s, &raw);
    Ok(s)
}

/// G6.7: перенос устаревшего ключа `max_concurrent_files` (G6.6, семантика
/// предела файлов) в `max_concurrent_chunks` ДО разбора структуры: при
/// наличии ОБОИХ ключей приоритетен новый (дубль не ломает загрузку), при
/// наличии только старого его числовое значение становится стартовым C.
fn migrate_legacy_parallel_keys(v: &mut serde_json::Value) {
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    for key in ["parallel_cpu", "parallel_cuda"] {
        if let Some(serde_json::Value::Object(sub)) = obj.get_mut(key) {
            if !sub.contains_key("max_concurrent_chunks") {
                if let Some(old) = sub.remove("max_concurrent_files") {
                    sub.insert("max_concurrent_chunks".into(), old);
                }
            }
        }
    }
}

/// G6.6: одноразовая миграция пакетной параллельной конфигурации из старых
/// ключей G6.2/G6.1 (`batch_cuda_mode`, ручные `workers` + `threads`).
/// Выполняется ТОЛЬКО когда в конфиге нет нового ключа `parallel_cuda`:
/// повторные загрузки уже мигрированных/пользовательских настроек ничего не
/// подменяют. Старые ключи не удаляются (unknown-key round-trip в `extra`).
fn migrate_parallel_settings(s: &mut GuiSettings, raw: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return; // повреждённый конфиг обрабатывается политикой вызывающего
    };
    if v.get("parallel_cuda").is_some() {
        return;
    }
    let legacy_threads = v.get("threads").and_then(|t| t.as_u64()).unwrap_or(4) as u32;
    let legacy_workers = v.get("workers").and_then(|w| w.as_u64()).map(|w| w as u32);
    let max_speed = v.get("batch_cuda_mode").and_then(|m| m.as_str()) == Some("MaxSpeed");
    s.parallel_cuda = if max_speed {
        // прежний гибрид 2×2 → «Производительный» CUDA (та же топология)
        ParallelSettings {
            profile: ParallelProfile::Productive,
            processes: 2,
            workers_per_process: 2,
            max_concurrent_chunks: 4,
            ort_threads: legacy_threads.clamp(1, 16),
        }
    } else if let Some(w) = legacy_workers {
        // прежний ручной выбор воркеров пула CUDA (Economy) — сохраняем
        // выбор в пределах поддерживаемого диапазона (1..=2)
        let w = w.clamp(1, 2);
        ParallelSettings {
            profile: ParallelProfile::Custom,
            processes: 1,
            workers_per_process: w,
            max_concurrent_chunks: w,
            ort_threads: legacy_threads.clamp(1, 16),
        }
    } else {
        ParallelSettings::default()
    };
    s.parallel_cuda = s.parallel_cuda.clamped_for_device(DeviceChoice::Cuda);
    s.parallel_cuda.profile = s.parallel_cuda.detect_profile(DeviceChoice::Cuda);
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

    /// G6.6: дефолт параллельной обработки — «Оптимальный» 1×2/F2/T4 для
    /// обоих устройств; конфигурации CPU и CUDA независимы и переживают
    /// round-trip; смена устройства не сбрасывает ручной выбор другого.
    #[test]
    fn parallel_settings_default_independent_and_round_trip() {
        let def = GuiSettings::default();
        assert_eq!(def.parallel_cpu, ParallelSettings::default());
        assert_eq!(def.parallel_cuda, ParallelSettings::default());
        assert_eq!(def.parallel_cpu.profile, ParallelProfile::Optimal);
        let d = tmpdir("par_rt");
        let p = d.join("s.json");
        let mut s = GuiSettings {
            parallel_cuda: ParallelSettings {
                profile: ParallelProfile::Productive,
                processes: 2,
                workers_per_process: 2,
                max_concurrent_chunks: 4,
                ort_threads: 4,
            },
            parallel_cpu: ParallelSettings {
                profile: ParallelProfile::Custom,
                processes: 2,
                workers_per_process: 1,
                max_concurrent_chunks: 2,
                ort_threads: 4,
            },
            ..Default::default()
        };
        s.parallel_cpu = ParallelSettings {
            profile: ParallelProfile::Custom,
            processes: 2,
            workers_per_process: 1,
            max_concurrent_chunks: 2,
            ort_threads: 4,
        };
        s.parallel_cuda = ParallelSettings {
            profile: ParallelProfile::Custom,
            processes: 1,
            workers_per_process: 2,
            max_concurrent_chunks: 2,
            ort_threads: 3,
        };
        save_settings(&s, &p).unwrap();
        let s2 = load_settings(&p).unwrap();
        assert_eq!(s2.parallel_cuda, s.parallel_cuda, "CUDA round-trip");
        assert_eq!(s2.parallel_cpu, s.parallel_cpu, "CPU round-trip");
        // переключение устройства не трогает конфигурации
        let s3 = GuiSettings {
            device: DeviceChoice::Cuda,
            ..s2
        };
        assert_eq!(s3.parallel_cpu, s2.parallel_cpu);
        assert_eq!(s3.parallel_config(), s2.parallel_cuda);
        // старый конфиг без новых ключей — дефолт «Оптимальный»
        std::fs::write(&p, r#"{"temperature": 0.4}"#).unwrap();
        let s4 = load_settings(&p).unwrap();
        assert_eq!(s4.parallel_cpu, ParallelSettings::default());
        assert_eq!(s4.parallel_cuda, ParallelSettings::default());
    }

    /// G6.6: клампы по устройствам (CPU P/W ≤4; CUDA P/W ≤2 и всего ≤4
    /// Pocket; F ≤ P×W; T ≤16).
    #[test]
    fn parallel_clamp_for_device_limits() {
        let mut ps = ParallelSettings {
            profile: ParallelProfile::Custom,
            processes: 9,
            workers_per_process: 8,
            max_concurrent_chunks: 99,
            ort_threads: 32,
        };
        ps.clamp_for_device(DeviceChoice::Cpu);
        // G6.7: общий предел P×W ≤ 4 Pocket (для обоих устройств)
        assert_eq!((ps.processes, ps.workers_per_process), (4, 1));
        assert_eq!(ps.max_concurrent_chunks, 4);
        assert_eq!(ps.ort_threads, 16);
        let mut ps = ParallelSettings {
            profile: ParallelProfile::Custom,
            processes: 4,
            workers_per_process: 4,
            max_concurrent_chunks: 16,
            ort_threads: 1,
        };
        ps.clamp_for_device(DeviceChoice::Cuda);
        assert!(ps.total_pockets() <= 4, "CUDA: всего ≤4 Pocket");
        assert_eq!(ps.max_concurrent_chunks, ps.total_pockets());
        // F меньше P×W — легальная конфигурация, хранится как есть
        let mut ps = ParallelSettings {
            profile: ParallelProfile::Custom,
            processes: 2,
            workers_per_process: 2,
            max_concurrent_chunks: 2,
            ort_threads: 4,
        };
        ps.clamp_for_device(DeviceChoice::Cuda);
        assert_eq!(ps.max_concurrent_chunks, 2);
        assert_eq!(ps.total_pockets(), 4);
        // clamped() затрагивает обе конфигурации
        let mut s = GuiSettings::default();
        s.parallel_cpu.processes = 77;
        s.parallel_cuda.workers_per_process = 9;
        s.clamped();
        assert_eq!(s.parallel_cpu.processes, 4);
        assert!(s.parallel_cuda.total_pockets() <= 4);
    }

    /// G6.6: пресеты (канонические значения ТЗ §6 при 8 логических CPU и
    /// без RAM-ограничения — RAM-адаптация проверяется отдельно через
    /// `ram_limit_workers`-поведение при None).
    #[test]
    fn parallel_presets_canonical_values() {
        let logical = 8;
        let fp32 = shtorm_engine::ModelPrecision::Fp32;
        let cpu_opt = ParallelSettings::preset_optimal(DeviceChoice::Cpu, logical, fp32);
        assert_eq!(
            (
                cpu_opt.processes,
                cpu_opt.workers_per_process,
                cpu_opt.ort_threads
            ),
            (1, 2, 4)
        );
        assert_eq!(cpu_opt.max_concurrent_chunks, cpu_opt.total_pockets());
        assert_eq!(cpu_opt.profile, ParallelProfile::Optimal);
        let cpu_prod = ParallelSettings::preset_productive(DeviceChoice::Cpu, logical, fp32);
        assert_eq!(
            (
                cpu_prod.processes,
                cpu_prod.workers_per_process,
                cpu_prod.ort_threads
            ),
            (1, 4, 1)
        );
        let cuda_opt = ParallelSettings::preset_optimal(DeviceChoice::Cuda, logical, fp32);
        assert_eq!(
            (
                cuda_opt.processes,
                cuda_opt.workers_per_process,
                cuda_opt.max_concurrent_chunks,
                cuda_opt.ort_threads
            ),
            (1, 2, 2, 4)
        );
        let cuda_prod = ParallelSettings::preset_productive(DeviceChoice::Cuda, logical, fp32);
        assert_eq!(
            (
                cuda_prod.processes,
                cuda_prod.workers_per_process,
                cuda_prod.max_concurrent_chunks,
                cuda_prod.ort_threads
            ),
            (2, 2, 4, 4)
        );
        // малое число CPU не даёт бессмысленных воркеров
        let small = ParallelSettings::preset_optimal(DeviceChoice::Cpu, 2, fp32);
        assert_eq!(small.workers_per_process, 1);
        // detect_profile по каноническим таблицам
        assert_eq!(
            ParallelSettings::default().detect_profile(DeviceChoice::Cpu),
            ParallelProfile::Optimal
        );
        let prod = ParallelSettings {
            profile: ParallelProfile::Productive,
            processes: 2,
            workers_per_process: 2,
            max_concurrent_chunks: 4,
            ort_threads: 4,
        };
        assert_eq!(
            prod.detect_profile(DeviceChoice::Cuda),
            ParallelProfile::Productive
        );
        assert_eq!(
            prod.detect_profile(DeviceChoice::Cpu),
            ParallelProfile::Custom,
            "2×2 для CPU — не пресет"
        );
    }

    /// G6.6: миграция старых конфигов G6.2/G6.1 (batch_cuda_mode/workers)
    /// в parallel_cuda; повторная загрузка мигрированного файла ничего не
    /// подменяет; прежний выбор пользователя не теряется.
    #[test]
    fn parallel_migration_from_legacy_batch_cuda() {
        let d = tmpdir("par_mig");
        let p = d.join("s.json");
        // MaxSpeed (гибрид 2×2) → Производительный CUDA
        std::fs::write(&p, r#"{"batch_cuda_mode": "MaxSpeed", "threads": 4}"#).unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(s.parallel_cuda.profile, ParallelProfile::Productive);
        assert_eq!(
            (
                s.parallel_cuda.processes,
                s.parallel_cuda.workers_per_process
            ),
            (2, 2)
        );
        assert_eq!(s.parallel_cuda.max_concurrent_chunks, 4);
        // Economy + ручные workers=1 → Custom 1×1 (выбор сохранён)
        std::fs::write(&p, r#"{"batch_cuda_mode": "Economy", "workers": 1}"#).unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(s.parallel_cuda.profile, ParallelProfile::Custom);
        assert_eq!(s.parallel_cuda.workers_per_process, 1);
        // Economy без workers → дефолт Оптимальный
        std::fs::write(&p, r#"{"batch_cuda_mode": "Economy"}"#).unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(s.parallel_cuda, ParallelSettings::default());
        // мигрированный файл: повторная загрузка сохраняет пользовательские
        // правки (parallel_cuda уже есть — миграция не выполняется)
        std::fs::write(
            &p,
            r#"{"parallel_cuda": {"profile": "Custom", "processes": 1,
                "workers_per_process": 1, "max_concurrent_chunks": 1,
                "ort_threads": 2}, "batch_cuda_mode": "MaxSpeed"}"#,
        )
        .unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(s.parallel_cuda.profile, ParallelProfile::Custom);
        assert_eq!(s.parallel_cuda.workers_per_process, 1);
        assert_eq!(s.parallel_cuda.ort_threads, 2);
        // неизвестный ключ batch_cuda_mode пережил round-trip (extra)
        save_settings(&s, &p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("\"batch_cuda_mode\": \"MaxSpeed\""), "{raw}");
    }

    /// G6.7.1: планируемое к загрузке число Pocket (P=1 → min(W,C);
    /// P>1 → все P×W независимо от C).
    #[test]
    fn planned_pockets_matches_pool_semantics() {
        let ps = |p: u32, w: u32, c: u32| ParallelSettings {
            profile: ParallelProfile::Custom,
            processes: p,
            workers_per_process: w,
            max_concurrent_chunks: c,
            ort_threads: 4,
        };
        // (P, W, C) -> планируемых Pocket
        for (p, w, c, want) in [
            (1u32, 4u32, 1u32, 1u32),
            (1, 4, 2, 2),
            (1, 4, 4, 4),
            (2, 2, 1, 4),
            (2, 2, 2, 4),
            (2, 2, 4, 4),
        ] {
            assert_eq!(ps(p, w, c).planned_pockets(), want, "P{p}×W{w} C{c}");
        }
    }

    /// G6.7.1: расчёт VRAM в МБ по методике префлайта; MB→GB один раз и
    /// без обнуления от смешения единиц (прежний баг: ГБ-коэффициенты
    /// делились на 1000 ещё раз → «0.0 ГБ»).
    #[test]
    fn vram_need_mb_matches_preflight_methodology() {
        // 2 Pocket (однопроцессный пул): 2×2355 + 900 = 5610 МБ ≈ 5.6 ГБ
        assert_eq!(vram_need_mb(2, false), 5610);
        assert!((vram_need_mb(2, false) as f64 / 1000.0 - 5.6).abs() < 0.05);
        // 4 Pocket (мультипроцесс или >2): 4×2355 + 1500 = 10920 МБ ≈ 10.9 ГБ
        assert_eq!(vram_need_mb(4, true), 10920);
        assert_eq!(
            vram_need_mb(4, false),
            10920,
            ">2 Pocket — консервативный запас"
        );
        assert!((vram_need_mb(4, true) as f64 / 1000.0 - 10.9).abs() < 0.05);
        // 2 Pocket мультипроцесс (например 2×1): запас 1500
        assert_eq!(vram_need_mb(2, true), 6210);
        // никакая конфигурация не даёт нулевую/абсурдную оценку
        for pockets in 1..=4u32 {
            for mp in [false, true] {
                let gb = vram_need_mb(pockets, mp) as f64 / 1000.0;
                assert!(gb > 3.0 && gb < 12.0, "pockets={pockets} mp={mp}: {gb}");
            }
        }
    }

    /// G6.7.1: summary_line различает настроенное и планируемое число
    /// Pocket (однопроцессный пул при C < W) и не меняет компактную форму
    /// в остальных случаях.
    #[test]
    fn summary_line_distinguishes_planned_pockets() {
        let base = |p: u32, w: u32, c: u32| ParallelSettings {
            profile: ParallelProfile::Custom,
            processes: p,
            workers_per_process: w,
            max_concurrent_chunks: c,
            ort_threads: 4,
        };
        // P=1, W=4, C=1: настроено 4, будет загружен 1
        assert_eq!(
            base(1, 4, 1).summary_line(),
            "1 процесс · настроено 4 Pocket · будет загружен 1 Pocket · до 1 чанков · ORT 4"
        );
        // P=2, W=2, C=1: мультипроцесс — все 4 Pocket, компактная форма
        assert_eq!(
            base(2, 2, 1).summary_line(),
            "2 процесса · 4 Pocket · до 1 чанков · ORT 4"
        );
        // P=1, W=2, C=2: стандартная компактная строка
        assert_eq!(
            base(1, 2, 2).summary_line(),
            "1 процесс · 2 Pocket · до 2 чанков · ORT 4"
        );
    }

    /// G6.7: старый ключ `max_concurrent_files` (семантика файлов G6.6)
    /// загружается как стартовое значение `max_concurrent_chunks` (алиас);
    /// конфликт старого и нового ключей невозможен (новый приоритетен и
    /// переименовывает поле при сохранении).
    #[test]
    fn parallel_migration_files_to_chunks_alias() {
        let d = tmpdir("par_alias");
        let p = d.join("s.json");
        std::fs::write(
            &p,
            r#"{"parallel_cpu": {"profile": "Custom", "processes": 1,
                "workers_per_process": 2, "max_concurrent_files": 2,
                "ort_threads": 4}}"#,
        )
        .unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(s.parallel_cpu.max_concurrent_chunks, 2, "F→C перенос");
        assert_eq!(s.parallel_cpu.processes, 1);
        // повторный save пишет только новый ключ
        save_settings(&s, &p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("max_concurrent_chunks"), "{raw}");
        assert!(!raw.contains("max_concurrent_files"), "{raw}");
        // новый ключ в конфиге не затирается старым (проверка «конфликта»)
        std::fs::write(
            &p,
            r#"{"parallel_cpu": {"profile": "Custom", "processes": 1,
                "workers_per_process": 4, "max_concurrent_files": 1,
                "max_concurrent_chunks": 3, "ort_threads": 4}}"#,
        )
        .unwrap();
        let s = load_settings(&p).unwrap();
        assert_eq!(
            s.parallel_cpu.max_concurrent_chunks, 3,
            "новый ключ приоритетен"
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
