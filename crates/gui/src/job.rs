//! G2: фоновое задание синтеза из GUI — напрямую `app_core::pipeline::
//! synthesize_stream` в рабочем потоке (НЕ subprocess CLI). Здесь:
//! резолв модели fast/slow/custom, переменные окружения ORT (без изменения
//! системного PATH — только процесс GUI), сборка `PipelineOptions` с паритетом
//! к CLI, события прогресса для UI и итоговый `JobResult` со всеми путями
//! (итог/черновик/диагностический журнал/лог чанков).

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::hybrid::{self, ChildJobGuard, Ev, HybridDriver, HybridJob, HybridParams, PhaseMsg};
use app_core::persistent::{loaned_real_worker_factory, spawn_workers, WorkerSlot};
use app_core::pipeline::{
    log_path_for, synthesize_stream, synthesize_stream_with_worker_factory, write_chunk_log,
    write_diag_log, ChunkLogMeta, DiagMeta, PipelineOptions, RunPhase, RunStatus, StreamError,
};
use app_core::pool::SeedPolicy;
use app_core::sink::{make_sink_with_overwrite, OutputFormat};
use app_core::worker::{real_worker_factory, RealWorker};
use shtorm_engine::{Device, ModelPrecision};
use text_processing::CharRules;

use crate::batch::{
    count_actionable, plan_batch, preflight_batch, run_batch, run_prepared_parallel, BatchEvent,
    BatchFile, BatchPlan, BatchReport, BatchStatus, FileReport, FileStatus, PreparedBatch,
    PreparedFile,
};
use crate::resources;
use crate::settings::{
    DeviceChoice, GuiSettings, OutputFormatChoice, ParallelProfile, ParallelSettings,
};

// G4: единый резолвер ресурсов; реэкспорт сохраняет существующий API/тесты.
pub use crate::resources::{find_project_root, resolve_ort_dylib, resolve_ort_dylib_in};

/// Полный вход задания: снимок настроек (уже clamped), текст и итоговый путь.
#[derive(Debug, Clone)]
pub struct JobConfig {
    pub settings: GuiSettings,
    pub text: String,
    pub out_path: PathBuf,
}

/// G5.1: фаза задания для UI. После [JobPhase::Finalizing] отмена уже не
/// может предотвратить публикацию/замену результата — кнопка «Стоп» должна
/// быть отключена, а статус честно показывать сохранение результата.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    Generating,
    Finalizing,
    Publishing,
}

impl From<RunPhase> for JobPhase {
    fn from(p: RunPhase) -> Self {
        match p {
            RunPhase::Generating => JobPhase::Generating,
            RunPhase::Finalizing => JobPhase::Finalizing,
            RunPhase::Publishing => JobPhase::Publishing,
        }
    }
}

/// События задания для UI-потока (канал + `ctx.request_repaint`).
/// В пакетном режиме события разных файлов приходят ВПЕРЕМЕШКУ (G6.1:
/// постоянный пул обрабатывает файлы параллельно) — приёмник различает их
/// по `index` (K файла), а не по порядку прибытия.
#[derive(Debug, Clone)]
pub enum JobEvent {
    Progress {
        done: usize,
        total: usize,
    },
    /// G5.1: смена фазы (генерация/финализация/публикация).
    Phase(JobPhase),
    /// G6.1: пакет начат: всего файлов и сколько обрабатывается одновременно
    /// (постоянных воркеров пула; 1 — последовательный путь).
    /// G6.1.1: отправляется ДО preflight (сразу после plan_batch) с
    /// `workers = 0` — ранние FileDone preflight (пропуски/конфликты/
    /// unsafe-пути) обязаны попадать в уже инициализированное состояние UI.
    /// Второй BatchStarted в рамках одного пакета не отправляется.
    BatchStarted {
        total: usize,
        workers: usize,
    },
    /// G6.1.1: постоянные воркеры пула реально загружены и работают.
    /// Приходит после BatchStarted и только обновляет число воркеров в
    /// существующем состоянии — счётчики ранних FileDone не сбрасываются.
    /// Последовательный путь шлёт его с workers=1 до preflight.
    BatchWorkersReady {
        workers: usize,
    },
    /// G6.1: файл K/N стартовал (в параллельном пакете — из потока воркера).
    FileStarted {
        index: usize,
        total: usize,
        input: PathBuf,
        output: PathBuf,
    },
    /// Пакетный режим: прогресс чанков файла K/N.
    FileProgress {
        index: usize,
        total: usize,
        done: usize,
        chunks_total: usize,
    },
    /// G6.1: фаза конкретного файла пакета. После Finalizing отмена уже не
    /// может предотвратить публикацию ЭТОГО файла — «Стоп» учитывает фазы
    /// всех активных файлов.
    FilePhase {
        index: usize,
        phase: JobPhase,
    },
    /// Пакетный режим: итог одного файла (для журнала/статуса).
    FileDone {
        index: usize,
        total: usize,
        report: Box<FileReport>,
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
    /// G5.1: Completed И существовавший файл реально заменён (не создан новый).
    pub replaced_existing: bool,
    /// Пакетный отчёт (None — одиночный режим).
    pub batch: Option<BatchReport>,
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
            replaced_existing: false,
            batch: None,
        }
    }

    /// G5: существующий результат при выключенной перезаписи — не ошибка,
    /// а информационный пропуск (файл остаётся побитово неизменным).
    pub fn skipped(summary: String) -> Self {
        Self {
            status: JobStatus::Skipped,
            summary,
            final_path: None,
            partial_path: None,
            diag_path: None,
            chunk_log_path: None,
            rtf: None,
            progress: (0, 0),
            replaced_existing: false,
            batch: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Completed,
    /// Пакет: часть файлов готова, часть — с ошибками.
    Partial,
    Cancelled,
    Failed,
    /// Пакет: синтезировать было нечего (пустой план или все файлы пропущены).
    NoWork,
    /// G5: результат уже существует, перезапись выключена — файл пропущен.
    Skipped,
}

impl JobStatus {
    pub fn label(&self) -> &'static str {
        match self {
            JobStatus::Completed => "Завершено",
            JobStatus::Partial => "Частично",
            JobStatus::Cancelled => "Отменено",
            JobStatus::Failed => "Ошибка",
            JobStatus::NoWork => "Нет работы",
            JobStatus::Skipped => "Пропущено",
        }
    }
}

/// Каталог модели по выбору Fast/Slow/Custom (+ INT8-варианты G6.3 — тот же
/// каталог, другая точность); проверка обязательных файлов движка — как
/// валидация G1, но тем же кодом для реального запуска. Для INT8 проверяются
/// ТРИ квантованных графа; молчаливого отката на FP32 нет.
pub fn resolve_model_dir(s: &GuiSettings) -> Result<PathBuf> {
    let dir = match s.model.sub_dir() {
        Some(sub) => resources::resolve_model_base(s)?.join(sub),
        None => {
            let p = resources::resolve_user_path(&s.model_dir);
            anyhow::ensure!(
                !p.as_os_str().is_empty(),
                "для модели Custom не задан каталог model_dir"
            );
            p
        }
    };
    match s.model.precision() {
        ModelPrecision::Fp32 => {
            anyhow::ensure!(
                dir.join("flow_lm_main.onnx").is_file(),
                "в каталоге модели нет flow_lm_main.onnx: {}",
                dir.display()
            );
        }
        ModelPrecision::Int8 => {
            for file in [
                "flow_lm_main_int8.onnx",
                "flow_lm_flow_int8.onnx",
                "mimi_decoder_int8.onnx",
            ] {
                anyhow::ensure!(
                    dir.join(file).is_file(),
                    "в каталоге модели нет INT8-графа {file}: {} \
                     — режим INT8 требует все три квантованных графа \
                     (fallback на FP32 запрещён)",
                    dir.display()
                );
            }
        }
    }
    Ok(dir)
}

/// Результат чтения char_rules из runtime.json модели. Отсутствие файла —
/// штатное состояние (правила не заданы); невалидный файл/секция — явное
/// предупреждение для пользователя, а не молчаливое игнорирование.
#[derive(Debug, Clone)]
pub enum CharRulesLoad {
    Absent,
    Loaded(CharRules),
    Warning(String),
}

/// char_rules из runtime.json модели (та же логика, что CLI --char-rules).
pub fn load_char_rules(model_dir: &Path) -> CharRulesLoad {
    let p = model_dir.join("runtime.json");
    let raw = match std::fs::read_to_string(&p) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CharRulesLoad::Absent,
        Err(e) => {
            return CharRulesLoad::Warning(format!(
                "runtime.json не прочитан ({}): {e} — char_rules не применяются",
                p.display()
            ))
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return CharRulesLoad::Warning(format!(
                "runtime.json невалиден ({}): {e} — char_rules не применяются",
                p.display()
            ))
        }
    };
    match v.get("char_rules") {
        Some(cr) => match serde_json::from_value::<CharRules>(cr.clone()) {
            Ok(rules) => CharRulesLoad::Loaded(rules),
            Err(e) => CharRulesLoad::Warning(format!(
                "секция char_rules в {} не разобрана: {e} — правила не применяются",
                p.display()
            )),
        },
        None => CharRulesLoad::Warning(format!(
            "в {} нет секции char_rules — правила не применяются",
            p.display()
        )),
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
        // относительный голос — от каталога exe (портативный запуск)
        voice: resources::resolve_user_path(&s.voice_wav),
        device: match s.device {
            DeviceChoice::Cpu => Device::Cpu,
            DeviceChoice::Cuda => Device::cuda(s.device_id),
        },
        threads: s.threads as usize,
        // G6.3: точность графов из выбранного варианта модели; каждый Pocket
        // задания (пул/гибрид/дочерние процессы) грузит одни и те же файлы.
        precision: s.model.precision(),
        seed: if s.seed < 0 {
            None
        } else {
            Some(s.seed as u64)
        },
        temperature: s.temperature,
        decode_steps: s.decode_steps as usize,
        // «Авто» разрешается здесь во встроенное −4.0 (teratts-android);
        // число передаётся как есть
        eos_threshold: s.eos_threshold.value(),
        eos_tail_short: s.eos_tail_short,
        eos_tail_long: s.eos_tail_long,
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
    resources::resolve_user_path(&s.out_dir).join(format!("{stem}.{}", s.format.extension_str()))
}

/// G5/G5.1: защита пути результата. Замена разрешена только для готового
/// аудиофайла назначения; путь не должен проходить через symlink/junction,
/// выходить из каталога вывода или совпадать с исходным текстом/голосом/
/// каталогом модели. Проверки физические (canonicalize существующего
/// префикса), поэтому альтернативные имена не обходят ограничения.
/// `model_dir` — уже РАЗРЕШЁННЫЙ каталог модели (ошибка резолва не может
/// молча пропустить проверку: она случилась раньше и задание не дошло бы
/// сюда).
pub fn ensure_output_path_safe(s: &GuiSettings, out_path: &Path, model_dir: &Path) -> Result<()> {
    let out_root = resources::resolve_user_path(&s.out_dir);
    crate::batch::ensure_no_reparse_components(out_path)?;
    anyhow::ensure!(
        crate::batch::physically_under(out_path, &out_root),
        "выходной путь вне каталога результата: {}",
        out_path.display()
    );
    let text_path = resources::resolve_user_path(&s.text_file);
    anyhow::ensure!(
        !crate::batch::physically_same(out_path, &text_path),
        "выходной путь совпадает с исходным текстом: {}",
        out_path.display()
    );
    let voice_path = resources::resolve_user_path(&s.voice_wav);
    anyhow::ensure!(
        !crate::batch::physically_same(out_path, &voice_path),
        "выходной путь совпадает с WAV-голосом: {}",
        out_path.display()
    );
    anyhow::ensure!(
        !crate::batch::physically_under(out_path, model_dir),
        "выходной путь внутри каталога модели: {}",
        out_path.display()
    );
    Ok(())
}

/// G5.1: создать недостающие каталоги пути, НЕ следуя по symlink/junction.
/// Каждый существующий компонент проверяется на reparse point ПЕРЕД тем,
/// как через него создаётся следующий; создаются только отсутствующие
/// компоненты (по одному, `create_dir`). `std::fs::create_dir_all` так не
/// умеет: он молча проходит существующие junction и создаёт каталоги уже
/// «по ту сторону» ссылки — поэтому здесь ручной обход.
pub fn create_dir_all_no_reparse(path: &Path) -> Result<()> {
    let abs = crate::batch::absolute_lexical(path);
    let mut chain: Vec<&Path> = abs.ancestors().collect();
    chain.reverse();
    for anc in chain {
        if anc.as_os_str().is_empty() {
            continue;
        }
        match std::fs::symlink_metadata(anc) {
            Ok(md) => {
                if crate::batch::is_reparse_metadata(&md) {
                    bail!(
                        "путь проходит через ссылку/точку перенаправления (reparse point): {} — укажите физический каталог",
                        anc.display()
                    );
                }
                anyhow::ensure!(md.is_dir(), "компонент пути не каталог: {}", anc.display());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(anc)
                    .map_err(|e| anyhow::anyhow!("создать каталог {}: {e}", anc.display()))?;
            }
            Err(e) => bail!("проверить компонент пути {}: {e}", anc.display()),
        }
    }
    Ok(())
}

/// Уже инициализированная DLL (ORT грузится в процесс один раз — смена
/// CPU/CUDA требует перезапуска GUI; честная ошибка вместо тихого фолбэка).
static ORT_LOADED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Сравнение путей без учёта регистра на Windows и хвостовых разделителей.
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

/// Нужна ли инициализация ORT. Проверка ORT_LOADED выполняется ДО любых
/// изменений PATH/ORT_DYLIB_PATH: тот же DLL — no-op, другой — ошибка.
fn ort_init_needed(dll: &Path, loaded: Option<&Path>) -> Result<bool> {
    match loaded {
        Some(prev) if same_path(prev, dll) => Ok(false),
        Some(prev) => bail!(
            "onnxruntime уже загружен с {} — для смены CPU/CUDA перезапустите приложение",
            prev.display()
        ),
        None => Ok(true),
    }
}

/// Добавить каталоги в НАЧАЛО PATH без повторов (повторное задание на том же
/// runtime не раздувает PATH CUDA-каталогами). None — PATH уже содержит всё.
fn merge_path_prepend(old: &std::ffi::OsStr, prepend: &[PathBuf]) -> Option<std::ffi::OsString> {
    let existing: Vec<PathBuf> = std::env::split_paths(old).collect();
    let mut front: Vec<PathBuf> = Vec::new();
    for d in prepend {
        if existing.iter().any(|p| same_path(p, d)) || front.iter().any(|p| same_path(p, d)) {
            continue;
        }
        front.push(d.clone());
    }
    if front.is_empty() {
        return None;
    }
    front.extend(existing);
    std::env::join_paths(front).ok()
}

/// Подготовить окружение и инициализировать ORT (один раз за процесс).
/// PATH/ORT_DYLIB_PATH меняются ТОЛЬКО в процессе GUI (система не трогается).
pub fn setup_ort_environment(s: &GuiSettings) -> Result<()> {
    let (dll, prepend) = resolve_ort_dylib(s)?;
    // Сначала проверяем ORT_LOADED — при повторном задании на том же runtime
    // PATH/ORT_DYLIB_PATH не меняются вовсе.
    if !ort_init_needed(&dll, ORT_LOADED.get().map(PathBuf::as_path))? {
        return Ok(());
    }
    // снимок env для отката, если ort::init не состоится
    let old_path = std::env::var_os("PATH");
    let old_dylib = std::env::var_os("ORT_DYLIB_PATH");
    if let Some(old) = &old_path {
        if let Some(merged) = merge_path_prepend(old, &prepend) {
            std::env::set_var("PATH", merged);
        }
    } else if !prepend.is_empty() {
        if let Ok(joined) = std::env::join_paths(prepend.clone()) {
            std::env::set_var("PATH", joined);
        }
    }
    std::env::set_var("ORT_DYLIB_PATH", &dll);
    if !ort::init().with_name("PocketTTS-GUI-Rust").commit() {
        // init не состоялся — не оставляем процесс с чужим DLL/путём
        match old_dylib {
            Some(v) => std::env::set_var("ORT_DYLIB_PATH", v),
            None => std::env::remove_var("ORT_DYLIB_PATH"),
        }
        match old_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        // G5: подсказка по устройству — частая причина сбоя загрузки ORT.
        let hint = match s.device {
            DeviceChoice::Cuda => {
                "проверьте драйвер NVIDIA и наличие CUDA/cuDNN DLL рядом с onnxruntime.dll; \
                 при нехватке VRAM уменьшите воркеров или выберите CPU"
            }
            DeviceChoice::Cpu => {
                "проверьте VC++ runtime (vcruntime140.dll и соседние) и целостность onnxruntime.dll"
            }
        };
        bail!("ort::init() не удалось (DLL: {}); {hint}", dll.display());
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
pub fn execute_job(cfg: &JobConfig, cancel: &AtomicBool, emit: &dyn Fn(JobEvent)) -> JobResult {
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
    emit(JobEvent::Log(format!("модель: {}", model_dir.display())));
    let mut char_warning = None;
    let char_rules = match load_char_rules(&model_dir) {
        CharRulesLoad::Absent => None,
        CharRulesLoad::Loaded(rules) => Some(rules),
        CharRulesLoad::Warning(w) => {
            // невалидный/неполный runtime.json не игнорируем молча —
            // предупреждение уходит в журнал и в итоговую сводку
            emit(JobEvent::Log(format!("ВНИМАНИЕ: {w}")));
            char_warning = Some(w);
            None
        }
    };
    if let Err(e) = setup_ort_environment(s) {
        return attach_warning(fail(format!("{e:#}")), &char_warning);
    }
    // G5.1: защита путей — СТРОГО ДО любого создания каталогов. Существующие
    // компоненты (включая reparse-точки) и физическое расположение проверяются
    // до mkdir: junction в пути результата не может «провести» создание
    // каталога за пределы разрешённого каталога вывода.
    if let Err(e) = ensure_output_path_safe(s, &cfg.out_path, &model_dir) {
        return attach_warning(fail(format!("{e:#}")), &char_warning);
    }
    // Папка результата может не существовать — создаём без следования по
    // symlink/junction (каждый существующий компонент перепроверяется).
    if let Some(parent) = cfg.out_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            if let Err(e) = create_dir_all_no_reparse(parent) {
                return attach_warning(fail(format!("{e:#}")), &char_warning);
            }
        }
    }
    // G5.1: повторная валидация ПОСЛЕ создания — закрывает окно TOCTOU
    // (появившаяся между проверкой и созданием ссылка была бы раскрыта
    // canonicalize как физически чужое расположение).
    if let Err(e) = ensure_output_path_safe(s, &cfg.out_path, &model_dir) {
        return attach_warning(fail(format!("{e:#}")), &char_warning);
    }
    let overwrite = s.overwrite_existing;
    // G5.1: существовал ли готовый файл ДО запуска — семантика сообщений
    // («создан новый» vs «обновлён существующий») и замены при отмене.
    let existed_before = cfg.out_path.exists();
    if existed_before {
        if !overwrite {
            // Не ошибка и не модальное окно: файл просто пропускается и
            // остаётся побайтово неизменным.
            return attach_warning(
                JobResult::skipped(format!(
                    "Пропущено: файл уже существует: {} (перезапись выключена)",
                    cfg.out_path.display()
                )),
                &char_warning,
            );
        }
        emit(JobEvent::Log(format!(
            "перезапись разрешена: {} — старый файл будет заменён только после успешного синтеза",
            cfg.out_path.display()
        )));
    }
    let ffmpeg_path = if s.ffmpeg_path.trim().is_empty() {
        None
    } else {
        // относительный путь из настроек — от каталога exe
        Some(resources::resolve_user_path(&s.ffmpeg_path))
    };
    let preset = s.preset_for_format();
    let sink = match make_sink_with_overwrite(
        &cfg.out_path,
        24_000,
        output_format(s.format),
        preset,
        ffmpeg_path.as_deref(),
        overwrite,
    ) {
        Ok(sink) => sink,
        Err(e) => return attach_warning(fail(format!("{e:#}")), &char_warning),
    };
    let opts = pipeline_options(s, model_dir.clone(), char_rules);

    emit(JobEvent::Log(format!(
        "запуск: {} | {} | воркеров {} | {} | {}",
        cfg.out_path.display(),
        device_label(s),
        s.effective_workers(),
        s.format.label(),
        if preset.is_empty() { "-" } else { preset }
    )));

    // --- синтез (прогресс/фазы → UI; отмена до точки решения — Cancelled
    // без публикации; после неё конвейер доходит до конца, UI знает фазу) ---
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
        &|ph| emit(JobEvent::Phase(ph.into())),
    );
    let elapsed = t0.elapsed().as_secs_f32();

    let mut res = match result {
        Ok(outcome) => {
            finish_from_outcome(cfg, outcome, elapsed, cancel, overwrite, existed_before)
        }
        Err(e) => finish_from_error(cfg, &e, elapsed),
    };
    res.progress = progress;
    attach_warning(res, &char_warning)
}

/// G3/G6.1: пакетный прогон. Снимок файлов строится ДО первого синтеза
/// (`plan_batch`), предзапусковые проверки — ДО запуска воркеров
/// (`preflight_batch`). Два пути выполнения:
/// - **Постоянный пул (G6.1)**: пакетный CUDA с ≥2 файлами-кандидатами —
///   N воркеров, каждый со своим Pocket, загруженным ОДИН раз на весь пакет;
///   один файл целиком обрабатывает один воркер; общая динамическая очередь.
///   Диагностический откат на прежний пофайловый путь — `SHTORM_GUI_BATCH_POOL=0`.
/// - **Последовательный путь (прежний G3)**: модель создаётся на каждый файл
///   (CPU-пакеты, пакеты из одного файла, откат выше).
///
/// Ошибки изолированы; отмена останавливает выдачу новых файлов, активные
/// дорабатывают по семантике G5.1.
pub fn execute_batch(
    settings: &GuiSettings,
    source: &Path,
    cancel: &Arc<AtomicBool>,
    emit: &Arc<dyn Fn(JobEvent) + Send + Sync>,
) -> JobResult {
    let t0 = Instant::now();
    // относительная папка результата — от каталога exe (портативный режим)
    let out_root = resources::resolve_user_path(&settings.out_dir);
    let plan = match plan_batch(source, &out_root) {
        Ok(p) => p,
        Err(e) => return JobResult::failed(format!("пакетный режим: {e:#}")),
    };
    emit(JobEvent::Log(format!(
        "пакет: {} | найдено {} файлов | результат: {}",
        source.display(),
        plan.files.len(),
        plan.out_root.display()
    )));
    if plan.files.is_empty() {
        // Пустой пакет — не «успешный синтез»: отдельный статус NoWork.
        let cancelled = cancel.load(Ordering::SeqCst);
        let report = BatchReport {
            files: Vec::new(),
            conflicts: plan.conflicts.clone(),
            cancelled,
            pool_loads: None,
            pool_workers: None,
            pool_processes: None,
        };
        return JobResult {
            status: if cancelled {
                JobStatus::Cancelled
            } else {
                JobStatus::NoWork
            },
            summary: format!(
                "{}; файлов .txt/.md/.log не найдено — синтез не выполнялся",
                report.summary()
            ),
            final_path: None,
            partial_path: None,
            diag_path: None,
            chunk_log_path: None,
            rtf: None,
            progress: (0, 0),
            replaced_existing: false,
            batch: Some(report),
        };
    }

    // G6.1.1: инициализация пакетного UI ДО любых событий файлов — preflight
    // обоих путей (пул и последовательный) уже шлёт FileDone для пропущенных/
    // конфликтных/битых файлов, и они обязаны учитываться в счётчиках.
    // Воркеров ещё нет (0): реальное число сообщит BatchWorkersReady после
    // загрузки пула (или сразу в последовательном пути).
    emit(JobEvent::BatchStarted {
        total: plan.files.len(),
        workers: 0,
    });

    let ext = settings.format.extension_str().to_string();
    let overwrite = settings.overwrite_existing;
    let pool_enabled = std::env::var("SHTORM_GUI_BATCH_POOL").as_deref() != Ok("0");
    let cfg = settings.parallel_config();
    let actionable = if pool_enabled {
        count_actionable(&plan, &ext, overwrite)
    } else {
        0
    };
    // G6.6: единая модель P/W/F/T для CPU и CUDA. Запрошенная конфигурация
    // протоколируется ДО префлайтов; любое отклонение фактической топологии
    // от запрошенной объясняется в журнале явно (молчаливой подмены нет).
    emit(JobEvent::Log(format!(
        "параллельная обработка: профиль {} | {} | запрошено P={} W={} F={} T={}",
        cfg.profile.label(),
        device_label(settings),
        cfg.processes,
        cfg.workers_per_process,
        cfg.max_concurrent_files,
        cfg.ort_threads
    )));
    let topo = decide_topology(settings, &cfg, actionable, pool_enabled, emit);
    let report = match topo {
        BatchTopology::Sequential => {
            execute_batch_sequential(settings, &plan, &ext, overwrite, cancel, emit)
        }
        BatchTopology::InProcessPool { workers } => {
            match execute_batch_persistent_pool(
                settings, &cfg, workers, &plan, &ext, overwrite, cancel, emit,
            ) {
                Ok(r) => r,
                Err(e) => return JobResult::failed(format!("пакетный режим: {e:#}")),
            }
        }
        BatchTopology::MultiProcess {
            children,
            workers_per_child,
            max_active_files,
        } => match execute_batch_hybrid(
            settings,
            &cfg,
            children,
            workers_per_child,
            max_active_files,
            &plan,
            &ext,
            overwrite,
            cancel,
            emit,
        ) {
            Ok(r) => r,
            Err(e) => {
                return JobResult::failed(format!(
                    "пакетный режим ({children}×{workers_per_child}): {e:#}"
                ))
            }
        },
    };

    let status = match report.status() {
        BatchStatus::Completed => JobStatus::Completed,
        BatchStatus::Partial => JobStatus::Partial,
        BatchStatus::Cancelled => JobStatus::Cancelled,
        BatchStatus::Failed => JobStatus::Failed,
        BatchStatus::NoWork => JobStatus::NoWork,
    };
    let mut summary = report.summary();
    summary.push_str(&format!("; {:.1} с", t0.elapsed().as_secs_f32()));
    JobResult {
        status,
        summary,
        final_path: None,
        partial_path: None,
        diag_path: None,
        chunk_log_path: None,
        rtf: None,
        progress: (report.total(), report.total()),
        replaced_existing: false,
        batch: Some(report),
    }
}

/// Перевод BatchEvent → JobEvent для последовательного пути пакета.
/// `cur_file` — K текущего файла (обновляется на FileStarted ДО synth):
/// фазы execute_job превращаются в FilePhase с индексом файла.
#[allow(clippy::too_many_arguments)]
fn execute_batch_sequential(
    settings: &GuiSettings,
    plan: &BatchPlan,
    ext: &str,
    overwrite: bool,
    cancel: &Arc<AtomicBool>,
    emit: &Arc<dyn Fn(JobEvent) + Send + Sync>,
) -> BatchReport {
    // G6.1.1: BatchStarted уже отправлен в execute_batch; последовательный
    // путь обрабатывает файлы по одному — «воркер» готов сразу, до preflight.
    emit(JobEvent::BatchWorkersReady { workers: 1 });
    let cur_file = std::sync::atomic::AtomicUsize::new(0);
    let emit_batch = |ev: BatchEvent| match ev {
        BatchEvent::Planned { total, warnings } => {
            emit(JobEvent::Log(format!(
                "пакет: план зафиксирован — {total} файлов, предупреждений: {}",
                warnings.len()
            )));
        }
        BatchEvent::FileStarted {
            index,
            total,
            input,
            output,
        } => {
            cur_file.store(index, Ordering::SeqCst);
            let msg = format!(
                "[{index}/{total}] старт: {} → {}",
                input.display(),
                output.display()
            );
            emit(JobEvent::FileStarted {
                index,
                total,
                input,
                output,
            });
            emit(JobEvent::Log(msg));
        }
        BatchEvent::FileProgress {
            index,
            total,
            done,
            chunks_total,
        } => {
            emit(JobEvent::FileProgress {
                index,
                total,
                done,
                chunks_total,
            });
        }
        BatchEvent::FileFinished {
            index,
            total,
            report,
        } => {
            emit(JobEvent::Log(format!(
                "[{index}/{total}] {}",
                report.line()
            )));
            emit(JobEvent::FileDone {
                index,
                total,
                report,
            });
        }
        BatchEvent::Log(line) => emit(JobEvent::Log(line)),
    };
    run_batch(
        plan,
        ext,
        overwrite,
        cancel,
        &emit_batch,
        |file, out_path, cancel, progress| {
            let text = match std::fs::read_to_string(&file.input) {
                Ok(t) => t,
                Err(e) => {
                    return FileReport::failed(
                        file,
                        Some(out_path.to_path_buf()),
                        format!("прочитать {}: {e}", file.input.display()),
                    )
                }
            };
            let cfg = JobConfig {
                settings: settings.clone(),
                text,
                out_path: out_path.to_path_buf(),
            };
            // execute_job принимает Fn, а progress — FnMut: внутренняя
            // изменяемость через RefCell (без потери промежуточных событий).
            let progress_cell = std::cell::RefCell::new(progress);
            let res = execute_job(&cfg, cancel, &|ev| match ev {
                JobEvent::Progress { done, total } => {
                    if let Ok(mut p) = progress_cell.try_borrow_mut() {
                        (*p)(done, total);
                    }
                }
                JobEvent::Phase(ph) => {
                    emit(JobEvent::FilePhase {
                        index: cur_file.load(Ordering::SeqCst),
                        phase: ph,
                    });
                }
                JobEvent::Log(line) => {
                    emit(JobEvent::Log(format!("[{}] {line}", file.input.display())));
                }
                _ => {}
            });
            file_report_from_job_result(&file.input, &file.relative, out_path, res)
        },
    )
}

/// Ориентировочная VRAM одного CUDA-воркера (Pocket + арены ORT), МБ —
/// по замерам P2B.3 (2 воркера ≈ 4.9 ГБ, база ~0.2 ГБ).
const BATCH_POOL_VRAM_PER_WORKER_MB: f64 = 2355.0;
/// Запас поверх N воркеров однопроцессного пула (фон/фрагментация).
/// Префлайт — НЕ абсолютная защита от OOM, а способ заранее выбрать
/// экономную конфигурацию.
const BATCH_POOL_VRAM_RESERVE_MB: f64 = 900.0;
/// Консервативный запас мультипроцессного режима (P2B.3: пик 9.2–9.4 ГБ;
/// B4 словил CUDA OOM при ~11.5 ГБ свободных — покрывает фон, фрагментацию
/// и промежуточные тензоры декодера).
const MULTIPROCESS_VRAM_RESERVE_MB: f64 = 1500.0;

/// G6.6: фактическая топология запуска пакета. Решение чистое (unit-
/// тестируемое); журналирование отклонений от запрошенной конфигурации —
/// внутри `decide_topology`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatchTopology {
    /// Последовательный путь: <2 файлов-кандидатов (чанк-параллелизм
    /// одиночного файла в нём быстрее файлового пула) или диагностический
    /// откат `SHTORM_GUI_BATCH_POOL=0`.
    Sequential,
    /// Постоянный пул в ЭТОМ процессе: N постоянных Pocket, динамическая
    /// файловая очередь (`persistent.rs`).
    InProcessPool { workers: usize },
    /// Дочерние рабочие процессы с Pocket (CPU и CUDA; Named Pipes/Job
    /// Object/watchdog — гибридная инфраструктура G6.2): `children` ×
    /// `workers_per_child`; диспетчер держит не более `max_active_files`
    /// файлов одновременно.
    MultiProcess {
        children: usize,
        workers_per_child: usize,
        max_active_files: usize,
    },
}

/// G6.6: выбор топологии по конфигурации P/W/F/T, числу файлов-кандидатов
/// и ресурсам. Правила:
/// - `pool_allowed=false` (диагностический откат) или <2 кандидатов —
///   последовательный путь;
/// - `active = min(F, P×W, кандидатов)`; лишние Pocket НЕ создаются;
/// - P=1 или active ≤ W — однопроцессный пул на `active` Pocket (второй
///   процесс не нужен — пустые процессы не запускаются);
/// - иначе — мультипроцесс: P процессов × W Pocket (все Pocket создаются;
///   диспетчер ограничивает одновременные файлы значением F);
/// - CUDA: VRAM-префлайт по существующей методике; авто-снижение (профиль
///   «Оптимальный») — с явной записью; явный выбор («Производительный»/
///   «Свой») при нехватке VRAM в мультипроцессе откатывается на
///   экономичный однопроцессный пул с объяснением, при нехватке в пуле —
///   предупреждение без подмены.
fn decide_topology(
    settings: &GuiSettings,
    cfg: &ParallelSettings,
    actionable: usize,
    pool_allowed: bool,
    emit: &Arc<dyn Fn(JobEvent) + Send + Sync>,
) -> BatchTopology {
    if !pool_allowed {
        emit(JobEvent::Log(
            "ВНИМАНИЕ: SHTORM_GUI_BATCH_POOL=0 — диагностический откат на последовательный \
             путь (модель загружается на каждый файл)"
                .into(),
        ));
        return BatchTopology::Sequential;
    }
    if actionable < 2 {
        // Пул с одним файлом бессмыслен: чанк-параллелизм одиночного файла
        // (механизм A) быстрее файлового пула из одного Pocket.
        return BatchTopology::Sequential;
    }
    let pockets = cfg.total_pockets() as usize;
    let mut active = cfg
        .max_concurrent_files
        .min(pockets as u32)
        .min(actionable as u32)
        .max(1) as usize;
    if active < 1 {
        active = 1;
    }
    let w = cfg.workers_per_process as usize;
    if cfg.processes == 1 || active <= w {
        if cfg.processes > 1 {
            emit(JobEvent::Log(format!(
                "запрошено {} процессов, но одновременных файлов {active} ≤ {} воркеров на \
                 процесс — достаточно одного процесса без пустых дочерних процессов",
                cfg.processes, w
            )));
        }
        // CUDA: VRAM-префлайт однопроцессного пула
        if settings.device == DeviceChoice::Cuda {
            let free = vram_free_or_warn(settings, emit);
            if let Some(free) = free {
                let need =
                    active as f64 * BATCH_POOL_VRAM_PER_WORKER_MB + BATCH_POOL_VRAM_RESERVE_MB;
                if free < need {
                    if cfg.profile == ParallelProfile::Optimal && active > 1 {
                        emit(JobEvent::Log(format!(
                            "VRAM-префлайт: свободно {free:.0} МБ < {need:.0} МБ — профиль \
                             «Оптимальный»: 1 воркер (один постоянный Pocket)"
                        )));
                        active = 1;
                    } else {
                        emit(JobEvent::Log(format!(
                            "ВНИМАНИЕ: VRAM-префлайт: свободно {free:.0} МБ < {need:.0} МБ — \
                             конфигурация {active} Pocket может не загрузиться (ошибка загрузки \
                             завершит пакет с диагнозом)"
                        )));
                    }
                }
            }
        }
        return BatchTopology::InProcessPool { workers: active };
    }
    // мультипроцесс: все P×W Pocket создаются в дочерних процессах
    let children = cfg.processes as usize;
    let total_pockets = children * w;
    if settings.device == DeviceChoice::Cuda {
        if let Some(free) = vram_free_or_warn(settings, emit) {
            let need =
                total_pockets as f64 * BATCH_POOL_VRAM_PER_WORKER_MB + MULTIPROCESS_VRAM_RESERVE_MB;
            if free < need {
                let fallback = w.min(2);
                emit(JobEvent::Log(format!(
                    "ВНИМАНИЕ: запрошено {children} процессов × {w} Pocket (нужно ~{need:.0} МБ \
                     VRAM, свободно {free:.0} МБ) — конфигурация НЕ запускается, используется \
                     однопроцессный пул на {fallback} Pocket. Освободите VRAM и запустите пакет \
                     снова"
                )));
                return BatchTopology::InProcessPool { workers: fallback };
            }
            emit(JobEvent::Log(format!(
                "VRAM-префлайт {children}×{w}: свободно {free:.0} МБ ≥ {need:.0} МБ — запуск \
                 {total_pockets} Pocket ({children} процесса × {w} воркера), до {active} файлов \
                 одновременно"
            )));
        } else {
            emit(JobEvent::Log(
                "ВНИМАНИЕ: мультипроцессный CUDA: свободную VRAM проверить не удалось \
                 (nvidia-smi недоступен) — используется однопроцессный пул"
                    .into(),
            ));
            return BatchTopology::InProcessPool { workers: w.min(2) };
        }
    }
    BatchTopology::MultiProcess {
        children,
        workers_per_child: w,
        max_active_files: active,
    }
}

/// VRAM-префлайт; fake-режим (тесты IPC без GPU) пропускает проверку.
/// None — nvidia-smi недоступен (уже с журнальным сообщением).
fn vram_free_or_warn(
    settings: &GuiSettings,
    emit: &Arc<dyn Fn(JobEvent) + Send + Sync>,
) -> Option<f64> {
    if std::env::var("SHTORM_G62_FAKE").as_deref() == Ok("1") {
        emit(JobEvent::Log(
            "ВНИМАНИЕ: SHTORM_G62_FAKE=1 — движок-заглушка без ONNX (диагностика IPC); \
             VRAM-префлайт пропущен"
                .into(),
        ));
        return Some(f64::INFINITY);
    }
    let free = app_core::diagnostics::gpu_free_mb(settings.device_id);
    if free.is_none() {
        emit(JobEvent::Log(
            "ВНИМАНИЕ: свободную VRAM проверить не удалось (nvidia-smi недоступен)".into(),
        ));
    }
    free
}

/// G6.2/G6.6: пакет на мультипроцессном пуле — `children` дочерних рабочих
/// процессов × `workers_per_child` постоянных Pocket (CPU и CUDA; гибридная
/// инфраструктура G6.2: Named Pipes, Job Object, watchdog, безопасная
/// отмена). Диспетчер держит не более `max_active_files` файлов одновременно.
/// ONNX грузится ТОЛЬКО в дочерних процессах. События воркеров
/// транслируются в существующие JobEvent — UI-модель G6.1.1 не меняется.
#[allow(clippy::too_many_arguments)]
fn execute_batch_hybrid(
    settings: &GuiSettings,
    cfg: &ParallelSettings,
    children: usize,
    workers_per_child: usize,
    max_active_files: usize,
    plan: &BatchPlan,
    extension: &str,
    overwrite: bool,
    cancel: &Arc<AtomicBool>,
    emit: &Arc<dyn Fn(JobEvent) + Send + Sync>,
) -> Result<BatchReport> {
    let total = plan.files.len();

    // --- подготовка движка без ORT в этом процессе: только резолв модели ---
    let model_dir = resolve_model_dir(settings)?;
    emit(JobEvent::Log(format!("модель: {}", model_dir.display())));
    if let CharRulesLoad::Warning(w) = load_char_rules(&model_dir) {
        emit(JobEvent::Log(format!("ВНИМАНИЕ: {w}")));
    }
    let opts = pipeline_options(settings, model_dir.clone(), None);

    // --- предзапусковые проверки ДО процессов (те же, что у пула) ---
    let emit_batch = batch_event_forwarder(emit);
    let prepared = preflight_batch(plan, extension, overwrite, cancel, &|ev| emit_batch(ev));
    let mut reports = prepared.reports;
    let mut to_synth = Vec::with_capacity(prepared.to_synth.len());
    for pf in prepared.to_synth {
        match batch_file_prepare_paths(settings, &pf.out, &model_dir) {
            Ok(()) => to_synth.push(pf),
            Err(e) => {
                let file = &plan.files[pf.index];
                let r = FileReport::failed(file, Some(pf.out.clone()), format!("{e:#}"));
                emit_batch(BatchEvent::FileFinished {
                    index: pf.index + 1,
                    total,
                    report: Box::new(r.clone()),
                });
                reports[pf.index] = r;
            }
        }
    }
    if to_synth.is_empty() {
        return Ok(BatchReport {
            files: reports,
            conflicts: prepared.conflicts,
            cancelled: cancel.load(Ordering::SeqCst),
            pool_loads: None,
            pool_workers: None,
            pool_processes: None,
        });
    }

    // --- задания: per-file seed по тому же правилу, что пути G6.1 ---
    let jobs: Vec<HybridJob> = to_synth
        .iter()
        .map(|pf| HybridJob {
            pos: pf.index,
            input: pf.input.clone(),
            relative: pf.relative.clone(),
            out: pf.out.clone(),
            seed: opts.seed.unwrap_or_else(|| SeedPolicy::Random.resolve()),
        })
        .collect();
    let jobs_by_pos: std::collections::HashMap<usize, &HybridJob> =
        jobs.iter().map(|j| (j.pos, j)).collect();

    // --- ожидание событий UI ---
    // G6.2.1: BatchWorkersReady сообщается ОДИН раз, когда инициализация
    // УСТОЯЛАСЬ — с ФАКТИЧЕСКИМ числом Ready-воркеров. При частичном отказе
    // (E2E-D) это 3 из 4, а не номинальные 4: частично инициализированный
    // процесс нельзя считать полностью готовым.
    let init_settled = {
        let emit = Arc::clone(emit);
        move |n: usize| {
            if n > 0 {
                emit(JobEvent::Log(format!(
                    "мультипроцесс {children}×{workers_per_child}: {n} воркеров готовы к работе"
                )));
                emit(JobEvent::BatchWorkersReady { workers: n });
            } else {
                emit(JobEvent::Log(
                    "ВНИМАНИЕ: мультипроцесс: ни один воркер не сообщил Ready".into(),
                ));
            }
        }
    };
    let ui_event = {
        let emit = Arc::clone(emit);
        move |ev: &Ev| match ev {
            Ev::FileStarted { pos, .. } => {
                if let Some(j) = jobs_by_pos.get(pos) {
                    emit(JobEvent::FileStarted {
                        index: pos + 1,
                        total,
                        input: j.input.clone(),
                        output: j.out.clone(),
                    });
                    emit(JobEvent::Log(format!(
                        "[{}/{}] старт: {} → {}",
                        pos + 1,
                        total,
                        j.input.display(),
                        j.out.display()
                    )));
                }
            }
            Ev::FileProgress {
                pos,
                done,
                total: chunks,
                ..
            } => {
                emit(JobEvent::FileProgress {
                    index: pos + 1,
                    total,
                    done: *done,
                    chunks_total: *chunks,
                });
            }
            Ev::FilePhase { pos, phase, .. } => {
                let ph = match phase {
                    PhaseMsg::Generating => JobPhase::Generating,
                    PhaseMsg::Finalizing => JobPhase::Finalizing,
                    PhaseMsg::Publishing => JobPhase::Publishing,
                };
                emit(JobEvent::FilePhase {
                    index: pos + 1,
                    phase: ph,
                });
            }
            Ev::FileFinished { pos, report, .. } => {
                emit(JobEvent::Log(format!(
                    "[{}/{}] {}",
                    pos + 1,
                    total,
                    report.line()
                )));
                emit(JobEvent::FileDone {
                    index: pos + 1,
                    total,
                    report: Box::new(report.clone()),
                });
            }
            _ => {}
        }
    };

    // --- запуск дочерних процессов и диспетчеризация ---
    let log_emit = Arc::clone(emit);
    // G6.2.1 FAIL-CLOSED: без действующего KILL_ON_JOB_CLOSE 2×2 не стартует.
    // Проверка ДО первого процесса: CUDA-загрузок и сирот не возникает.
    let job_guard = ChildJobGuard::new();
    if let Err(e) = job_guard.ready() {
        let msg = format!(
            "мультипроцессный режим ({children}×{workers_per_child}) не запущен: защита \
             процессов Windows Job Object недоступна ({e:#}). Выберите однопроцессную \
             конфигурацию"
        );
        log_emit(JobEvent::Log(format!("ВНИМАНИЕ: {msg}")));
        for pf in &to_synth {
            let file = &plan.files[pf.index];
            let r = FileReport::failed(file, Some(pf.out.clone()), msg.clone());
            emit_batch(BatchEvent::FileFinished {
                index: pf.index + 1,
                total,
                report: Box::new(r.clone()),
            });
            reports[pf.index] = r;
        }
        return Ok(BatchReport {
            files: reports,
            conflicts: prepared.conflicts,
            cancelled: cancel.load(Ordering::SeqCst),
            pool_loads: Some(0),
            pool_workers: Some(0),
            pool_processes: Some(0),
        });
    }
    #[cfg(not(test))]
    let exe = std::env::current_exe()?;
    #[cfg(not(test))]
    let spawn = |ci: usize| hybrid::spawn_child_process(&job_guard, &exe, ci);
    // В lib-тестах current_exe — тестовый харнесс: дочерние worker-процессы
    // запускаются через его child-entry (та же реализация worker_main).
    #[cfg(test)]
    let spawn = |ci: usize| hybrid::spawn_child_via_test_harness(&job_guard, ci);
    let log = |m: String| log_emit(JobEvent::Log(m));
    let fake = std::env::var("SHTORM_G62_FAKE").as_deref() == Ok("1");
    // G6.6: ORT-потоки пакетной конфигурации (T) доходят до дочерних
    // процессов через GuiSettings (worker читает только IPC Init).
    let mut child_settings = settings.clone();
    child_settings.threads = cfg.ort_threads;
    let params = HybridParams {
        settings: child_settings,
        jobs: jobs.clone(),
        workers_per_child,
        children,
        fake,
        test: Vec::new(),
    };
    let driver = HybridDriver {
        params: &params,
        cancel: Arc::clone(cancel),
        spawn: &spawn,
        log: &log,
        event: &ui_event,
        init_settled: &init_settled,
        watchdog: hybrid::CHILD_WATCHDOG,
        startup_timeout: hybrid::CHILD_STARTUP_TIMEOUT,
        // G6.2.2: зависшая IPC-отправка не блокирует цикл диспетчера
        write_timeout: crate::ipc::WRITE_WAIT_TIMEOUT,
        // G6.6: предел одновременных файлов F
        max_active_files,
    };
    emit(JobEvent::Log(format!(
        "мультипроцесс {children}×{workers_per_child}: запуск {children} рабочих процессов \
         ({} Pocket на процесс; до {max_active_files} файлов одновременно; файлов к синтезу {})",
        workers_per_child,
        jobs.len()
    )));
    let t_spawn = Instant::now();
    let outcome = match hybrid::run_hybrid(&driver) {
        Ok(o) => o,
        Err(e) => {
            // ни один дочерний процесс не поднялся — файлы-кандидаты Failed
            let msg = format!("мультипроцессный пул не запущен: {e:#}");
            emit(JobEvent::Log(format!("ВНИМАНИЕ: {msg}")));
            for pf in &to_synth {
                let file = &plan.files[pf.index];
                let r = FileReport::failed(file, Some(pf.out.clone()), msg.clone());
                emit_batch(BatchEvent::FileFinished {
                    index: pf.index + 1,
                    total,
                    report: Box::new(r.clone()),
                });
                reports[pf.index] = r;
            }
            return Ok(BatchReport {
                files: reports,
                conflicts: prepared.conflicts,
                cancelled: false,
                pool_loads: Some(0),
                pool_workers: Some(0),
                pool_processes: Some(0),
            });
        }
    };
    emit(JobEvent::Log(format!(
        "мультипроцесс {children}×{workers_per_child}: процессы/воркеры готовы за {:.1} с; \
         Pocket загружен {} раз(а); процессов: {}",
        t_spawn.elapsed().as_secs_f32(),
        outcome.pocket_loads,
        outcome.children_spawned
    )));

    // --- сборка отчёта по позициям плана (исходный порядок) ---
    for (i, pf) in to_synth.iter().enumerate() {
        let mut rep = match outcome.reports.get(i).and_then(|r| r.clone()) {
            Some(r) => r,
            None => FileReport::failed(
                &plan.files[pf.index],
                Some(pf.out.clone()),
                "файл не получил результата (внутренняя ошибка диспетчера)",
            ),
        };
        // заполнить поля у placeholder-отчётов (Failed диспетчера)
        if rep.input.as_os_str().is_empty() {
            rep.input = pf.input.clone();
            rep.relative = pf.relative.clone();
            rep.output = Some(pf.out.clone());
        }
        reports[pf.index] = rep;
    }

    let cancelled =
        cancel.load(Ordering::SeqCst) || reports.iter().any(|r| r.status == FileStatus::Cancelled);
    let mut report = BatchReport {
        files: reports,
        conflicts: prepared.conflicts,
        cancelled,
        pool_loads: Some(outcome.pocket_loads),
        // G6.2.1: фактическое число готовых воркеров, без «дотягивания» до 2.
        pool_workers: Some(outcome.workers_ready),
        pool_processes: Some(outcome.children_spawned),
    };
    if report.pool_workers == Some(0) {
        report.pool_workers = None;
    }
    Ok(report)
}

/// Общий переводчик BatchEvent → JobEvent (последовательный путь, пул,
/// гибрид): журнал K/N, per-file прогресс/фазы и FileDone.
fn batch_event_forwarder(
    emit: &Arc<dyn Fn(JobEvent) + Send + Sync>,
) -> Arc<dyn Fn(BatchEvent) + Send + Sync> {
    let emit = Arc::clone(emit);
    Arc::new(move |ev: BatchEvent| match ev {
        BatchEvent::Planned { total, warnings } => {
            emit(JobEvent::Log(format!(
                "пакет: план зафиксирован — {total} файлов, предупреждений: {}",
                warnings.len()
            )));
        }
        BatchEvent::FileStarted {
            index,
            total,
            input,
            output,
        } => {
            let msg = format!(
                "[{index}/{total}] старт: {} → {}",
                input.display(),
                output.display()
            );
            emit(JobEvent::FileStarted {
                index,
                total,
                input,
                output,
            });
            emit(JobEvent::Log(msg));
        }
        BatchEvent::FileProgress {
            index,
            total,
            done,
            chunks_total,
        } => {
            emit(JobEvent::FileProgress {
                index,
                total,
                done,
                chunks_total,
            });
        }
        BatchEvent::FileFinished {
            index,
            total,
            report,
        } => {
            emit(JobEvent::Log(format!(
                "[{index}/{total}] {}",
                report.line()
            )));
            emit(JobEvent::FileDone {
                index,
                total,
                report,
            });
        }
        BatchEvent::Log(line) => emit(JobEvent::Log(line)),
    })
}

/// G5.1-подготовка пути результата одного файла пакета (в последовательном
/// пути её делает execute_job; в пуле — ДО запуска воркеров, чтобы конфликт
/// путей не всплывал посреди параллельного прогона): проверка безопасности
/// ДО создания каталогов, создание без следования по reparse-точкам,
/// повторная валидация ПОСЛЕ создания.
fn batch_file_prepare_paths(s: &GuiSettings, out_path: &Path, model_dir: &Path) -> Result<()> {
    ensure_output_path_safe(s, out_path, model_dir)?;
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            create_dir_all_no_reparse(parent)?;
        }
    }
    ensure_output_path_safe(s, out_path, model_dir)
}

/// G6.1/G6.6: пакет на ПОСТОЯННОМ пуле воркеров (CPU и CUDA). Pocket
/// загружается ровно один раз на воркера (на весь пакет); каждый файл
/// целиком обрабатывает один воркер; файлы распределяет общая динамическая
/// очередь. `workers` — фактическое число Pocket (решение `decide_topology`,
/// лишние Pocket не создаются); ORT-потоки — из конфигурации G6.6.
#[allow(clippy::too_many_arguments)]
fn execute_batch_persistent_pool(
    settings: &GuiSettings,
    cfg: &ParallelSettings,
    workers: usize,
    plan: &BatchPlan,
    extension: &str,
    overwrite: bool,
    cancel: &Arc<AtomicBool>,
    emit: &Arc<dyn Fn(JobEvent) + Send + Sync>,
) -> Result<BatchReport> {
    let total = plan.files.len();

    // --- подготовка движка: один раз на весь пакет ---
    let model_dir = resolve_model_dir(settings)?;
    emit(JobEvent::Log(format!("модель: {}", model_dir.display())));
    let mut char_rules = None;
    match load_char_rules(&model_dir) {
        CharRulesLoad::Absent => {}
        CharRulesLoad::Loaded(rules) => char_rules = Some(rules),
        CharRulesLoad::Warning(w) => {
            // невалидный/неполный runtime.json не игнорируем молча
            emit(JobEvent::Log(format!("ВНИМАНИЕ: {w}")));
        }
    }
    setup_ort_environment(settings)?;

    // --- предзапусковые проверки ДО запуска воркеров ---
    let emit_batch: Arc<dyn Fn(BatchEvent) + Send + Sync> = {
        let emit = Arc::clone(emit);
        Arc::new(move |ev: BatchEvent| match ev {
            BatchEvent::Planned { total, warnings } => {
                emit(JobEvent::Log(format!(
                    "пакет: план зафиксирован — {total} файлов, предупреждений: {}",
                    warnings.len()
                )));
            }
            BatchEvent::FileStarted {
                index,
                total,
                input,
                output,
            } => {
                let msg = format!(
                    "[{index}/{total}] старт: {} → {}",
                    input.display(),
                    output.display()
                );
                emit(JobEvent::FileStarted {
                    index,
                    total,
                    input,
                    output,
                });
                emit(JobEvent::Log(msg));
            }
            BatchEvent::FileProgress {
                index,
                total,
                done,
                chunks_total,
            } => {
                emit(JobEvent::FileProgress {
                    index,
                    total,
                    done,
                    chunks_total,
                });
            }
            BatchEvent::FileFinished {
                index,
                total,
                report,
            } => {
                emit(JobEvent::Log(format!(
                    "[{index}/{total}] {}",
                    report.line()
                )));
                emit(JobEvent::FileDone {
                    index,
                    total,
                    report,
                });
            }
            BatchEvent::Log(line) => emit(JobEvent::Log(line)),
        })
    };
    let prepared = preflight_batch(plan, extension, overwrite, cancel, &|ev| emit_batch(ev));
    let mut reports = prepared.reports;

    // подготовка путей результатов (reparse/каталоги) — тоже до воркеров
    let mut to_synth = Vec::with_capacity(prepared.to_synth.len());
    for pf in prepared.to_synth {
        match batch_file_prepare_paths(settings, &pf.out, &model_dir) {
            Ok(()) => to_synth.push(pf),
            Err(e) => {
                let file = &plan.files[pf.index];
                let r = FileReport::failed(file, Some(pf.out.clone()), format!("{e:#}"));
                emit_batch(BatchEvent::FileFinished {
                    index: pf.index + 1,
                    total,
                    report: Box::new(r.clone()),
                });
                reports[pf.index] = r;
            }
        }
    }
    if to_synth.is_empty() {
        return Ok(BatchReport {
            files: reports,
            conflicts: prepared.conflicts,
            cancelled: cancel.load(Ordering::SeqCst),
            pool_loads: None,
            pool_workers: None,
            pool_processes: None,
        });
    }

    // --- загрузка пула: Pocket один раз на воркера, параллельно ---
    // G6.6: ORT-потоки пакетной конфигурации (T) подставляются в опции
    // конвейера; настройка `threads` одиночного режима не затрагивается.
    let mut pool_settings = settings.clone();
    pool_settings.threads = cfg.ort_threads;
    let opts = pipeline_options(&pool_settings, model_dir, char_rules);
    let spawn_seed = opts.seed.unwrap_or(42);
    emit(JobEvent::Log(format!(
        "постоянный пул: загрузка {workers} Pocket…"
    )));
    let t_pool = Instant::now();
    let spawned = spawn_workers(workers, real_worker_factory(&opts, spawn_seed), spawn_seed);
    if !spawned.failures.is_empty() {
        // частично созданные Pocket освобождаются drop'ом слотов; повторов
        // загрузки нет; пакет в неопределённом состоянии НЕ начинается —
        // файлы получают Failed с диагнозом (готовых результатов нет)
        let msg = spawned
            .failures
            .iter()
            .map(|(w, e)| format!("воркер {w}: {e}"))
            .collect::<Vec<_>>()
            .join("; ");
        emit(JobEvent::Log(format!("ВНИМАНИЕ: пул не запущен: {msg}")));
        let mut conflicts = prepared.conflicts;
        for pf in &to_synth {
            let file = &plan.files[pf.index];
            let r =
                FileReport::failed(file, Some(pf.out.clone()), format!("пул не запущен: {msg}"));
            emit_batch(BatchEvent::FileFinished {
                index: pf.index + 1,
                total,
                report: Box::new(r.clone()),
            });
            reports[pf.index] = r;
        }
        return Ok(BatchReport {
            files: reports,
            conflicts: std::mem::take(&mut conflicts),
            cancelled: false,
            pool_loads: Some(spawned.loaded),
            pool_workers: Some(workers),
            pool_processes: None,
        });
    }
    let load_ms: Vec<_> = spawned.load_ms.iter().map(|ms| format!("{ms}")).collect();
    emit(JobEvent::Log(format!(
        "постоянный пул: {workers} воркер(ов) готовы за {:.1} с (загрузки: {} мс); Pocket \
         загружен {workers} раз(а); файлов к синтезу: {}",
        t_pool.elapsed().as_secs_f32(),
        load_ms.join("/"),
        to_synth.len()
    )));
    // G6.1.1: пул загружен — сообщаем число реально работающих воркеров.
    // Раньше сюда помещался второй BatchStarted, который обнулял счётчики
    // после preflight FileDone (дефект G6.1.1 №1).
    emit(JobEvent::BatchWorkersReady { workers });

    // --- параллельный прогон на пуле ---
    let settings_c = settings.clone();
    let opts_c = opts.clone();
    let cancel_c = Arc::clone(cancel);
    let emit_c = Arc::clone(emit);
    let synth = move |pf: &PreparedFile, _wid: usize, slot: &WorkerSlot<RealWorker>| {
        execute_batch_file_persistent(&settings_c, &opts_c, pf, total, &cancel_c, &emit_c, slot)
    };
    let mut report = run_prepared_parallel(
        plan,
        PreparedBatch {
            reports,
            to_synth,
            conflicts: prepared.conflicts,
        },
        cancel,
        spawned.slots,
        &emit_batch,
        synth,
    );
    report.pool_loads = Some(spawned.loaded);
    report.pool_workers = Some(workers);
    Ok(report)
}

/// Один файл пакета на ПОСТОЯННОМ воркере (выполняется в потоке воркера
/// пула). Тот же конвейер, что `execute_job` (без per-файловой подготовки
/// движка/путей — она сделана до запуска воркеров), но:
/// - файл целиком идёт через один loan-воркер (`workers = 1`);
/// - per-file seed устанавливается на постоянный Pocket фабрикой одалживания;
/// - ошибка движка ОТРАВЛЯЕТ слот (Pocket не переиспользуется), ошибка
///   публикации — нет (кодирование прошло успешно, Pocket исправен).
#[allow(clippy::too_many_arguments)]
fn execute_batch_file_persistent(
    s: &GuiSettings,
    opts: &PipelineOptions,
    pf: &PreparedFile,
    total_files: usize,
    cancel: &Arc<AtomicBool>,
    emit: &Arc<dyn Fn(JobEvent) + Send + Sync>,
    slot: &WorkerSlot<RealWorker>,
) -> FileReport {
    let t0 = Instant::now();
    let k = pf.index + 1;
    // чтение текста — ошибка файла, движок не при чём: Pocket не трогаем
    let text = match std::fs::read_to_string(&pf.input) {
        Ok(t) => t,
        Err(e) => {
            return FileReport::failed(
                &batch_file_of(pf),
                Some(pf.out.clone()),
                format!("прочитать {}: {e}", pf.input.display()),
            )
        }
    };
    let cfg = JobConfig {
        settings: s.clone(),
        text,
        out_path: pf.out.clone(),
    };
    let existed_before = pf.out.exists();
    let ffmpeg_path = if s.ffmpeg_path.trim().is_empty() {
        None
    } else {
        Some(resources::resolve_user_path(&s.ffmpeg_path))
    };
    let preset = s.preset_for_format();
    let overwrite = s.overwrite_existing;
    // создание sink — тоже до движка: ошибка ffmpeg/пути не отравляет Pocket
    let sink = match make_sink_with_overwrite(
        &pf.out,
        24_000,
        output_format(s.format),
        preset,
        ffmpeg_path.as_deref(),
        overwrite,
    ) {
        Ok(sink) => sink,
        Err(e) => {
            return FileReport::failed(&batch_file_of(pf), Some(pf.out.clone()), format!("{e:#}"))
        }
    };
    emit(JobEvent::Log(format!(
        "[{k}/{total_files}] запуск: {} | {} | постоянный пул | {} | {}",
        pf.out.display(),
        device_label(s),
        s.format.label(),
        if preset.is_empty() { "-" } else { preset }
    )));

    let result = synthesize_stream_with_worker_factory(
        &cfg.text,
        opts,
        sink,
        |done, total| {
            emit(JobEvent::FileProgress {
                index: k,
                total: total_files,
                done,
                chunks_total: total,
            });
        },
        cancel,
        &|ph| {
            emit(JobEvent::FilePhase {
                index: k,
                phase: ph.into(),
            })
        },
        loaned_real_worker_factory(Arc::clone(slot)),
    );
    let elapsed = t0.elapsed().as_secs_f32();

    let (res, engine_broken) = match result {
        Ok(outcome) => {
            // Ok = Completed или Cancelled; отмена прерывала файл посреди
            // генерации — defensive-отравление (пакет всё равно завершается)
            let broken = outcome.status == RunStatus::Cancelled;
            (
                finish_from_outcome(&cfg, outcome, elapsed, cancel, overwrite, existed_before),
                broken,
            )
        }
        Err(e) => {
            let publish_failed = e
                .downcast_ref::<StreamError>()
                .map(|se| se.publish_failed)
                .unwrap_or(false);
            (finish_from_error(&cfg, &e, elapsed), !publish_failed)
        }
    };
    if engine_broken {
        // Pocket в неопределённом состоянии — освободить и не переиспользовать
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
        emit(JobEvent::Log(format!(
            "[{k}/{total_files}] ВНИМАНИЕ: Pocket этого воркера после сбоя не \
             переиспользуется (слот отравлен)"
        )));
    }
    file_report_from_job_result(&pf.input, &pf.relative, &pf.out, res)
}

/// Временный BatchFile-вид PreparedFile для конструкторов FileReport
/// (вход/относительный путь совпадают с планом).
fn batch_file_of(pf: &PreparedFile) -> BatchFile {
    BatchFile {
        input: pf.input.clone(),
        relative: pf.relative.clone(),
    }
}

/// Преобразование JobResult одного файла в FileReport пакета — единая
/// семантика статусов/причин последовательного пути и постоянного пула.
pub(crate) fn file_report_from_job_result(
    input: &Path,
    relative: &Path,
    out_path: &Path,
    res: JobResult,
) -> FileReport {
    let mut rep = FileReport {
        input: input.to_path_buf(),
        relative: relative.to_path_buf(),
        output: Some(out_path.to_path_buf()),
        status: FileStatus::Skipped,
        reason: None,
        final_path: res.final_path.clone(),
        partial_path: res.partial_path.clone(),
        diag_path: res.diag_path.clone(),
        chunk_log_path: res.chunk_log_path.clone(),
        chunks_done: res.progress.0,
        chunks_total: res.progress.1,
    };
    match res.status {
        JobStatus::Completed => {
            rep.status = FileStatus::Completed;
            // G5.1: «обновлён» только для реально существовавшего файла,
            // новый файл не называется «обновлённым».
            rep.reason = Some(if res.replaced_existing {
                "файл обновлён (старый заменён)".into()
            } else {
                "создан новый файл".into()
            });
        }
        JobStatus::Cancelled => {
            rep.status = FileStatus::Cancelled;
            rep.reason = Some(res.summary);
        }
        JobStatus::Failed => {
            rep.status = FileStatus::Failed;
            rep.reason = Some(res.summary);
        }
        // для одного файла эти статусы не возникают; не маскируем
        // неожиданное состояние под успех
        JobStatus::Partial => {
            rep.status = FileStatus::Failed;
            rep.reason = Some(format!("частичный результат: {}", res.summary));
        }
        JobStatus::NoWork | JobStatus::Skipped => {
            rep.status = FileStatus::Skipped;
            rep.reason = Some(res.summary);
        }
    }
    rep
}

/// Явное предупреждение (например, невалидный runtime.json) — в начало
/// итоговой сводки, чтобы пользователь увидел его, а не только в журнале.
fn attach_warning(mut r: JobResult, warning: &Option<String>) -> JobResult {
    if let Some(w) = warning {
        r.summary = format!("{w}; {}", r.summary);
    }
    r
}

/// Успешный возврат synthesize_stream: Completed (итог + RTF + лог чанков)
/// или Cancelled (черновик + диагностический журнал, итог не публикуется).
/// `existed_before` — существовал ли готовый файл до запуска: отделяет
/// «создан новый файл» от «обновлён существующий» и в отмене уточняет,
/// что старый файл сохранён.
pub(crate) fn finish_from_outcome(
    cfg: &JobConfig,
    o: app_core::pipeline::StreamOutcome,
    elapsed: f32,
    cancel: &AtomicBool,
    overwrite: bool,
    existed_before: bool,
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
        // G5.1: при отмене до точки решения старый готовый файл гарантированно
        // не тронут — говорим об этом явно, а не «итоговый файл не создан».
        let outcome_note = if existed_before && overwrite {
            "замена НЕ выполнена — старый файл сохранён побайтово"
        } else {
            "итоговый файл не создан"
        };
        let diag = write_diag_log(
            &cfg.out_path,
            "Cancelled",
            &format!(
                "{reason}; {outcome_note}; черновик сохранён: {}",
                o.partial_path.display()
            ),
            &DiagMeta::from_outcome(&o),
        );
        return JobResult {
            status: JobStatus::Cancelled,
            summary: format!(
                "ОТМЕНЕНО на {}/{} чанках; {outcome_note}; черновик: {}",
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
            replaced_existing: false,
            batch: None,
        };
    }
    // Completed: RTF/паузы/денойз в сводку; лог чанков опционален (сбой
    // журнала не маскирует успешное аудио). G5.1: «обновлён» — только когда
    // готовый файл реально существовал и был заменён; иначе — новый файл.
    let speech_s = o.speech_samples as f64 / o.sample_rate as f64;
    let (prefix, replaced) = if existed_before {
        ("ФАЙЛ ОБНОВЛЁН (старый заменён)", true)
    } else {
        ("ГОТОВО (создан новый файл)", false)
    };
    let mut summary = format!(
        "{prefix}: {} ({:.2} с: речь {:.2} с + паузы {:.2} с; чанков {}; RTF {:.3})",
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
            preset: cfg.settings.preset_for_format().to_string(),
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
        replaced_existing: replaced,
        batch: None,
    }
}

/// Ошибка synthesize_stream: типизированный StreamError (прогресс/seed/
/// черновик уже внутри) + диагностический журнал рядом с итогом.
/// G5.1: сбой собственно публикации (кодирование прошло успешно) выделен
/// отдельным статусом сообщения — старый готовый файл не изменён.
pub(crate) fn finish_from_error(cfg: &JobConfig, e: &anyhow::Error, elapsed: f32) -> JobResult {
    let _ = elapsed;
    if let Some(se) = e.downcast_ref::<StreamError>() {
        let diag = write_diag_log(
            &cfg.out_path,
            "Failed",
            &format!("{}\nпричина: ошибка выполнения задания", se.message),
            &DiagMeta::from_error(se),
        );
        let summary = if se.publish_failed {
            format!("ОШИБКА ПУБЛИКАЦИИ: {se} — старый файл не изменён, черновик цел")
        } else {
            format!("ОШИБКА: {se}")
        };
        return JobResult {
            status: JobStatus::Failed,
            summary,
            final_path: None,
            partial_path: Some(se.partial.clone()),
            diag_path: diag,
            chunk_log_path: None,
            rtf: None,
            progress: (se.chunks_done, se.total),
            replaced_existing: false,
            batch: None,
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
        replaced_existing: false,
        batch: None,
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
            eos_threshold: crate::settings::EosThresholdSetting::Fixed(-3.5),
            eos_tail_short: shtorm_engine::EosTail::Fixed(7),
            eos_tail_long: shtorm_engine::EosTail::Fixed(0),
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
        assert_eq!(o.eos_tail_short, shtorm_engine::EosTail::Fixed(7));
        assert_eq!(o.eos_tail_long, shtorm_engine::EosTail::Fixed(0));
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

    /// G4.1: сквозная цепочка GUI → PipelineOptions → Pocket. Дефолтные
    /// настройки («Авто», 5/3) разрешаются в точные значения teratts-android
    /// −4.0/5/3 — те же числа, что и дефолты CLI (паритет значений).
    #[test]
    fn eos_defaults_flow_gui_to_pipeline() {
        let s = GuiSettings::default();
        let o = pipeline_options(&s, PathBuf::from(r"C:\m"), None);
        assert_eq!(
            o.eos_threshold, -4.0,
            "«Авто» == −4.0 (встроенное значение)"
        );
        assert_eq!(o.eos_tail_short, shtorm_engine::EosTail::Fixed(5));
        assert_eq!(o.eos_tail_long, shtorm_engine::EosTail::Fixed(3));
        // движок получает то же самое: снимок параметров воркера (worker.rs,
        // mirror-тест engine_params) снимается ровно с этих PipelineOptions.
        let params = app_core::worker::engine_params(&o);
        assert_eq!(params.eos_threshold, -4.0);
        assert_eq!(params.eos_tail_short, shtorm_engine::EosTail::Fixed(5));
        assert_eq!(params.eos_tail_long, shtorm_engine::EosTail::Fixed(3));
    }

    /// G4.1: EOS-параметры НИКОГДА не читаются из runtime.json модели —
    /// оттуда берутся только char_rules. EOS-ключи в файле игнорируются
    /// молча и не влияют на PipelineOptions.
    #[test]
    fn eos_never_read_from_runtime_json() {
        let d = tmpdir("eos_runtime");
        let model = fake_model(&d, "ru-v2-slow");
        // runtime.json с «соблазнительными» EOS-ключами и валидными char_rules
        std::fs::write(
            model.join("runtime.json"),
            r#"{"eos_threshold": 9.9, "eos_tail": 77, "frames_after_eos": 6,
                "char_rules": {"char_map": {"ё": "е"}}}"#,
        )
        .unwrap();
        let s = GuiSettings {
            eos_threshold: crate::settings::EosThresholdSetting::Fixed(0.5),
            eos_tail_short: shtorm_engine::EosTail::Fixed(2),
            eos_tail_long: shtorm_engine::EosTail::Default,
            ..Default::default()
        };
        let rules = match load_char_rules(&model) {
            CharRulesLoad::Loaded(r) => r,
            other => panic!("char_rules должны загрузиться: {other:?}"),
        };
        assert_eq!(rules.char_map.len(), 1, "char_rules читаются как раньше");
        let o = pipeline_options(&s, model, Some(rules));
        assert_eq!(o.eos_threshold, 0.5, "порог — из GUI, не из runtime.json");
        assert_eq!(o.eos_tail_short, shtorm_engine::EosTail::Fixed(2));
        assert_eq!(o.eos_tail_long, shtorm_engine::EosTail::Default);
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

    /// G6.3: INT8-варианты резолвятся в те же каталоги ru-v2-fast/slow;
    /// отсутствие ЛЮБОГО из трёх INT8-графов — явная ошибка с именем файла
    /// (молчаливого отката на FP32 нет); полный набор — успех.
    #[test]
    fn resolve_model_dir_int8_variants() {
        let base = tmpdir("models_int8");
        let fast = fake_model(&base, "ru-v2-fast");
        let slow = fake_model(&base, "ru-v2-slow");
        let int8_files = [
            "flow_lm_main_int8.onnx",
            "flow_lm_flow_int8.onnx",
            "mimi_decoder_int8.onnx",
        ];
        for f in int8_files {
            std::fs::write(fast.join(f), b"fake").unwrap();
            std::fs::write(slow.join(f), b"fake").unwrap();
        }
        let mut s = GuiSettings {
            onnx_base: base.display().to_string(),
            ..Default::default()
        };
        s.model = ModelChoice::FastInt8;
        assert_eq!(resolve_model_dir(&s).unwrap(), fast);
        s.model = ModelChoice::SlowInt8;
        assert_eq!(resolve_model_dir(&s).unwrap(), slow);
        // убираем каждый из трёх по очереди — ошибка называет файл
        // (каждая итерация — свой чистый каталог, тесты файлы не удаляют)
        for (i, missing) in int8_files.iter().enumerate() {
            let probe = tmpdir(&format!("models_int8_probe_{i}"));
            let d = probe.join("ru-v2-slow");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("flow_lm_main.onnx"), b"fake").unwrap();
            for f in int8_files {
                if f != *missing {
                    std::fs::write(d.join(f), b"fake").unwrap();
                }
            }
            let s = GuiSettings {
                model: ModelChoice::SlowInt8,
                onnx_base: probe.display().to_string(),
                ..Default::default()
            };
            let err = resolve_model_dir(&s).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains(missing), "нет имени {missing}: {msg}");
            assert!(msg.contains("fallback"), "нет запрета fallback: {msg}");
        }
    }

    /// G6.3: точность из выбранного варианта модели переносится в
    /// PipelineOptions (FP32-варианты — Fp32, INT8-варианты — Int8).
    #[test]
    fn pipeline_options_carries_precision() {
        for (choice, precision) in [
            (ModelChoice::Fast, shtorm_engine::ModelPrecision::Fp32),
            (ModelChoice::Slow, shtorm_engine::ModelPrecision::Fp32),
            (ModelChoice::FastInt8, shtorm_engine::ModelPrecision::Int8),
            (ModelChoice::SlowInt8, shtorm_engine::ModelPrecision::Int8),
            (ModelChoice::Custom, shtorm_engine::ModelPrecision::Fp32),
        ] {
            let s = GuiSettings {
                model: choice,
                ..Default::default()
            };
            assert_eq!(
                pipeline_options(&s, PathBuf::from(r"C:\m"), None).precision,
                precision,
                "{choice:?}"
            );
        }
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

    /// char_rules читаются из runtime.json модели; отсутствие файла — Absent,
    /// невалидный JSON/секция — явное Warning (не молчаливое игнорирование).
    #[test]
    fn char_rules_from_runtime_json() {
        let d = tmpdir("crules");
        assert!(
            matches!(load_char_rules(&d), CharRulesLoad::Absent),
            "нет файла — штатное отсутствие правил"
        );
        std::fs::write(d.join("runtime.json"), r#"{"other": 1}"#).unwrap();
        assert!(
            matches!(load_char_rules(&d), CharRulesLoad::Warning(_)),
            "нет секции — предупреждение"
        );
        std::fs::write(d.join("runtime.json"), b"{not json").unwrap();
        let w = match load_char_rules(&d) {
            CharRulesLoad::Warning(w) => w,
            other => panic!("ожидалось Warning, получено {other:?}"),
        };
        assert!(w.contains("невалиден"), "{w}");
        std::fs::write(
            d.join("runtime.json"),
            r#"{"char_rules": {"char_map": {"ё": "е"}}}"#,
        )
        .unwrap();
        let rules = match load_char_rules(&d) {
            CharRulesLoad::Loaded(r) => r,
            other => panic!("ожидалось Loaded, получено {other:?}"),
        };
        assert_eq!(
            rules.char_map.get("ё").and_then(|v| v.as_str()),
            Some("е"),
            "правила разобраны: {:?}",
            rules.char_map
        );
    }

    /// Инициализация ORT: тот же runtime — no-op; другой DLL — ошибка. Тест
    /// не загружает настоящий ORT (чистая проверка решения).
    #[test]
    fn ort_init_is_idempotent_and_rejects_switch() {
        let dll = Path::new(r"C:\deps\portable\onnxruntime.dll");
        assert!(ort_init_needed(dll, None).unwrap(), "первый запуск");
        assert!(
            !ort_init_needed(dll, Some(dll)).unwrap(),
            "повторное задание на том же runtime — без переинициализации"
        );
        // регистр и хвостовые разделители не создают ложную «смену runtime»
        assert!(
            !ort_init_needed(Path::new(r"c:\DEPS\portable\onnxruntime.dll"), Some(dll)).unwrap()
        );
        let other = Path::new(r"C:\deps\ort-cpu\onnxruntime.dll");
        let err = ort_init_needed(other, Some(dll)).unwrap_err();
        assert!(format!("{err:#}").contains("перезапустите"), "{err:#}");
    }

    /// PATH не раздувается: каталоги CUDA добавляются один раз, повторное
    /// слияние уже присутствующих путей ничего не меняет.
    #[test]
    fn path_prepend_is_deduped_and_idempotent() {
        let old = std::env::join_paths([Path::new("a"), Path::new("cuda")]).unwrap();
        let prepend = vec![PathBuf::from("cuda"), PathBuf::from("b")];
        let merged = merge_path_prepend(&old, &prepend).expect("b добавляется");
        let parts: Vec<String> = std::env::split_paths(&merged)
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert_eq!(parts, vec!["b", "a", "cuda"], "cuda не дублируется");
        assert!(
            merge_path_prepend(&merged, &prepend).is_none(),
            "повторное применение — no-op"
        );
        assert!(
            merge_path_prepend(&old, &prepend[..1]).is_none(),
            "уже присутствующий каталог не добавляется"
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
        assert!(
            res.summary.contains("flow_lm_main.onnx") || res.summary.contains("onnx_base"),
            "{}",
            res.summary
        );
        assert!(res.partial_path.is_none());
        assert!(res.diag_path.is_none());
        assert!(!cfg.out_path.exists());

        // G5: существующий итог при выключенной перезаписи — не ошибка, а
        // пропуск; содержимое файла остаётся побайтово неизменным.
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
        assert_eq!(res.status, JobStatus::Skipped);
        assert!(res.summary.contains("уже существует"), "{}", res.summary);
        assert_eq!(std::fs::read(&cfg.out_path).unwrap(), b"keep");
    }

    // ===== G5.1: порядок проверок путей (песочницы %TEMP%, без удалений) =====

    #[cfg(windows)]
    fn make_junction(target: &Path, link: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// G5.1: create_dir_all_no_reparse создаёт обычную вложенность, но
    /// отказывает на junction-компоненте, НЕ создавая ничего «по ту сторону»
    /// ссылки (create_dir_all прошёл бы молча).
    #[cfg(windows)]
    #[test]
    fn create_dirs_refuses_junction_and_creates_clean_paths() {
        let d = tmpdir("mkdirs");
        // обычная вложенность — создаётся
        let clean = d.join("out").join("book").join("deep");
        create_dir_all_no_reparse(&clean).unwrap();
        assert!(clean.is_dir());
        // junction в середине пути — отказ, цель не тронута
        let real = d.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = d.join("alias");
        if !make_junction(&real, &link) {
            eprintln!("SKIP create_dirs_refuses_junction: нет прав на junction");
            return;
        }
        let through = link.join("newdir").join("deeper");
        let err = create_dir_all_no_reparse(&through).unwrap_err();
        assert!(format!("{err:#}").contains("перенаправления"), "{err:#}");
        assert!(
            !real.join("newdir").exists(),
            "по ту сторону junction ничего не создано"
        );
        // файл на месте компонента-каталога — отказ
        let blocker = d.join("blocker");
        std::fs::write(&blocker, b"f").unwrap();
        let err = create_dir_all_no_reparse(&blocker.join("sub")).unwrap_err();
        assert!(format!("{err:#}").contains("не каталог"), "{err:#}");
    }

    /// G5.1: полный путь execute_job — проверка безопасности строго ДО
    /// создания каталогов. Junction в родителе результата + отсутствующие
    /// вложенные каталоги: задание отказывает, по ту сторону ссылки ничего
    /// не создаётся (прежний порядок создал бы real/newdir/ до проверки).
    #[cfg(windows)]
    #[test]
    fn execute_job_checks_junction_before_creating_dirs() {
        let d = tmpdir("job_junction");
        let base = tmpdir("job_junction_models");
        fake_model(&base, "ru-v2-slow");
        let real = d.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = d.join("alias");
        if !make_junction(&real, &link) {
            eprintln!(
                "SKIP execute_job_checks_junction_before_creating_dirs: нет прав на junction"
            );
            return;
        }
        let text_file = d.join("book.txt");
        std::fs::write(&text_file, "Текст.").unwrap();
        let out_dir = link.join("newdir");
        let s = GuiSettings {
            onnx_base: base.display().to_string(),
            text_file: text_file.display().to_string(),
            out_dir: out_dir.display().to_string(),
            ..Default::default()
        };
        let cfg = JobConfig {
            settings: s,
            text: "Текст.".into(),
            out_path: out_dir.join("book.wav"),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let res = execute_job(&cfg, &cancel, &|_| {});
        assert_eq!(res.status, JobStatus::Failed, "{}", res.summary);
        assert!(
            res.summary.contains("перенаправления"),
            "ожидается отказ по reparse-точке: {}",
            res.summary
        );
        assert!(
            !real.join("newdir").exists(),
            "каталоги НЕ созданы до проверки безопасности"
        );
    }

    /// G5.1: ensure_output_path_safe проверяет каталог модели по РАЗРЕШЁННОМУ
    /// пути (ошибка резолва не может молча пропустить проверку — параметр).
    #[test]
    fn output_path_inside_model_dir_is_rejected() {
        let d = tmpdir("model_overlap");
        let model = fake_model(&d, "ru-v2-slow");
        let text_file = d.join("book.txt");
        std::fs::write(&text_file, "Текст.").unwrap();
        let s = GuiSettings {
            onnx_base: d.display().to_string(),
            text_file: text_file.display().to_string(),
            out_dir: model.display().to_string(),
            ..Default::default()
        };
        let out = model.join("book.wav");
        let err = ensure_output_path_safe(&s, &out, &model).unwrap_err();
        assert!(
            format!("{err:#}").contains("внутри каталога модели"),
            "{err:#}"
        );
        // выход вне каталога модели — норма
        let ok_dir = d.join("out");
        let s = GuiSettings {
            out_dir: ok_dir.display().to_string(),
            ..s
        };
        ensure_output_path_safe(&s, &ok_dir.join("book.wav"), &model).unwrap();
    }

    /// Пустой пакет: не «Завершено», а NoWork — синтез не выполнялся.
    #[test]
    fn empty_batch_is_no_work() {
        let d = tmpdir("empty_batch");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let s = GuiSettings {
            out_dir: d.join("out").display().to_string(),
            ..Default::default()
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
        let res = execute_batch(&s, &src, &cancel, &emit);
        assert_eq!(res.status, JobStatus::NoWork, "{}", res.summary);
        assert!(res.summary.contains("не найдено"), "{}", res.summary);
        assert!(res.summary.contains("НЕЧЕГО ДЕЛАТЬ"), "{}", res.summary);
        assert_eq!(res.batch.expect("отчёт").total(), 0);
    }

    /// Все файлы пропущены (результаты уже есть): тоже NoWork, без
    /// полноценного «успешного синтеза».
    #[test]
    fn all_skipped_batch_is_no_work() {
        let d = tmpdir("skipped_batch");
        let src = d.join("src");
        let out = d.join("out");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(src.join("a.txt"), "Текст.").unwrap();
        std::fs::write(out.join("a.wav"), b"keep").unwrap();
        let s = GuiSettings {
            out_dir: out.display().to_string(),
            ..Default::default()
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
        let res = execute_batch(&s, &src, &cancel, &emit);
        assert_eq!(res.status, JobStatus::NoWork, "{}", res.summary);
        assert!(res.summary.contains("НЕЧЕГО ДЕЛАТЬ"), "{}", res.summary);
        let report = res.batch.expect("отчёт");
        assert_eq!(report.skipped(), 1);
        assert_eq!(report.completed(), 0);
        assert_eq!(std::fs::read(out.join("a.wav")).unwrap(), b"keep");
    }

    /// Отмена до первого файла через execute_batch — общий статус Cancelled.
    #[test]
    fn cancel_before_first_batch_file_is_cancelled() {
        let d = tmpdir("cancel_batch");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "Текст.").unwrap();
        std::fs::write(src.join("b.txt"), "Текст.").unwrap();
        let s = GuiSettings {
            out_dir: d.join("out").display().to_string(),
            ..Default::default()
        };
        let cancel = Arc::new(AtomicBool::new(true));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
        let res = execute_batch(&s, &src, &cancel, &emit);
        assert_eq!(res.status, JobStatus::Cancelled, "{}", res.summary);
        assert!(res.summary.contains("ОТМЕНЕНО"), "{}", res.summary);
        let report = res.batch.expect("отчёт");
        assert!(report.cancelled);
        assert_eq!(report.skipped(), 2);
    }

    // ===== G6.1.1: порядок пакетных событий (без ORT/CUDA — чистые события) =====

    /// Сбор событий execute_batch в Vec (для проверок порядка).
    fn collect_events(
        s: &GuiSettings,
        src: &Path,
        cancel: &Arc<AtomicBool>,
    ) -> (JobResult, Vec<JobEvent>) {
        let events: Arc<std::sync::Mutex<Vec<JobEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = {
            let events = Arc::clone(&events);
            Arc::new(move |ev| events.lock().unwrap().push(ev))
        };
        let res = execute_batch(s, src, cancel, &emit);
        let collected = events.lock().unwrap().clone();
        (res, collected)
    }

    /// G6.1.1 (Test F): последовательный путь шлёт РОВНО ОДИН BatchStarted
    /// ДО любых FileDone (включая preflight Skipped) и BatchWorkersReady(1)
    /// тоже до FileDone. Синтез здесь падает на отсутствии модели — события
    /// и порядок от этого не зависят.
    #[test]
    fn sequential_batch_emits_single_started_before_any_filedone() {
        let d = tmpdir("g611_seq_events");
        let src = d.join("src");
        let out = d.join("out");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(src.join("a.txt"), "Текст.").unwrap();
        std::fs::write(src.join("b.txt"), "Текст.").unwrap();
        std::fs::write(out.join("a.wav"), b"exists").unwrap(); // preflight Skipped
        let s = GuiSettings {
            onnx_base: d.join("no-models").display().to_string(), // execute_job упадёт
            out_dir: out.display().to_string(),
            ..Default::default()
        };
        let (res, events) = collect_events(&s, &src, &Arc::new(AtomicBool::new(false)));
        assert_eq!(res.status, JobStatus::Failed, "{}", res.summary);
        let started = events
            .iter()
            .filter(|e| matches!(e, JobEvent::BatchStarted { .. }))
            .count();
        assert_eq!(started, 1, "ровно один BatchStarted: {events:?}");
        let pos_started = events
            .iter()
            .position(|e| matches!(e, JobEvent::BatchStarted { .. }))
            .unwrap();
        let first_done = events
            .iter()
            .position(|e| matches!(e, JobEvent::FileDone { .. }))
            .expect("есть хотя бы preflight FileDone");
        assert!(
            pos_started < first_done,
            "BatchStarted строго до первого FileDone: {events:?}"
        );
        let ready_pos = events
            .iter()
            .position(|e| matches!(e, JobEvent::BatchWorkersReady { .. }))
            .expect("последовательный путь сообщает BatchWorkersReady(1)");
        assert!(ready_pos < first_done, "BatchWorkersReady до FileDone");
        if let JobEvent::BatchStarted { total, workers } = &events[pos_started] {
            assert_eq!(*total, 2);
            assert_eq!(*workers, 0, "воркеры изначально не готовы");
        }
        if let JobEvent::BatchWorkersReady { workers } = &events[ready_pos] {
            assert_eq!(*workers, 1);
        }
        // preflight-пропуск учтён событием до старта синтезируемого файла
        let done_skipped = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    JobEvent::FileDone { report, .. }
                        if report.status == FileStatus::Skipped
                            && report.reason.as_deref().is_some_and(|r| r.contains("существует"))
                )
            })
            .count();
        assert_eq!(done_skipped, 1, "существующий результат — ранний Skipped");
    }

    /// G6.1.1 (Test B/C на уровне execute_batch): весь пакет пропущен —
    /// NoWork, ровно один BatchStarted, ни одного BatchWorkersReady с
    /// воркерами пула и ни одного FileStarted (Pocket не нужен).
    #[test]
    fn all_skipped_batch_events_have_no_workers_ready() {
        let d = tmpdir("g611_allskip_events");
        let src = d.join("src");
        let out = d.join("out");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        for i in 0..3 {
            std::fs::write(src.join(format!("f{i}.txt")), "Текст.").unwrap();
            std::fs::write(out.join(format!("f{i}.wav")), b"done").unwrap();
        }
        let s = GuiSettings {
            out_dir: out.display().to_string(),
            ..Default::default()
        };
        let (res, events) = collect_events(&s, &src, &Arc::new(AtomicBool::new(false)));
        assert_eq!(res.status, JobStatus::NoWork, "{}", res.summary);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, JobEvent::BatchStarted { .. }))
                .count(),
            1
        );
        let dones = events
            .iter()
            .filter(|e| matches!(e, JobEvent::FileDone { .. }))
            .count();
        assert_eq!(dones, 3, "все пропуски дали FileDone");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, JobEvent::FileStarted { .. })),
            "ни один файл не стартовал"
        );
    }

    /// G6.1.1: отмена до preflight — Cancelled; BatchStarted всё равно
    /// отправляется до FileDone, все файлы «отменено до запуска».
    #[test]
    fn cancelled_batch_events_order() {
        let d = tmpdir("g611_cancel_events");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "Текст.").unwrap();
        std::fs::write(src.join("b.txt"), "Текст.").unwrap();
        let s = GuiSettings {
            out_dir: d.join("out").display().to_string(),
            ..Default::default()
        };
        let (res, events) = collect_events(&s, &src, &Arc::new(AtomicBool::new(true)));
        assert_eq!(res.status, JobStatus::Cancelled, "{}", res.summary);
        let pos_started = events
            .iter()
            .position(|e| matches!(e, JobEvent::BatchStarted { .. }))
            .expect("BatchStarted отправлен");
        let first_done = events
            .iter()
            .position(|e| matches!(e, JobEvent::FileDone { .. }))
            .expect("FileDone отмены");
        assert!(pos_started < first_done, "{events:?}");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, JobEvent::FileDone { .. }))
                .count(),
            2
        );
    }

    // ============ реальные прогоны (нужны модели + ORT DLL + ffmpeg) ============
    // Запускать ПО ОДНОМУ устройству на процесс (ORT грузится в процесс
    // один раз; смена CPU/CUDA внутри процесса невозможна — честная ошибка):
    //   cargo test -p pockettts-gui-rust --release --lib real_matrix -- --ignored --nocapture
    //   SHTORM_GUI_TEST_DEVICE=cuda cargo test -p pockettts-gui-rust --release --lib real_matrix -- --ignored --nocapture

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
            eos_threshold: crate::settings::EosThresholdSetting::Fixed(-4.0),
            eos_tail_short: shtorm_engine::EosTail::Fixed(5),
            eos_tail_long: shtorm_engine::EosTail::Fixed(3),
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
        if fmt.needs_ffmpeg() {
            s.ffmpeg_path = REAL_FFMPEG.into();
            // раздельные пресеты: каждый формат берёт свой
            s.mp3_preset = "-V2 (Высокое)".into();
            s.opus_preset = "opus 64k".into();
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

    /// G3: реальный пакетный прогон на CPU FP32 (модели + ORT).
    /// 3 валидных файла (вложенные каталоги), 1 битый (не-UTF8) и 1
    /// существующий результат: Completed 3 / Failed 1 / Skipped 1,
    /// структура подкаталогов сохранена, существующий файл не перезаписан.
    #[test]
    #[ignore = "реальный пакетный синтез: модели + ORT DLL; CPU FP32"]
    fn real_batch_cpu() {
        let d = tmpdir("g3_batch");
        let src = d.join("src");
        let out = d.join("out");
        std::fs::create_dir_all(src.join("sub").join("deep")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(src.join("a.txt"), "П+ервый текст.").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), "Втор+ой текст.").unwrap();
        std::fs::write(src.join("sub").join("deep").join("c.txt"), "Тр+етий текст.").unwrap();
        std::fs::write(src.join("d.md"), "Четв+ёртый текст.").unwrap();
        std::fs::write(src.join("bad.txt"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        // существующий результат: не должен перезаписываться
        std::fs::write(out.join("a.wav"), b"keep").unwrap();

        let mut s = real_settings(OutputFormatChoice::Wav, &out);
        s.device = DeviceChoice::Cpu;
        s.workers = Some(2);
        s.clamped();
        let cancel = Arc::new(AtomicBool::new(false));
        let events: Arc<std::sync::Mutex<Vec<JobEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = {
            let events = Arc::clone(&events);
            Arc::new(move |ev| events.lock().unwrap().push(ev))
        };
        let res = execute_batch(&s, &src, &cancel, &emit);
        eprintln!("[g3_batch] {} | {}", res.status.label(), res.summary);
        for line in res.summary.lines() {
            eprintln!("  {line}");
        }

        // 3 готово + 1 ошибка + 1 пропущено = частичный успех, не Completed
        assert_eq!(res.status, JobStatus::Partial, "{}", res.summary);
        assert!(res.summary.contains("ЧАСТИЧНЫЙ УСПЕХ"), "{}", res.summary);
        let report = res.batch.expect("пакетный отчёт");
        assert_eq!(report.total(), 5, "{}", report.summary());
        assert_eq!(report.completed(), 3, "{}", report.summary());
        assert_eq!(report.failed(), 1, "{}", report.summary());
        assert_eq!(report.skipped(), 1, "{}", report.summary());

        // структура подкаталогов сохранена
        assert!(out.join("sub").join("b.wav").is_file());
        assert!(out.join("sub").join("deep").join("c.wav").is_file());
        assert!(out.join("d.wav").is_file());
        // существующий результат не тронут, битый файл без итога
        assert_eq!(std::fs::read(out.join("a.wav")).unwrap(), b"keep");
        assert!(!out.join("bad.wav").exists());
        let bad = report
            .files
            .iter()
            .find(|f| f.input.ends_with("bad.txt"))
            .expect("битый файл в отчёте");
        assert_eq!(bad.status, FileStatus::Failed);
        assert!(
            bad.reason.as_deref().unwrap().contains("прочитать"),
            "{:?}",
            bad.reason
        );

        // прогресс по файлам и чанкам шёл через события
        let events = events.lock().unwrap().clone();
        let file_starts = events
            .iter()
            .filter(|e| matches!(e, JobEvent::FileProgress { .. }))
            .count();
        assert!(file_starts >= 3, "прогресс чанков по файлам: {file_starts}");
        assert!(events
            .iter()
            .any(|e| matches!(e, JobEvent::FileDone { .. })));
    }

    /// G6.3: реальный пакетный синтез INT8 через полный путь GUI
    /// (execute_batch → пул → Pocket с ModelPrecision::Int8). Проверяет:
    /// 2/2 файла синтезированы, WAV валидны, и — ГЛАВНОЕ — выход ОТЛИЧАЕТСЯ
    /// от FP32 побайтово (доказывает, что INT8-графы реально загружены, а
    /// не случился молчаливый откат на FP32). CPU.
    /// Запуск: `cargo test -p pockettts-gui-rust --release --lib real_batch_cpu_int8
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore = "реальный синтез: модели + ORT DLL"]
    fn real_batch_cpu_int8() {
        let d = tmpdir("g63_int8_batch");
        let src = d.join("src");
        let out = d.join("out");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(src.join("one.txt"), "Стр+елки час+ов показ+ывали п+олночь.").unwrap();
        std::fs::write(
            src.join("two.txt"),
            "Ветер перелист+ал страницы забытой книги.",
        )
        .unwrap();

        let mut s = real_settings(OutputFormatChoice::Wav, &out);
        s.device = DeviceChoice::Cpu;
        s.model = ModelChoice::FastInt8;
        s.workers = Some(2);
        s.clamped();
        let no_emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
        let res = execute_batch(&s, &src, &Arc::new(AtomicBool::new(false)), &no_emit);
        eprintln!("[g63_int8_batch] {} | {}", res.status.label(), res.summary);
        assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
        let report = res.batch.expect("пакетный отчёт");
        assert_eq!(report.completed(), 2, "{}", report.summary());
        for name in ["one.wav", "two.wav"] {
            let wav = out.join(name);
            let bytes = std::fs::read(&wav).unwrap();
            assert!(
                bytes.len() > 10_000,
                "{name}: {}/{} байт",
                bytes.len(),
                wav.display()
            );
            assert_eq!(&bytes[0..4], b"RIFF", "{name}: не WAV");
            assert!(
                bytes[44..].iter().any(|&b| b != 0),
                "{name}: данные не должны быть тишиной"
            );
        }

        // тот же один текст через Fast FP32 одиночным заданием: байты другие
        // (квантование меняет выход) — значит INT8-графы действительно грузились
        let s_fp32 = GuiSettings {
            model: ModelChoice::Fast,
            ..s.clone()
        };
        let fp32_out = out.join("one_fp32.wav");
        let res32 = execute_job(
            &JobConfig {
                settings: s_fp32,
                text: "Стр+елки час+ов показ+ывали п+олночь.".into(),
                out_path: fp32_out.clone(),
            },
            &Arc::new(AtomicBool::new(false)),
            &|_| {},
        );
        assert_eq!(res32.status, JobStatus::Completed, "{}", res32.summary);
        let int8 = std::fs::read(out.join("one.wav")).unwrap();
        let fp32 = std::fs::read(&fp32_out).unwrap();
        assert_ne!(
            int8, fp32,
            "INT8 и FP32 выходы обязаны отличаться (иначе был fallback на FP32)"
        );
    }

    /// G6.6: короткий реальный тест НОВОЙ ручной конфигурации CUDA —
    /// мультипроцесс 2×1/T4 (2 процесса × 1 Pocket, F=2) на поднаборе из
    /// 4 файлов main12. WAV обязаны совпадать побитово с пулом 1×2/T4
    /// (равный T). Прогон 1×2/T4 на том же поднаборе — внутренний эталон.
    /// 2×2/T4 покрыт real_batch_cuda_hybrid, 1×2/T4 — real_batch_cuda_pool.
    #[test]
    #[ignore = "реальный CUDA: модели + ORT GPU DLL (~3 мин)"]
    fn real_batch_cuda_manual_g66() {
        let parts = p2b3_parts();
        let src_dir = {
            let d = tmpdir("g66_cuda_short");
            std::fs::create_dir_all(&d).unwrap();
            for p in &parts[..4] {
                std::fs::copy(p, d.join(p.file_name().unwrap())).unwrap();
            }
            d
        };
        let run = |tag: &str, cfg: crate::settings::ParallelSettings| -> Vec<(String, Vec<u8>)> {
            let d = tmpdir(tag);
            let mut s = p2b3_settings(&d, None);
            s.out_dir = d.join("out").display().to_string();
            s.parallel_cuda = cfg.clamped_for_device(DeviceChoice::Cuda);
            s.clamped();
            let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
            let t0 = Instant::now();
            let res = execute_batch(&s, &src_dir, &Arc::new(AtomicBool::new(false)), &emit);
            eprintln!(
                "[{tag}] {} | {:.1} с | {}",
                res.status.label(),
                t0.elapsed().as_secs_f32(),
                res.summary
            );
            assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
            let rep = res.batch.expect("отчёт");
            assert_eq!(rep.completed(), 4, "{}", rep.summary());
            read_batch_wavs(&d.join("out"))
                .into_iter()
                .collect::<Vec<_>>()
        };
        let ref_wavs = run("g66_cuda_ref_1x2", ps_custom(1, 2, 2, 4));
        let mp_wavs = run("g66_cuda_2x1", ps_custom(2, 1, 2, 4));
        assert_eq!(ref_wavs.len(), 4);
        assert_eq!(mp_wavs.len(), 4);
        for ((name_r, bytes_r), (name_m, bytes_m)) in ref_wavs.iter().zip(mp_wavs.iter()) {
            assert_eq!(name_r, name_m);
            assert_eq!(
                bytes_r, bytes_m,
                "{name_r}: 1×2/T4 и 2×1/T4 обязаны совпадать побитово"
            );
        }
    }
    // Запуск (один тест — один процесс; таймаут снаружи):
    //   cargo test -p pockettts-gui-rust --release --lib \
    //     real_batch_cpu_pool_g66 -- --exact --ignored --nocapture --test-threads=1

    /// Настройки CPU-пула на корпусе P2B.3 (main12) с конфигурацией P/W/F/T.
    fn cpu_pool_settings(out_dir: &Path, cfg: crate::settings::ParallelSettings) -> GuiSettings {
        let mut s = p2b3_settings(out_dir, None);
        s.device = DeviceChoice::Cpu;
        s.parallel_cpu = cfg.clamped_for_device(DeviceChoice::Cpu);
        s.clamped();
        s
    }

    fn ps_custom(processes: u32, w: u32, f: u32, t: u32) -> crate::settings::ParallelSettings {
        crate::settings::ParallelSettings {
            profile: crate::settings::ParallelProfile::Custom,
            processes,
            workers_per_process: w,
            max_concurrent_files: f,
            ort_threads: t,
        }
    }

    /// Читает WAV-итоги пакета в map имя→байты (только completed).
    fn read_batch_wavs(out_root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut map = std::collections::BTreeMap::new();
        fn walk(dir: &Path, map: &mut std::collections::BTreeMap<String, Vec<u8>>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, map);
                } else if p.extension().and_then(|e| e.to_str()) == Some("wav") {
                    let name = p.file_name().unwrap().to_string_lossy().to_string();
                    map.insert(name, std::fs::read(&p).unwrap());
                }
            }
        }
        walk(out_root, &mut map);
        map
    }

    /// G6.6: обязательная матрица реального CPU-пакета —
    /// A) Slow FP32 пул 1×2/T4 (профессия «Оптимальный»);
    /// B) Slow FP32 пул 1×4/T1 («Производительный»);
    /// C) Slow FP32 мультипроцесс 2×1/T4 на коротком поднаборе (4 файла);
    /// D) Fast INT8 пул 1×2/T4.
    /// Паритет: A ≡ C побайтово на общих файлах (равный T=4, разные
    /// топологии — правило «WAV зависит только от T»); B отличается от A
    /// (T=1 меняет порядок редукции ORT).
    #[test]
    #[ignore = "реальный CPU-синтез: модели + ORT CPU DLL (~8 мин)"]
    fn real_batch_cpu_pool_g66() {
        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let short_src = {
            // короткий поднабор: копия 4 файлов в отдельный каталог
            let d = tmpdir("g66_short");
            std::fs::create_dir_all(&d).unwrap();
            for p in &parts[..4] {
                let name = p.file_name().unwrap();
                std::fs::copy(p, d.join(name)).unwrap();
            }
            d
        };
        let run = |tag: &str,
                   cfg: crate::settings::ParallelSettings,
                   src: &Path,
                   model: ModelChoice|
         -> (
            BatchReport,
            std::collections::BTreeMap<String, Vec<u8>>,
            Vec<String>,
        ) {
            let d = tmpdir(tag);
            let mut s = cpu_pool_settings(&d, cfg);
            s.model = model;
            s.out_dir = d.join("out").display().to_string();
            s.clamped();
            let logs: Arc<std::sync::Mutex<Vec<String>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = {
                let logs = Arc::clone(&logs);
                Arc::new(move |ev| {
                    if let JobEvent::Log(line) = ev {
                        logs.lock().unwrap().push(line);
                    }
                })
            };
            let t0 = Instant::now();
            let res = execute_batch(&s, src, &Arc::new(AtomicBool::new(false)), &emit);
            eprintln!(
                "[{tag}] {} | {:.1} с | {}",
                res.status.label(),
                t0.elapsed().as_secs_f32(),
                res.summary
            );
            assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
            let rep = res.batch.expect("отчёт");
            let wavs = read_batch_wavs(&d.join("out"));
            let logs = logs.lock().unwrap().clone();
            (rep, wavs, logs)
        };

        // A) Slow FP32 пул 1×2/T4
        let (rep_a, wavs_a, logs_a) = run(
            "g66_a_1x2_t4",
            ps_custom(1, 2, 2, 4),
            &src_dir,
            ModelChoice::Slow,
        );
        assert_eq!(rep_a.completed(), 12, "{}", rep_a.summary());
        assert_eq!(rep_a.pool_loads, Some(2), "Pocket загружен ровно 2 раза");
        assert_eq!(rep_a.pool_workers, Some(2));
        assert_eq!(wavs_a.len(), 12);
        assert!(logs_a.iter().any(|l| l.contains("постоянный пул")));

        // B) Slow FP32 пул 1×4/T1 (производительный профиль)
        let (rep_b, wavs_b, _) = run(
            "g66_b_1x4_t1",
            ps_custom(1, 4, 4, 1),
            &src_dir,
            ModelChoice::Slow,
        );
        assert_eq!(rep_b.completed(), 12);
        assert_eq!(rep_b.pool_loads, Some(4));
        // T=1 меняет побитовый результат (порядок редукции intra-op ORT)
        let common_ab: Vec<_> = wavs_a.keys().cloned().collect();
        assert!(
            common_ab.iter().any(|n| wavs_a[n] != wavs_b[n]),
            "T4 и T1 должны отличаться побитово хотя бы на одном файле"
        );

        // C) мультипроцесс 2×1/T4, короткий поднабор
        let (rep_c, wavs_c, logs_c) = run(
            "g66_c_2x1_t4",
            ps_custom(2, 1, 2, 4),
            &short_src,
            ModelChoice::Slow,
        );
        assert_eq!(rep_c.completed(), 4, "{}", rep_c.summary());
        assert_eq!(rep_c.pool_processes, Some(2), "два дочерних процесса");
        assert_eq!(rep_c.pool_loads, Some(2), "Pocket в дочерних процессах");
        assert!(logs_c.iter().any(|l| l.contains("мультипроцесс 2×1")));
        // паритет A ≡ C на общих файлах (равный T, разная топология)
        for (name, bytes_c) in &wavs_c {
            let bytes_a = wavs_a
                .get(name)
                .unwrap_or_else(|| panic!("файл {name} отсутствует в прогоне A"));
            assert_eq!(
                bytes_a, bytes_c,
                "{name}: 1×2/T4 и 2×1/T4 обязаны совпадать побитово"
            );
        }

        // D) Fast INT8 пул 1×2/T4
        let (rep_d, wavs_d, _) = run(
            "g66_d_int8",
            ps_custom(1, 2, 2, 4),
            &src_dir,
            ModelChoice::FastInt8,
        );
        assert_eq!(rep_d.completed(), 12);
        assert_eq!(rep_d.pool_loads, Some(2));
        // INT8 ≠ FP32 (разные графы)
        assert!(
            wavs_a.keys().any(|n| wavs_a[n] != wavs_d[n]),
            "INT8 и FP32 обязаны отличаться"
        );
    }

    /// G5/G5.1: реальная перезапись WAV — существующий файл, новый файл при
    /// включённой перезаписи, отмена до публикации, повтор после отмены.
    /// Старый файл заменяется только после успешного синтеза; при отмене
    /// остаётся побайтово неизменным, `.partial` — для диагностики.
    /// Запуск: `cargo test -p pockettts-gui-rust --release --lib
    /// real_overwrite_wav -- --ignored --nocapture` (для CUDA:
    /// SHTORM_GUI_TEST_DEVICE=cuda).
    #[test]
    #[ignore = "реальный синтез: модели + ORT DLL"]
    fn real_overwrite_wav() {
        let cancel_false = || Arc::new(AtomicBool::new(false));

        // 1) существующий + overwrite: файл ОБНОВЛЁН, не «создан»
        let d = tmpdir("g51_ow_update");
        let mut s = real_settings(OutputFormatChoice::Wav, &d);
        s.overwrite_existing = true;
        let out = derive_out_path(&s);
        std::fs::write(&out, b"old").unwrap();
        let res = execute_job(
            &JobConfig {
                settings: s.clone(),
                text: REAL_TEXT.into(),
                out_path: out.clone(),
            },
            &cancel_false(),
            &|_| {},
        );
        eprintln!("[g51_ow_update] {} | {}", res.status.label(), res.summary);
        assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
        assert!(res.summary.contains("ФАЙЛ ОБНОВЛЁН"), "{}", res.summary);
        assert!(!res.summary.contains("создан новый"), "{}", res.summary);
        assert!(
            res.replaced_existing,
            "существовавший файл помечен заменённым"
        );
        let new_bytes = std::fs::read(&out).unwrap();
        assert!(new_bytes.starts_with(b"RIFF"), "старый файл заменён WAV");
        assert_ne!(new_bytes, b"old".as_slice(), "содержимое изменилось");
        assert!(
            !std::fs::read_dir(&d)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".partial")),
            "после успешной замены .partial не остался"
        );

        // 2) НОВЫЙ файл при включённой перезаписи — «создан новый файл»,
        // не «обновлён»
        let d2 = tmpdir("g51_ow_new");
        let mut s2 = real_settings(OutputFormatChoice::Wav, &d2);
        s2.overwrite_existing = true;
        let out2 = derive_out_path(&s2);
        let res2 = execute_job(
            &JobConfig {
                settings: s2,
                text: REAL_TEXT.into(),
                out_path: out2.clone(),
            },
            &cancel_false(),
            &|_| {},
        );
        eprintln!("[g51_ow_new] {} | {}", res2.status.label(), res2.summary);
        assert_eq!(res2.status, JobStatus::Completed, "{}", res2.summary);
        assert!(
            res2.summary.contains("создан новый файл"),
            "новый файл не называется «обновлённым»: {}",
            res2.summary
        );
        assert!(!res2.summary.contains("ОБНОВЛЁН"), "{}", res2.summary);
        assert!(!res2.replaced_existing);
        assert!(out2.is_file());

        // 3) отмена после ≥1 чанка при существующем результате: старый файл
        // побайтово (строже SHA256) неизменен, черновик сохранён
        let d3 = tmpdir("g51_ow_cancel");
        let mut s3 = real_settings(OutputFormatChoice::Wav, &d3);
        s3.overwrite_existing = true;
        s3.max_tokens = 20; // гарантированно несколько чанков
        let out3 = derive_out_path(&s3);
        std::fs::write(&out3, b"old-bytes-cancel").unwrap();
        let cancel3 = Arc::new(AtomicBool::new(false));
        let c3 = Arc::clone(&cancel3);
        let res3 = execute_job(
            &JobConfig {
                settings: s3.clone(),
                text: REAL_TEXT.into(),
                out_path: out3.clone(),
            },
            &cancel3,
            &|ev| {
                if let JobEvent::Progress { done, .. } = &ev {
                    if *done >= 1 {
                        c3.store(true, Ordering::SeqCst);
                    }
                }
            },
        );
        eprintln!("[g51_ow_cancel] {} | {}", res3.status.label(), res3.summary);
        assert_eq!(res3.status, JobStatus::Cancelled, "{}", res3.summary);
        assert_eq!(
            std::fs::read(&out3).unwrap(),
            b"old-bytes-cancel",
            "старый файл побайтово цел (эквивалент SHA256-совпадения)"
        );
        assert!(
            res3.summary.contains("старый файл сохранён"),
            "отмена явно сообщает о сохранении старого файла: {}",
            res3.summary
        );
        assert!(res3.partial_path.as_ref().is_some_and(|p| p.is_file()));

        // 4) повторная генерация после отмены: тот же путь, файл обновлён
        let res4 = execute_job(
            &JobConfig {
                settings: s3,
                text: REAL_TEXT.into(),
                out_path: out3.clone(),
            },
            &cancel_false(),
            &|_| {},
        );
        eprintln!("[g51_ow_retry] {} | {}", res4.status.label(), res4.summary);
        assert_eq!(res4.status, JobStatus::Completed, "{}", res4.summary);
        assert!(res4.summary.contains("ФАЙЛ ОБНОВЛЁН"), "{}", res4.summary);
        assert!(res4.replaced_existing);
        let bytes4 = std::fs::read(&out3).unwrap();
        assert!(bytes4.starts_with(b"RIFF"), "итог — настоящий WAV");
        assert_ne!(bytes4, b"old-bytes-cancel");
    }

    /// G5.1: реальная перезапись MP3/Opus через ffmpeg — существующий файл
    /// заменяется только после успешного кодирования; новый файл при
    /// включённой перезаписи сообщается как созданный; сиротских процессов
    /// ffmpeg после заданий нет.
    #[test]
    #[ignore = "реальный синтез: модели + ORT DLL + ffmpeg"]
    fn real_overwrite_mp3_opus() {
        fn ffmpeg_count() -> u32 {
            let out = std::process::Command::new("tasklist")
                .arg("/FI")
                .arg("IMAGENAME eq ffmpeg.exe")
                .arg("/FO")
                .arg("CSV")
                .arg("/NH")
                .output()
                .expect("tasklist");
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.to_ascii_lowercase().starts_with("\"ffmpeg.exe"))
                .count() as u32
        }
        let baseline = ffmpeg_count();

        for (tag, fmt, magic) in [
            ("g51_ow_mp3", OutputFormatChoice::Mp3, &b"ID3"[..]),
            ("g51_ow_opus", OutputFormatChoice::Opus, &b"OggS"[..]),
        ] {
            // существующий + overwrite → заменён корректным контейнером
            let d = tmpdir(tag);
            let mut s = real_settings(fmt, &d);
            s.overwrite_existing = true;
            let out = derive_out_path(&s);
            std::fs::write(&out, b"old-bytes").unwrap();
            let res = execute_job(
                &JobConfig {
                    settings: s.clone(),
                    text: REAL_TEXT.into(),
                    out_path: out.clone(),
                },
                &Arc::new(AtomicBool::new(false)),
                &|_| {},
            );
            eprintln!("[{tag}] {} | {}", res.status.label(), res.summary);
            assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
            assert!(res.summary.contains("ФАЙЛ ОБНОВЛЁН"), "{}", res.summary);
            assert!(res.replaced_existing);
            let bytes = std::fs::read(&out).unwrap();
            assert!(bytes.starts_with(magic), "заменён корректным контейнером");
            assert_ne!(bytes, b"old-bytes");
            assert!(
                !std::fs::read_dir(&d)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().contains(".partial")),
                ".partial убран после публикации"
            );

            // новый файл при включённой перезаписи — «создан новый файл»
            let d2 = tmpdir(&format!("{tag}_new"));
            let mut s2 = real_settings(fmt, &d2);
            s2.overwrite_existing = true;
            let out2 = derive_out_path(&s2);
            let res2 = execute_job(
                &JobConfig {
                    settings: s2,
                    text: REAL_TEXT.into(),
                    out_path: out2.clone(),
                },
                &Arc::new(AtomicBool::new(false)),
                &|_| {},
            );
            eprintln!("[{tag}_new] {} | {}", res2.status.label(), res2.summary);
            assert_eq!(res2.status, JobStatus::Completed, "{}", res2.summary);
            assert!(
                res2.summary.contains("создан новый файл"),
                "{}",
                res2.summary
            );
            assert!(!res2.replaced_existing);
            assert!(std::fs::read(&out2).unwrap().starts_with(magic));
        }

        // после всех ffmpeg-заданий сиротских процессов не осталось
        let after = ffmpeg_count();
        assert_eq!(
            after, baseline,
            "ffmpeg-процессы завершены ({baseline} → {after})"
        );
    }

    /// G5.1: ошибка ffmpeg при существующем результате и включённой
    /// перезаписи — Failed, старый файл побайтово цел, черновик для
    /// диагностики. «ffmpeg» — cmd-заглушка, проходящая проверку -version,
    /// но падающая на аргументах энкодера.
    #[cfg(windows)]
    #[test]
    #[ignore = "реальный прогон с cmd-заглушкой ffmpeg; модели + ORT DLL"]
    fn real_ffmpeg_error_existing_result() {
        let d = tmpdir("g51_ff_err");
        let stub = d.join("ffmpeg_stub.bat");
        std::fs::write(
            &stub,
            "@echo off\r\nif \"%1\"==\"-version\" exit /b 0\r\nexit /b 1\r\n",
        )
        .unwrap();
        let mut s = real_settings(OutputFormatChoice::Mp3, &d);
        s.overwrite_existing = true;
        s.ffmpeg_path = stub.display().to_string();
        let out = derive_out_path(&s);
        std::fs::write(&out, b"old-bytes-ffmpeg-err").unwrap();
        let res = execute_job(
            &JobConfig {
                settings: s,
                text: REAL_TEXT.into(),
                out_path: out.clone(),
            },
            &Arc::new(AtomicBool::new(false)),
            &|_| {},
        );
        eprintln!("[g51_ff_err] {} | {}", res.status.label(), res.summary);
        assert_eq!(res.status, JobStatus::Failed, "{}", res.summary);
        assert!(
            res.summary.contains("ffmpeg"),
            "ошибка связана с ffmpeg: {}",
            res.summary
        );
        assert_eq!(
            std::fs::read(&out).unwrap(),
            b"old-bytes-ffmpeg-err",
            "старый файл побайтово цел при ошибке — итог не заменён"
        );
    }

    /// G5.1: пакетный режим с существующим и новым результатом при
    /// overwrite=true — существующий обновляется («файл обновлён»), новый
    /// создаётся («создан новый файл»); повтор с overwrite=false — оба
    /// пропущены без изменения.
    #[test]
    #[ignore = "реальный пакетный синтез: модели + ORT DLL; CPU"]
    fn real_batch_overwrite_mixed() {
        let d = tmpdir("g51_batch_ow");
        let src = d.join("src");
        let out = d.join("out");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(src.join("a.txt"), "П+ервый пакетный текст.").unwrap();
        std::fs::write(src.join("b.txt"), "Вт+орой пакетный текст.").unwrap();
        std::fs::write(out.join("a.wav"), b"old-a").unwrap(); // существующий

        let mut s = real_settings(OutputFormatChoice::Wav, &out);
        s.device = DeviceChoice::Cpu;
        s.workers = Some(2);
        s.overwrite_existing = true;
        s.clamped();
        let cancel = Arc::new(AtomicBool::new(false));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
        let res = execute_batch(&s, &src, &cancel, &emit);
        eprintln!("[g51_batch_ow] {} | {}", res.status.label(), res.summary);
        assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
        let report = res.batch.expect("отчёт");
        assert_eq!(report.completed(), 2, "{}", report.summary());
        let rep_a = report
            .files
            .iter()
            .find(|f| f.input.ends_with("a.txt"))
            .unwrap();
        let rep_b = report
            .files
            .iter()
            .find(|f| f.input.ends_with("b.txt"))
            .unwrap();
        assert_eq!(rep_a.status, FileStatus::Completed);
        assert!(
            rep_a.reason.as_deref().unwrap().contains("обновлён")
                && !rep_a.reason.as_deref().unwrap().contains("создан"),
            "существующий — «обновлён»: {:?}",
            rep_a.reason
        );
        assert_eq!(rep_b.status, FileStatus::Completed);
        assert!(
            rep_b
                .reason
                .as_deref()
                .unwrap()
                .contains("создан новый файл"),
            "новый — «создан»: {:?}",
            rep_b.reason
        );
        // старый байтовый мусор заменён настоящим WAV
        assert!(std::fs::read(out.join("a.wav"))
            .unwrap()
            .starts_with(b"RIFF"));
        assert!(out.join("b.wav").is_file());

        // повтор с выключенной перезаписью: оба пропущены, файлы не тронуты
        let before_a = std::fs::read(out.join("a.wav")).unwrap();
        let before_b = std::fs::read(out.join("b.wav")).unwrap();
        let mut s2 = s.clone();
        s2.overwrite_existing = false;
        let emit2: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
        let res2 = execute_batch(&s2, &src, &Arc::new(AtomicBool::new(false)), &emit2);
        eprintln!(
            "[g51_batch_ow_skip] {} | {}",
            res2.status.label(),
            res2.summary
        );
        assert_eq!(res2.status, JobStatus::NoWork, "{}", res2.summary);
        let report2 = res2.batch.expect("отчёт");
        assert_eq!(report2.skipped(), 2);
        assert_eq!(std::fs::read(out.join("a.wav")).unwrap(), before_a);
        assert_eq!(std::fs::read(out.join("b.wav")).unwrap(), before_b);
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
                    "--eos-tail-short",
                    "5",
                    "--eos-tail-long",
                    "3",
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

    /// G4.1: побитовый WAV-паритет GUI == CLI для НЕстандартных EOS-настроек
    /// (порог −1 и хвосты 0/0; затем None/None). Дефолтный −4/5/3 паритет
    /// проверяет real_matrix_device. Реальные модели + ORT (CPU).
    /// cargo test -p pockettts-gui-rust --release --lib real_eos_parity -- --ignored --nocapture
    #[test]
    #[ignore = "реальный синтез: модели + ORT DLL; CPU"]
    fn real_eos_parity() {
        let run_pair = |tag: &str, mutate: &dyn Fn(&mut GuiSettings), cli_eos: &[&str]| {
            let d = tmpdir(tag);
            let mut s = real_settings(OutputFormatChoice::Wav, &d);
            s.workers = Some(2);
            mutate(&mut s);
            s.clamped();
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
                .expect("target/release/synthesize.exe");
            let cli_out = d.join(format!("cli_{tag}.wav"));
            let text_file = d.join("text.txt");
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
                .args(cli_eos)
                .env("ORT_DYLIB_PATH", &dll)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("запуск CLI");
            assert!(st.success(), "CLI завершился ошибкой: {st:?}");
            let gui = std::fs::read(&out).unwrap();
            let cli_b = std::fs::read(&cli_out).unwrap();
            assert_eq!(
                gui, cli_b,
                "{tag}: GUI и CLI с одинаковыми EOS-настройками обязаны совпадать побитово"
            );
            eprintln!("EOS PARITY [{tag}]: {} байт, GUI == CLI", gui.len());
        };

        // 0/0 с порогом −1
        run_pair(
            "g41_min_tail",
            &|s| {
                s.eos_threshold = crate::settings::EosThresholdSetting::Fixed(-1.0);
                s.eos_tail_short = shtorm_engine::EosTail::Fixed(0);
                s.eos_tail_long = shtorm_engine::EosTail::Fixed(0);
            },
            &[
                "--eos-threshold",
                "-1.0",
                "--eos-tail-short",
                "0",
                "--eos-tail-long",
                "0",
            ],
        );
        // None/None (штатный fallback движка 5/3)
        run_pair(
            "g41_none_tail",
            &|s| {
                s.eos_threshold = crate::settings::EosThresholdSetting::Auto;
                s.eos_tail_short = shtorm_engine::EosTail::Default;
                s.eos_tail_long = shtorm_engine::EosTail::Default;
            },
            &[
                "--eos-threshold",
                "-4.0",
                "--eos-tail-short",
                "none",
                "--eos-tail-long",
                "none",
            ],
        );
    }

    // ===== G6.6: выбор топологии пакета (чистая логика) =====

    type EmitLogs = (
        Arc<dyn Fn(JobEvent) + Send + Sync>,
        Arc<std::sync::Mutex<Vec<String>>>,
    );

    fn emit_collector() -> EmitLogs {
        let logs: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let l2 = Arc::clone(&logs);
        (
            Arc::new(move |ev| {
                if let JobEvent::Log(line) = ev {
                    l2.lock().unwrap().push(line);
                }
            }),
            logs,
        )
    }

    fn ps(processes: u32, w: u32, f: u32, t: u32) -> crate::settings::ParallelSettings {
        crate::settings::ParallelSettings {
            profile: crate::settings::ParallelProfile::Custom,
            processes,
            workers_per_process: w,
            max_concurrent_files: f,
            ort_threads: t,
        }
    }

    /// Базовые правила: <2 кандидатов или диагностический откат —
    /// последовательный путь; P=1 — однопроцессный пул на active Pocket;
    /// лишние Pocket не создаются (active ≤ F, кандидаты).
    #[test]
    fn topology_basic_rules() {
        let cpu = GuiSettings::default(); // device Cpu, без fake-VRAM
        let (emit, _logs) = emit_collector();
        assert_eq!(
            decide_topology(&cpu, &ps(1, 2, 2, 4), 1, true, &emit),
            BatchTopology::Sequential,
            "1 кандидат — чанк-параллелизм одиночного файла"
        );
        assert_eq!(
            decide_topology(&cpu, &ps(1, 2, 2, 4), 0, false, &emit),
            BatchTopology::Sequential,
            "SHTORM_GUI_BATCH_POOL=0 — диагностический откат"
        );
        assert_eq!(
            decide_topology(&cpu, &ps(1, 2, 2, 4), 5, true, &emit),
            BatchTopology::InProcessPool { workers: 2 }
        );
        // F < P×W: Pocket сверх F не создаются
        assert_eq!(
            decide_topology(&cpu, &ps(1, 4, 2, 4), 8, true, &emit),
            BatchTopology::InProcessPool { workers: 2 }
        );
        // кандидатов меньше F — активных не больше кандидатов
        assert_eq!(
            decide_topology(&cpu, &ps(1, 4, 4, 4), 3, true, &emit),
            BatchTopology::InProcessPool { workers: 3 }
        );
        // CPU 2×1: мультипроцесс
        assert_eq!(
            decide_topology(&cpu, &ps(2, 1, 2, 4), 6, true, &emit),
            BatchTopology::MultiProcess {
                children: 2,
                workers_per_child: 1,
                max_active_files: 2
            }
        );
        // CPU 2×2 с F=2: второй процесс не нужен (active ≤ W)
        assert_eq!(
            decide_topology(&cpu, &ps(2, 2, 2, 4), 6, true, &emit),
            BatchTopology::InProcessPool { workers: 2 }
        );
        // CUDA 2×2 F=4 без fake — nvidia-smi недоступен в тестах → пул 1×2
        // (проверено отдельно в topology_cuda_preflight ниже с fake-обходом)
    }

    /// Fake-режим пропускает VRAM-префлайт: мультипроцесс CUDA выбирается.
    #[test]
    fn topology_cuda_fake_multiprocess() {
        std::env::set_var("SHTORM_G62_FAKE", "1");
        let cuda = GuiSettings {
            device: DeviceChoice::Cuda,
            ..Default::default()
        };
        let (emit, logs) = emit_collector();
        let topo = decide_topology(&cuda, &ps(2, 2, 4, 4), 6, true, &emit);
        std::env::remove_var("SHTORM_G62_FAKE");
        assert!(logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("VRAM-префлайт пропущен")));
        assert_eq!(
            topo,
            BatchTopology::MultiProcess {
                children: 2,
                workers_per_child: 2,
                max_active_files: 4
            }
        );
    }

    // ===== G6.1: реальный пакетный CUDA на постоянном пуле =====
    // Запуск (по одному на процесс; ORT грузится один раз):
    //   SHTORM_GUI_TEST_DEVICE=cuda cargo test -p pockettts-gui-rust --release --lib \
    //     real_batch_cuda_pool -- --ignored --nocapture --test-threads=1

    /// Параметры эталона P2B.3: slow FP32, TF32=0, I/O binding on (дефолт),
    /// temp 0.3, steps 1, seed 42, EOS -4/5/3, anti-repeat, brightness 8 дБ,
    /// denoise off, pause_scale 1.0, paragraph_pause 0.9, max_tokens 50,
    /// голос — эталон P2B.1/P2B.2/P2B.3 «Василий Леванов F5-TTS3.wav».
    const P2B_VOICE: &str = r"C:\PocketTTS-GUI\examples\Василий Леванов F5-TTS3.wav";
    const P2B_SOURCE: &str =
        r"C:\Users\alexa\Documents\Гарри Поттер отрывок 2_accented_processed_normalized.txt";
    /// Набор из P2B.3 (12 файлов, 77 чанков на весь текст).
    const P2B_PARTS_DIR: &str = r"out\cuda_p2b3\main12_bal_20261110-122635";

    /// Каталог частей P2B.3: 12 файлов, конкатенация побайтово = исходник.
    fn p2b3_parts() -> Vec<PathBuf> {
        let root = crate::job::find_project_root().expect("корень проекта");
        // SHTORM_G61_PARTS_DIR — другой набор P2B.3 (например ctrl6) для
        // контрольных прогонов той же матрицы
        let dir = std::env::var("SHTORM_G61_PARTS_DIR")
            .map(|d| root.join(d))
            .unwrap_or_else(|_| root.join(P2B_PARTS_DIR));
        let expected_files: usize = std::env::var("SHTORM_G61_PARTS_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(12);
        let mut parts: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("набор P2B.3 не найден {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("part_") && n.ends_with(".txt"))
            })
            .collect();
        assert_eq!(
            parts.len(),
            expected_files,
            "ожидалось {expected_files} частей набора"
        );
        parts.sort();
        // исходный текст сохраняется побайтово (сильнее хеша)
        let joined: Vec<u8> = parts
            .iter()
            .flat_map(|p| std::fs::read(p).unwrap())
            .collect();
        let original = std::fs::read(P2B_SOURCE).expect("исходный текст P2B.3 (только чтение)");
        assert_eq!(
            joined, original,
            "конкатенация частей обязана быть побайтово равна исходнику"
        );
        parts
    }

    /// Настройки эталона P2B.3 для пакетного прогона (CUDA, seed 42).
    fn p2b3_settings(out_dir: &Path, workers: Option<u32>) -> GuiSettings {
        let mut s = real_settings(OutputFormatChoice::Wav, out_dir);
        s.device = DeviceChoice::Cuda;
        s.voice_wav = P2B_VOICE.into();
        s.max_tokens = 50;
        s.seed = 42;
        s.workers = workers;
        s.clamped();
        s
    }

    fn ffmpeg_process_count() -> u32 {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq ffmpeg.exe", "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.to_ascii_lowercase().starts_with("\"ffmpeg.exe"))
            .count() as u32
    }

    /// G6.1: главный тест — пакет 12 файлов на постоянном пуле 2 воркеров:
    /// Pocket загружен ровно 2 раза за весь пакет, каждый WAV побитово равен
    /// результату последовательного пути (модель на файл, те же настройки).
    /// Покрывает: независимость от того, какой Pocket получил файл,
    /// влияние повторного использования Pocket, 77 чанков без потерь/дублей.
    #[test]
    #[ignore = "реальный пакетный синтез CUDA: модели + ORT GPU DLL (2 прогона 12 файлов)"]
    fn real_batch_cuda_pool() {
        let parts = p2b3_parts();
        let expected_files: usize = std::env::var("SHTORM_G61_PARTS_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(12);
        let ffmpeg0 = ffmpeg_process_count();
        let vram0 = app_core::diagnostics::gpu_total_used_mb();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let cancel = Arc::new(AtomicBool::new(false));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});

        // 1) эталон: прежний последовательный путь (модель на КАЖДЫЙ файл)
        std::env::set_var("SHTORM_GUI_BATCH_POOL", "0");
        let d_ref = tmpdir("g61_pool_ref");
        let mut s_ref = p2b3_settings(&d_ref, Some(1));
        s_ref.out_dir = d_ref.join("out").display().to_string();
        s_ref.clamped();
        let ref_res = execute_batch(&s_ref, &src_dir, &cancel, &emit);
        eprintln!("[g61_ref] {} | {}", ref_res.status.label(), ref_res.summary);
        assert_eq!(ref_res.status, JobStatus::Completed, "{}", ref_res.summary);
        let ref_rep = ref_res.batch.expect("отчёт");
        assert_eq!(ref_rep.completed(), expected_files);
        assert!(
            ref_rep.pool_loads.is_none(),
            "последовательный путь без пула"
        );

        // 2) новый путь: постоянный пул, авто 2 воркера
        std::env::remove_var("SHTORM_GUI_BATCH_POOL");
        let d_pool = tmpdir("g61_pool_new");
        let mut s_pool = p2b3_settings(&d_pool, None);
        s_pool.out_dir = d_pool.join("out").display().to_string();
        s_pool.clamped();
        let cancel2 = Arc::new(AtomicBool::new(false));
        let logs: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let emit2: Arc<dyn Fn(JobEvent) + Send + Sync> = {
            let logs = Arc::clone(&logs);
            Arc::new(move |ev| {
                if let JobEvent::Log(line) = ev {
                    logs.lock().unwrap().push(line);
                }
            })
        };
        let pool_res = execute_batch(&s_pool, &src_dir, &cancel2, &emit2);
        eprintln!(
            "[g61_pool] {} | {}",
            pool_res.status.label(),
            pool_res.summary
        );
        assert_eq!(
            pool_res.status,
            JobStatus::Completed,
            "{}",
            pool_res.summary
        );
        let pool_rep = pool_res.batch.expect("отчёт");
        assert_eq!(pool_rep.completed(), expected_files);
        // ГЛАВНЫЙ КРИТЕРИЙ: Pocket загружен ровно 2 раза на весь пакет
        assert_eq!(pool_rep.pool_loads, Some(2), "{}", pool_rep.summary());
        assert_eq!(pool_rep.pool_workers, Some(2));
        let logs = logs.lock().unwrap().join("\n");
        assert!(
            logs.contains("Pocket загружен 2 раз"),
            "журнал пула: {logs}"
        );

        // 3) побитовый паритет всех 12 WAV: пул == последовательный путь
        for f in &pool_rep.files {
            let out_pool = f.final_path.as_ref().expect("итог пула");
            let out_ref = d_ref.join("out").join(f.relative.with_extension("wav"));
            let a = std::fs::read(out_pool).expect("WAV пула");
            let b = std::fs::read(&out_ref)
                .unwrap_or_else(|e| panic!("WAV эталона {}: {e}", out_ref.display()));
            assert_eq!(
                a,
                b,
                "{}: WAV пула обязан быть побитово равен эталону",
                f.relative.display()
            );
            assert!(a.starts_with(b"RIFF"));
        }

        // 4) все чанки на месте (77 на весь текст любого набора P2B.3)
        let chunks: usize = pool_rep.files.iter().map(|f| f.chunks_total).sum();
        assert_eq!(chunks, 77, "сумма чанков набора");
        assert!(pool_rep
            .files
            .iter()
            .all(|f| f.chunks_done == f.chunks_total));

        // 5) без осиротевших ffmpeg; VRAM вернулась к базе (без утечки)
        assert_eq!(ffmpeg_process_count(), ffmpeg0, "сиротских ffmpeg нет");
        let vram1 = app_core::diagnostics::gpu_total_used_mb();
        if let (Some(v0), Some(v1)) = (vram0, vram1) {
            assert!(
                v1 < v0 + 2500.0,
                "VRAM после пакета {v1:.0} МБ против базы {v0:.0} МБ — похоже на утечку"
            );
            eprintln!("[g61_vram] база {v0:.0} МБ → после пакета {v1:.0} МБ");
        }
    }

    /// G6.1.1 (§7 приёмки): часть WAV уже существует (overwrite=false) —
    /// preflight пропускает их ДО загрузки Pocket (FileDone Skipped раньше
    /// BatchWorkersReady), остальные синтезируются пулом; счётчики событий
    /// (модель apply_event) полностью совпадают с итоговым BatchReport.
    #[test]
    #[ignore = "реальный пакетный синтез CUDA: модели + ORT GPU DLL (2 прогона)"]
    fn real_batch_cuda_pool_partial_skip_events() {
        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let d = tmpdir("g611_partial_skip");
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});

        // прогон 1: полный пакет на пуле — все 12 готовы (готовые WAV для копии)
        let out1 = d.join("out1");
        let mut s = p2b3_settings(&d, None);
        s.out_dir = out1.display().to_string();
        s.clamped();
        let r1 = execute_batch(&s, &src_dir, &Arc::new(AtomicBool::new(false)), &emit);
        assert_eq!(r1.status, JobStatus::Completed, "{}", r1.summary);
        assert_eq!(r1.batch.as_ref().unwrap().pool_loads, Some(2));

        // прогон 2: НОВЫЙ каталог результата с ПОЛОВИНОЙ готовых WAV
        // (копирование — только создание новых файлов, ничего не удаляем)
        let out2 = d.join("out2");
        std::fs::create_dir_all(&out2).unwrap();
        let mut preexisting = 0usize;
        for (i, p) in parts.iter().enumerate() {
            if i % 2 == 0 {
                let name = p.with_extension("wav").file_name().unwrap().to_os_string();
                std::fs::copy(out1.join(&name), out2.join(&name)).unwrap();
                preexisting += 1;
            }
        }
        let events: Arc<std::sync::Mutex<Vec<JobEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let emit2: Arc<dyn Fn(JobEvent) + Send + Sync> = {
            let events = Arc::clone(&events);
            Arc::new(move |ev| events.lock().unwrap().push(ev))
        };
        let mut s2 = s.clone();
        s2.out_dir = out2.display().to_string();
        s2.clamped();
        let r2 = execute_batch(&s2, &src_dir, &Arc::new(AtomicBool::new(false)), &emit2);
        eprintln!("[g611_partial] {} | {}", r2.status.label(), r2.summary);
        assert_eq!(r2.status, JobStatus::Completed, "{}", r2.summary);
        let rep = r2.batch.expect("отчёт");
        let expect_synth = parts.len() - preexisting;
        assert_eq!(rep.skipped(), preexisting, "{}", rep.summary());
        assert_eq!(rep.completed(), expect_synth, "{}", rep.summary());
        assert_eq!(rep.pool_loads, Some(2), "пул работает на уцелевших файлах");
        // копии не тронуты, новые файлы есть
        for (i, p) in parts.iter().enumerate() {
            let name = p.with_extension("wav").file_name().unwrap().to_os_string();
            let f2 = out2.join(&name);
            assert!(f2.is_file(), "итог {} существует", f2.display());
            if i % 2 == 0 {
                assert_eq!(
                    std::fs::read(&f2).unwrap(),
                    std::fs::read(out1.join(&name)).unwrap(),
                    "существующий результат побайтово цел (копия из прогона 1)"
                );
            }
        }

        // --- порядок событий: ранние Skipped ДО загрузки Pocket ---
        let events = events.lock().unwrap().clone();
        let pos_started = events
            .iter()
            .position(|e| matches!(e, JobEvent::BatchStarted { .. }))
            .expect("BatchStarted");
        let pos_ready = events
            .iter()
            .position(|e| matches!(e, JobEvent::BatchWorkersReady { .. }))
            .expect("BatchWorkersReady после загрузки пула");
        assert!(pos_started < pos_ready, "{events:?}");
        // «визуальный» счётчик по событиям (та же редукция, что apply_event)
        let mut finished = 0usize;
        let mut completed = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        let mut early_skip_after_ready = 0usize;
        for (pos, ev) in events.iter().enumerate() {
            if let JobEvent::FileDone { report, .. } = ev {
                match report.status {
                    FileStatus::Completed => completed += 1,
                    FileStatus::Failed => failed += 1,
                    FileStatus::Skipped => {
                        skipped += 1;
                        if pos > pos_ready
                            && report
                                .reason
                                .as_deref()
                                .is_some_and(|r| r.contains("существует"))
                        {
                            early_skip_after_ready += 1;
                        }
                    }
                    FileStatus::Cancelled => {}
                }
                finished += 1;
            }
        }
        assert_eq!(
            early_skip_after_ready, 0,
            "Skipped-«результат существует» решаются ДО загрузки воркеров"
        );
        // все предсуществующие пропуски пришли до BatchWorkersReady
        let skips_before_ready = events
            .iter()
            .take(pos_ready)
            .filter(|e| {
                matches!(
                    e,
                    JobEvent::FileDone { report, .. }
                        if report.status == FileStatus::Skipped
                )
            })
            .count();
        assert_eq!(skips_before_ready, preexisting, "{events:?}");
        if let JobEvent::BatchWorkersReady { workers } = &events[pos_ready] {
            assert_eq!(*workers, 2);
        }
        // счётчики событий == счётчикам отчёта (критерий G6.1.1)
        assert_eq!(finished, rep.total(), "finished == total отчёта");
        assert_eq!(completed, rep.completed());
        assert_eq!(skipped, rep.skipped());
        assert_eq!(failed, rep.failed());
        // FileStarted только для синтезированных
        let started = events
            .iter()
            .filter(|e| matches!(e, JobEvent::FileStarted { .. }))
            .count();
        assert_eq!(started, expect_synth);
        eprintln!(
            "[g611_partial] события: finished={finished} completed={completed} skipped={skipped} \
             (ранних до пула: {skips_before_ready}) failed={failed}; отчёт согласован"
        );
    }

    /// G6.1: отмена в середине пакета: новые файлы не назначаются, готовые
    /// ДО отмены файлы целы, повторный запуск доделывает остальные.
    #[test]
    #[ignore = "реальный пакетный синтез CUDA: модели + ORT GPU DLL"]
    fn real_batch_cuda_pool_cancel() {
        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let d = tmpdir("g61_pool_cancel");
        let mut s = p2b3_settings(&d, None);
        s.out_dir = d.join("out").display().to_string();
        s.clamped();

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_e = Arc::clone(&cancel);
        let done_files = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let done_e = Arc::clone(&done_files);
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(move |ev| {
            if let JobEvent::FileDone { .. } = ev {
                // «Стоп» после первого готового файла
                if done_e.fetch_add(1, Ordering::SeqCst) == 0 {
                    cancel_e.store(true, Ordering::SeqCst);
                }
            }
        });
        let res = execute_batch(&s, &src_dir, &cancel, &emit);
        eprintln!("[g61_cancel] {} | {}", res.status.label(), res.summary);
        assert_eq!(res.status, JobStatus::Cancelled, "{}", res.summary);
        let rep = res.batch.expect("отчёт");
        assert!(rep.completed() >= 1, "готовое до отмены не теряется");
        assert!(rep.skipped() >= 1, "назначение новых файлов остановлено");
        let completed = rep
            .files
            .iter()
            .filter(|f| f.status == FileStatus::Completed)
            .count();
        for f in rep
            .files
            .iter()
            .filter(|f| f.status == FileStatus::Completed)
        {
            let p = f.final_path.as_ref().expect("итог готового файла");
            assert!(p.is_file(), "{} цел после отмены", p.display());
        }
        // повторный запуск без отмены доделывает остальные
        let emit2: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
        let res2 = execute_batch(&s, &src_dir, &Arc::new(AtomicBool::new(false)), &emit2);
        eprintln!("[g61_retry] {} | {}", res2.status.label(), res2.summary);
        assert_eq!(res2.status, JobStatus::Completed, "{}", res2.summary);
        let rep2 = res2.batch.expect("отчёт");
        assert_eq!(rep2.completed(), 12 - completed, "{}", rep2.summary());
        assert_eq!(rep2.skipped(), completed, "готовые ранее — пропущены");
        assert_eq!(ffmpeg_process_count(), 0, "сиротских ffmpeg нет");
    }

    /// G6.1: перезапись в пакете — overwrite=false: существующие результаты
    /// пропускаются побайтово неизменными; overwrite=true: безопасная замена
    /// («файл обновлён») только после успешного синтеза.
    #[test]
    #[ignore = "реальный пакетный синтез CUDA: модели + ORT GPU DLL"]
    fn real_batch_cuda_pool_overwrite() {
        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let d = tmpdir("g61_pool_ow");
        let out = d.join("out");
        let mut s = p2b3_settings(&d, None);
        s.out_dir = out.display().to_string();
        s.clamped();
        let no_cancel = || Arc::new(AtomicBool::new(false));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});

        // 1) первый прогон — всё создаётся
        let r1 = execute_batch(&s, &src_dir, &no_cancel(), &emit);
        assert_eq!(r1.status, JobStatus::Completed, "{}", r1.summary);
        let before: Vec<Vec<u8>> = parts
            .iter()
            .map(|p| {
                let name = p.file_name().unwrap();
                std::fs::read(out.join(name).with_extension("wav")).unwrap()
            })
            .collect();

        // 2) повтор с overwrite=false: всё пропущено, байты нетронуты
        let r2 = execute_batch(&s, &src_dir, &no_cancel(), &emit);
        assert_eq!(r2.status, JobStatus::NoWork, "{}", r2.summary);
        for (i, p) in parts.iter().enumerate() {
            let name = p.file_name().unwrap();
            assert_eq!(
                std::fs::read(out.join(name).with_extension("wav")).unwrap(),
                before[i],
                "старые байты целы при выключенной перезаписи"
            );
        }

        // 3) повтор с overwrite=true: обновление после успешного синтеза
        let mut s3 = s.clone();
        s3.overwrite_existing = true;
        let r3 = execute_batch(&s3, &src_dir, &no_cancel(), &emit);
        assert_eq!(r3.status, JobStatus::Completed, "{}", r3.summary);
        let rep3 = r3.batch.expect("отчёт");
        assert_eq!(rep3.completed(), 12);
        assert!(rep3
            .files
            .iter()
            .all(|f| f.reason.as_deref().is_some_and(|r| r.contains("обновлён"))));
        // после замены не осталось .partial
        assert!(!std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".partial")));
        assert_eq!(ffmpeg_process_count(), 0, "сиротских ffmpeg нет");
    }

    /// G6.1: MP3/Opus в пакете на пуле — корректные контейнеры, без сирот.
    #[test]
    #[ignore = "реальный пакетный синтез CUDA: модели + ORT GPU DLL + ffmpeg"]
    fn real_batch_cuda_pool_mp3_opus() {
        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let ffmpeg0 = ffmpeg_process_count();
        for (fmt, magic) in [
            (OutputFormatChoice::Mp3, &b"ID3"[..]),
            (OutputFormatChoice::Opus, &b"OggS"[..]),
        ] {
            let d = tmpdir(&format!("g61_pool_{fmt:?}"));
            let mut s = p2b3_settings(&d, None);
            s.out_dir = d.join("out").display().to_string();
            s.format = fmt;
            s.clamped();
            let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
            let res = execute_batch(&s, &src_dir, &Arc::new(AtomicBool::new(false)), &emit);
            eprintln!("[g61_{fmt:?}] {} | {}", res.status.label(), res.summary);
            assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
            let rep = res.batch.expect("отчёт");
            assert_eq!(rep.completed(), 12);
            for f in &rep.files {
                let p = f.final_path.as_ref().expect("итог");
                let bytes = std::fs::read(p).unwrap();
                assert!(
                    bytes.starts_with(magic),
                    "контейнер {}: {}",
                    fmt.label(),
                    p.display()
                );
            }
        }
        assert_eq!(ffmpeg_process_count(), ffmpeg0, "сиротских ffmpeg нет");
    }

    // ===== G6.2: реальный гибридный пакет 2×2 (2 процесса × 2 воркера) =====

    /// Настройки гибрида: эталон P2B.3 + осознанный режим «Максимальная
    /// скорость». threads — по умолчанию 4 (серия t4 P2B.3).
    fn hybrid_settings(out_dir: &Path, threads: u32) -> GuiSettings {
        let mut s = p2b3_settings(out_dir, None);
        s.parallel_cuda = crate::settings::ParallelSettings {
            profile: crate::settings::ParallelProfile::Productive,
            processes: 2,
            workers_per_process: 2,
            max_concurrent_files: 4,
            ort_threads: threads,
        };
        s.clamped();
        s
    }

    /// G6.2: главный тест — пакет на гибриде 2×2: четырежды загруженный
    /// Pocket, два процесса, все WAV ПОБИТОВО равны экономичному пулу 1×2
    /// (который в G6.1 сверен с пофайловым путём), 77/77 чанков, VRAM
    /// возвращается к базе, ffmpeg не осиротел.
    #[test]
    #[ignore = "реальный гибридный синтез CUDA: модели + ORT GPU DLL (~100 с)"]
    fn real_batch_cuda_hybrid() {
        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let ffmpeg0 = ffmpeg_process_count();
        let vram0 = app_core::diagnostics::gpu_total_used_mb();
        let no_cancel = || Arc::new(AtomicBool::new(false));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});

        // 1) эталон: экономичный постоянный пул 1×2 (проверен G6.1 побитово)
        let d_ref = tmpdir("g62_ref12");
        let mut s_ref = p2b3_settings(&d_ref, None);
        s_ref.out_dir = d_ref.join("out").display().to_string();
        s_ref.clamped();
        let ref_res = execute_batch(&s_ref, &src_dir, &no_cancel(), &emit);
        eprintln!("[g62_ref] {} | {}", ref_res.status.label(), ref_res.summary);
        assert_eq!(ref_res.status, JobStatus::Completed, "{}", ref_res.summary);
        let ref_rep = ref_res.batch.expect("отчёт эталона");
        assert_eq!(ref_rep.pool_loads, Some(2));

        // 2) гибрид 2×2
        let d = tmpdir("g62_hybrid");
        let mut s = hybrid_settings(&d, 4);
        s.out_dir = d.join("out").display().to_string();
        s.clamped();
        let logs: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let emit2: Arc<dyn Fn(JobEvent) + Send + Sync> = {
            let logs = Arc::clone(&logs);
            Arc::new(move |ev| {
                if let JobEvent::Log(line) = ev {
                    logs.lock().unwrap().push(line);
                }
            })
        };
        let res = execute_batch(&s, &src_dir, &no_cancel(), &emit2);
        eprintln!("[g62_hybrid] {} | {}", res.status.label(), res.summary);
        let logs = logs.lock().unwrap().join("\n");
        if !logs.contains("VPAM-PREFLIGHT-SENTINEL") && !logs.contains("VRAM-префлайт 2×2")
        {
            // префлайт не прошёл — экономичный откат; тест бесполезен
            panic!("2×2 не запущен (VRAM-префлайт/фолбэк): {logs}");
        }
        assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
        let rep = res.batch.expect("отчёт гибрида");
        // ГЛАВНЫЙ КРИТЕРИЙ: 4 Pocket в 2 процессах
        assert_eq!(rep.pool_loads, Some(4), "{}", rep.summary());
        assert_eq!(rep.pool_processes, Some(2), "{}", rep.summary());
        assert_eq!(rep.pool_workers, Some(4), "{}", rep.summary());
        assert_eq!(rep.completed(), parts.len());
        // 3) побитовый паритет всех WAV гибрид == экономичный пул
        for f in &rep.files {
            let out_h = f.final_path.as_ref().expect("итог гибрида");
            let out_ref = d_ref.join("out").join(f.relative.with_extension("wav"));
            let a = std::fs::read(out_h).expect("WAV гибрида");
            let b = std::fs::read(&out_ref).expect("WAV эталона");
            assert_eq!(
                a,
                b,
                "{}: WAV гибрида обязан быть побитово равен пулу 1×2",
                f.relative.display()
            );
            assert!(a.starts_with(b"RIFF"));
        }
        // 4) чанки: 77 на main12 (или весь набор ctrl6)
        let chunks: usize = rep.files.iter().map(|f| f.chunks_total).sum();
        assert_eq!(chunks, 77, "сумма чанков набора");
        assert!(rep.files.iter().all(|f| f.chunks_done == f.chunks_total));
        // 5) без осиротевших ffmpeg/процессов; VRAM вернулась к базе
        assert_eq!(ffmpeg_process_count(), ffmpeg0, "сиротских ffmpeg нет");
        let vram1 = app_core::diagnostics::gpu_total_used_mb();
        if let (Some(v0), Some(v1)) = (vram0, vram1) {
            assert!(
                v1 < v0 + 2500.0,
                "VRAM после гибрида {v1:.0} МБ против базы {v0:.0} МБ — утечка"
            );
            eprintln!("[g62_vram] база {v0:.0} МБ → после гибрида {v1:.0} МБ");
        }
    }

    /// G6.2: отмена гибрида в середине пакета: новые файлы не назначаются,
    /// готовое до отмены цело, повторный запуск доделывает остальное.
    #[test]
    #[ignore = "реальный гибридный синтез CUDA (~120 с, два прогона)"]
    fn real_batch_cuda_hybrid_cancel() {
        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let d = tmpdir("g62_hybrid_cancel");
        let out = d.join("out");
        let mut s = hybrid_settings(&d, 4);
        s.out_dir = out.display().to_string();
        s.clamped();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_c = Arc::clone(&cancel);
        // отмена после ПЕРВОГО готового файла (пакет уже идёт)
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(move |ev| {
            if let JobEvent::FileDone { report, .. } = &ev {
                if report.status == crate::batch::FileStatus::Completed {
                    cancel_c.store(true, Ordering::SeqCst);
                }
            }
        });
        let r1 = execute_batch(&s, &src_dir, &cancel, &emit);
        eprintln!("[g62_cancel] {} | {}", r1.status.label(), r1.summary);
        assert_eq!(r1.status, JobStatus::Cancelled, "{}", r1.summary);
        let rep1 = r1.batch.expect("отчёт отмены");
        let completed1 = rep1.completed();
        assert!(completed1 >= 1, "готовое до отмены сохранено");
        assert!(rep1.is_cancelled());
        // каждый файл имеет терминальный результат
        assert_eq!(
            rep1.completed()
                + rep1.failed()
                + rep1.skipped()
                + rep1.count(crate::batch::FileStatus::Cancelled),
            parts.len()
        );
        // опубликованные до отмены WAV целы (RIFF)
        for f in &rep1.files {
            if let Some(p) = &f.final_path {
                assert!(std::fs::read(p).unwrap().starts_with(b"RIFF"));
            }
        }
        // повтор: готовое пропускается, остальное доделывается
        let noop: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
        let r2 = execute_batch(&s, &src_dir, &Arc::new(AtomicBool::new(false)), &noop);
        eprintln!("[g62_cancel_rerun] {} | {}", r2.status.label(), r2.summary);
        assert_eq!(r2.status, JobStatus::Completed, "{}", r2.summary);
        let rep2 = r2.batch.expect("отчёт повтора");
        assert_eq!(rep2.completed(), parts.len() - completed1);
        assert_eq!(rep2.skipped(), completed1);
        // политика проекта: .partial отменённых файлов СОХРАНЯЮТСЯ для
        // диагностики и не удаляются автоматически — их наличие ожидаемо
        assert!(
            std::fs::read_dir(&out)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".partial")),
            "черновики отмены сохранены для диагностики"
        );
        assert_eq!(ffmpeg_process_count(), 0, "сиротских ffmpeg нет");
    }

    /// G6.2: перезапись на гибриде — skip без изменений, overwrite=true
    /// атомарная замена после синтеза, без .partial.
    #[test]
    #[ignore = "реальный гибридный синтез CUDA (~120 с, три прогона)"]
    fn real_batch_cuda_hybrid_overwrite() {
        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let d = tmpdir("g62_hybrid_ow");
        let out = d.join("out");
        let mut s = hybrid_settings(&d, 4);
        s.out_dir = out.display().to_string();
        s.clamped();
        let no_cancel = || Arc::new(AtomicBool::new(false));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});

        let r1 = execute_batch(&s, &src_dir, &no_cancel(), &emit);
        assert_eq!(r1.status, JobStatus::Completed, "{}", r1.summary);
        let before: Vec<Vec<u8>> = parts
            .iter()
            .map(|p| {
                let name = p.file_name().unwrap();
                std::fs::read(out.join(name).with_extension("wav")).unwrap()
            })
            .collect();

        let r2 = execute_batch(&s, &src_dir, &no_cancel(), &emit);
        assert_eq!(r2.status, JobStatus::NoWork, "{}", r2.summary);
        for (i, p) in parts.iter().enumerate() {
            let name = p.file_name().unwrap();
            assert_eq!(
                std::fs::read(out.join(name).with_extension("wav")).unwrap(),
                before[i],
                "старые байты целы при выключенной перезаписи"
            );
        }

        let mut s3 = s.clone();
        s3.overwrite_existing = true;
        s3.clamped();
        let r3 = execute_batch(&s3, &src_dir, &no_cancel(), &emit);
        assert_eq!(r3.status, JobStatus::Completed, "{}", r3.summary);
        let rep3 = r3.batch.expect("отчёт");
        assert_eq!(rep3.completed(), parts.len());
        assert!(rep3
            .files
            .iter()
            .all(|f| f.reason.as_deref().is_some_and(|r| r.contains("обновлён"))));
        assert!(!std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".partial")));
        assert_eq!(ffmpeg_process_count(), 0, "сиротских ffmpeg нет");
    }

    /// G6.2: MP3/Opus на гибриде — корректные контейнеры, без сирот.
    #[test]
    #[ignore = "реальный гибридный синтез CUDA + ffmpeg (~150 с, две серии)"]
    fn real_batch_cuda_hybrid_mp3_opus() {
        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let ffmpeg0 = ffmpeg_process_count();
        for (fmt, magic) in [
            (OutputFormatChoice::Mp3, &b"ID3"[..]),
            (OutputFormatChoice::Opus, &b"OggS"[..]),
        ] {
            let d = tmpdir(&format!("g62_hybrid_{fmt:?}"));
            let mut s = hybrid_settings(&d, 4);
            s.out_dir = d.join("out").display().to_string();
            s.format = fmt;
            s.clamped();
            let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
            let res = execute_batch(&s, &src_dir, &Arc::new(AtomicBool::new(false)), &emit);
            eprintln!("[g62_{fmt:?}] {} | {}", res.status.label(), res.summary);
            if let Some(rep) = res.batch.as_ref() {
                for line in rep.detail_lines() {
                    eprintln!("[g62_{fmt:?}] {line}");
                }
            }
            assert_eq!(res.status, JobStatus::Completed, "{}", res.summary);
            let rep = res.batch.expect("отчёт");
            assert_eq!(rep.completed(), parts.len(), "{}", rep.summary());
            assert_eq!(rep.pool_processes, Some(2), "{}", rep.summary());
            for f in &rep.files {
                let p = f.final_path.as_ref().expect("итог");
                assert!(
                    std::fs::read(p).unwrap().starts_with(magic),
                    "контейнер {}: {}",
                    fmt.label(),
                    p.display()
                );
            }
        }
        assert_eq!(ffmpeg_process_count(), ffmpeg0, "сиротских ffmpeg нет");
    }

    /// G6.2: принудительная гибель дочернего PID во время реального пакета:
    /// его незавершённые файлы получают Failed с диагнозом, выживший процесс
    /// доделывает очередь, пакет завершается (не висит), процессов не остаётся.
    #[test]
    #[ignore = "реальный гибридный синтез CUDA с внедрённым сбоем (~60 с)"]
    fn real_batch_cuda_hybrid_child_killed() {
        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let d = tmpdir("g62_hybrid_kill");
        let mut s = hybrid_settings(&d, 4);
        s.out_dir = d.join("out").display().to_string();
        s.clamped();
        // гибельный PID получаем из лога диспетчера (процесс 1 подключается
        // вторым: «процесс 1: подключился (pid N)») и завершаем его на первом
        // старте файла — пакет уже идёт.
        let killed = Arc::new(AtomicBool::new(false));
        let killed_c = Arc::clone(&killed);
        let pid_cell: Arc<std::sync::Mutex<Option<u32>>> = Arc::new(std::sync::Mutex::new(None));
        let pid_parse = Arc::clone(&pid_cell);
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = {
            let killed_c = Arc::clone(&killed_c);
            let pid_parse = Arc::clone(&pid_parse);
            Arc::new(move |ev| match &ev {
                JobEvent::Log(line) => {
                    if let Some(rest) = line.strip_prefix("процесс 1: подключился (pid ")
                    {
                        if let Some(pid) =
                            rest.split(')').next().and_then(|v| v.parse::<u32>().ok())
                        {
                            *pid_parse.lock().unwrap() = Some(pid);
                        }
                    }
                }
                JobEvent::FileStarted { .. } => {
                    // одноразовое управляемое внедрение сбоя: первый FileStarted
                    let first = !killed_c.swap(true, Ordering::SeqCst);
                    let Some(pid) = first.then(|| *pid_parse.lock().unwrap()).flatten() else {
                        return;
                    };
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/F"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
                _ => {}
            })
        };
        let logs: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let emit2: Arc<dyn Fn(JobEvent) + Send + Sync> = {
            let logs = Arc::clone(&logs);
            let inner = Arc::clone(&emit);
            Arc::new(move |ev| {
                if let JobEvent::Log(l) = &ev {
                    logs.lock().unwrap().push(l.clone());
                }
                inner(ev);
            })
        };
        let res = execute_batch(&s, &src_dir, &Arc::new(AtomicBool::new(false)), &emit2);
        eprintln!("[g62_kill] {} | {}", res.status.label(), res.summary);
        assert!(killed.load(Ordering::SeqCst) && pid_cell.lock().unwrap().is_some());
        let rep = res.batch.expect("отчёт");
        assert_eq!(rep.total(), parts.len());
        // каждый файл терминален; часть — Failed, часть — Completed
        let completed = rep.completed();
        let failed = rep.failed();
        assert!(
            completed >= 1,
            "выживший процесс доделал часть: {}",
            rep.summary()
        );
        assert!(
            failed >= 1,
            "файлы убитого процесса Failed: {}",
            rep.summary()
        );
        assert_eq!(
            completed + failed + rep.skipped() + rep.count(crate::batch::FileStatus::Cancelled),
            parts.len()
        );
        let logs = logs.lock().unwrap().join("\n");
        assert!(
            logs.contains("дочерний процесс 1"),
            "диагностика гибели в журнале"
        );
        // терминальные Failed обязаны нести диагноз гибели
        for f in &rep.files {
            if f.status == crate::batch::FileStatus::Failed {
                let r = f.reason.as_deref().unwrap_or_default();
                assert!(
                    r.contains("процесс") || r.contains("воркер") || r.contains("заверш"),
                    "диагноз: {r}"
                );
            }
        }
        // ни один дочерний процесс не остался (tasklist по имени exe не растёт)
        assert_eq!(ffmpeg_process_count(), 0, "сиротских ffmpeg нет");
    }

    /// G6.1: замеры производительности пакетного CUDA на наборе main12 —
    /// прежний пофайловый путь (модель на каждый файл) против постоянного
    /// пула 1/2 воркера. Прогрев + 3 измеренных повтора на режим; в процессе
    /// сэмплируются GPU util/VRAM (nvidia-smi, ~0.3 с) и CPU (PowerShell,
    /// реже). wall — полный прогон пакета (включая загрузки Pocket).
    #[test]
    #[ignore = "длительный бенчмарк CUDA (~10 мин): модели + ORT GPU DLL"]
    fn real_batch_cuda_bench() {
        fn gpu_sample() -> Option<(u64, u64)> {
            let out = std::process::Command::new("nvidia-smi")
                .args([
                    "--query-gpu=utilization.gpu,memory.used",
                    "--format=csv,noheader,nounits",
                ])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let t = String::from_utf8_lossy(&out.stdout);
            let mut it = t.lines().next()?.trim().split(',');
            Some((
                it.next()?.trim().parse().ok()?,
                it.next()?.trim().parse().ok()?,
            ))
        }
        fn cpu_sample() -> Option<u64> {
            let out = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "(Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter \"Name='_Total'\" -Property PercentProcessorTime).PercentProcessorTime",
                ])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()
        }

        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let base = tmpdir("g61_bench");
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(|_| {});
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(std::sync::Mutex::new((0u64, 0u64, 0u64, 0u64, 0u64)));
        // (max_vram_mb, sum_util, n_util, sum_cpu, n_cpu)
        {
            let stop = Arc::clone(&stop);
            let stats = Arc::clone(&stats);
            std::thread::spawn(move || {
                let mut i = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    if let Some((util, used)) = gpu_sample() {
                        let mut s = stats.lock().unwrap();
                        s.0 = s.0.max(used);
                        s.1 += util;
                        s.2 += 1;
                    }
                    if i.is_multiple_of(4) {
                        if let Some(cpu) = cpu_sample() {
                            let mut s = stats.lock().unwrap();
                            s.3 += cpu;
                            s.4 += 1;
                        }
                    }
                    i += 1;
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            });
        }

        // (имя, прежний путь?, workers): old1 — модель на каждый файл;
        // pool1/pool2 — постоянный пул с 1/2 Pocket.
        let modes: [(&str, bool, Option<u32>); 3] = [
            ("old1", true, Some(1)),
            ("pool1", false, Some(1)),
            ("pool2", false, None),
        ];
        for (name, old, workers) in modes {
            if old {
                std::env::set_var("SHTORM_GUI_BATCH_POOL", "0");
            } else {
                std::env::remove_var("SHTORM_GUI_BATCH_POOL");
            }
            for rep in 0..=3u32 {
                let d = base.join(format!("{name}_r{rep}"));
                let mut s = p2b3_settings(&d, workers);
                s.out_dir = d.join("out").display().to_string();
                s.clamped();
                {
                    let mut s = stats.lock().unwrap();
                    *s = (0, 0, 0, 0, 0);
                }
                let t0 = Instant::now();
                let res = execute_batch(&s, &src_dir, &Arc::new(AtomicBool::new(false)), &emit);
                let wall = t0.elapsed().as_secs_f32();
                assert_eq!(
                    res.status,
                    JobStatus::Completed,
                    "{name} r{rep}: {}",
                    res.summary
                );
                // длительность аудио набора — по размерам WAV (PCM16 mono 24 кГц)
                let audio_s: f64 = std::fs::read_dir(d.join("out"))
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .to_ascii_lowercase()
                            .ends_with(".wav")
                    })
                    .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                    .map(|len| len.saturating_sub(44) as f64 / 48_000.0)
                    .sum();
                let (vram_max, util_avg, cpu_avg) = {
                    let s = stats.lock().unwrap();
                    (
                        s.0,
                        if s.2 > 0 {
                            s.1 as f64 / s.2 as f64
                        } else {
                            0.0
                        },
                        if s.4 > 0 {
                            s.3 as f64 / s.4 as f64
                        } else {
                            0.0
                        },
                    )
                };
                let rep_kind = if rep == 0 { "warmup" } else { "measured" };
                eprintln!(
                    "BENCH {name} {rep_kind}: wall {wall:.2} s | аудио {audio_s:.1} с | \
                     throughput {:.2} реч-с/с | VRAM max {vram_max} МБ | GPU util {util_avg:.0}% | \
                     CPU {cpu_avg:.0}% | {}",
                    audio_s / wall as f64,
                    res.summary
                );
            }
        }
        stop.store(true, Ordering::Relaxed);
        std::env::remove_var("SHTORM_GUI_BATCH_POOL");
    }

    /// G6.2 §16: бенчмарк РЕАЛЬНОГО GUI-пути — штатный экономичный режим
    /// 1×2 против гибрида 2×2 (threads=4 и threads=2) на наборе main12.
    /// Прогрев + 3 измеренных повтора на режим, каждый — новый каталог.
    /// Cold wall — полный `execute_batch` (включая загрузки Pocket);
    /// Warm wall — от `BatchWorkersReady` (все воркеры готовы) до последнего
    /// `FileDone`: ни загрузок моделей, ни preflight в окне (методика P2B.3,
    /// но в событиях GUI, а не в межпроцессном гейте). Побитовая сверка всех
    /// WAV каждого прогона с эталоном экономичного режима.
    #[test]
    #[ignore = "длительный бенчмарк CUDA (~15 мин): 3 режима × (прогрев + 3 повтора)"]
    fn real_batch_cuda_hybrid_bench() {
        fn gpu_sample() -> Option<(u64, u64)> {
            let out = std::process::Command::new("nvidia-smi")
                .args([
                    "--query-gpu=utilization.gpu,memory.used",
                    "--format=csv,noheader,nounits",
                ])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let t = String::from_utf8_lossy(&out.stdout);
            let mut it = t.lines().next()?.trim().split(',');
            Some((
                it.next()?.trim().parse().ok()?,
                it.next()?.trim().parse().ok()?,
            ))
        }
        fn cpu_sample() -> Option<u64> {
            let out = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "(Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter \"Name='_Total'\" -Property PercentProcessorTime).PercentProcessorTime",
                ])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()
        }

        let parts = p2b3_parts();
        let src_dir = parts[0].parent().unwrap().to_path_buf();
        let base = tmpdir("g62_bench");
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(std::sync::Mutex::new((0u64, 0u64, 0u64, 0u64, 0u64)));
        // (max_vram_mb, sum_util, n_util, sum_cpu, n_cpu)
        {
            let stop = Arc::clone(&stop);
            let stats = Arc::clone(&stats);
            std::thread::spawn(move || {
                let mut i = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    if let Some((util, used)) = gpu_sample() {
                        let mut s = stats.lock().unwrap();
                        s.0 = s.0.max(used);
                        s.1 += util;
                        s.2 += 1;
                    }
                    if i.is_multiple_of(4) {
                        if let Some(cpu) = cpu_sample() {
                            let mut s = stats.lock().unwrap();
                            s.3 += cpu;
                            s.4 += 1;
                        }
                    }
                    i += 1;
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            });
        }

        // (имя, режим, threads): экономичный 1×2 и гибрид 2×2 в двух профилях
        let economy = crate::settings::ParallelProfile::Optimal;
        let maxspeed = crate::settings::ParallelProfile::Productive;
        let modes: [(&str, crate::settings::ParallelProfile, u32); 3] = [
            ("gui1x2_t4", economy, 4),
            ("gui2x2_t4", maxspeed, 4),
            ("gui2x2_t2", maxspeed, 2),
        ];
        // эталон байтов: первый успешный прогон экономичного режима
        let mut reference: Option<Vec<(PathBuf, Vec<u8>)>> = None;
        let mut table: Vec<String> = Vec::new();
        for (name, mode, threads) in modes {
            for rep in 0..=3u32 {
                let d = base.join(format!("{name}_r{rep}"));
                let mut s = p2b3_settings(&d, None);
                s.out_dir = d.join("out").display().to_string();
                s.parallel_cuda.profile = mode;
                s.parallel_cuda.ort_threads = threads;
                s.clamped();
                {
                    let mut st = stats.lock().unwrap();
                    *st = (0, 0, 0, 0, 0);
                }
                // warm-окно: готовность всех воркеров → последний FileDone
                let ready_at: Arc<std::sync::Mutex<Option<Instant>>> =
                    Arc::new(std::sync::Mutex::new(None));
                let last_done: Arc<std::sync::Mutex<Option<Instant>>> =
                    Arc::new(std::sync::Mutex::new(None));
                let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = {
                    let ready_at = Arc::clone(&ready_at);
                    let last_done = Arc::clone(&last_done);
                    Arc::new(move |ev| match &ev {
                        JobEvent::BatchWorkersReady { .. } => {
                            let mut g = ready_at.lock().unwrap();
                            if g.is_none() {
                                *g = Some(Instant::now());
                            }
                        }
                        JobEvent::FileDone { .. } => {
                            *last_done.lock().unwrap() = Some(Instant::now());
                        }
                        _ => {}
                    })
                };
                let t0 = Instant::now();
                let res = execute_batch(&s, &src_dir, &Arc::new(AtomicBool::new(false)), &emit);
                let cold = t0.elapsed().as_secs_f32();
                assert_eq!(
                    res.status,
                    JobStatus::Completed,
                    "{name} r{rep}: {}",
                    res.summary
                );
                let report = res.batch.expect("отчёт");
                assert!(report.completed() == parts.len(), "{}", report.summary());
                let warm = match (*ready_at.lock().unwrap(), *last_done.lock().unwrap()) {
                    (Some(r), Some(l)) if l > r => Some((l - r).as_secs_f32()),
                    _ => None,
                };
                // аудио набора — по размерам WAV (PCM16 mono 24 кГц)
                let audio_s: f64 = std::fs::read_dir(d.join("out"))
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .to_ascii_lowercase()
                            .ends_with(".wav")
                    })
                    .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                    .map(|len| len.saturating_sub(44) as f64 / 48_000.0)
                    .sum();
                // побитовая сверка с эталоном (равносильно сверке SHA256)
                let mut writes: Vec<(PathBuf, Vec<u8>)> = Vec::new();
                for part in &parts {
                    let name_f = part.file_name().unwrap();
                    let p = d.join("out").join(name_f).with_extension("wav");
                    let bytes = std::fs::read(&p).unwrap_or_default();
                    writes.push((p.clone(), bytes));
                }
                match &reference {
                    None => reference = Some(writes.clone()),
                    Some(refer) => {
                        for (i, (p, bytes)) in writes.iter().enumerate() {
                            assert_eq!(
                                bytes,
                                &refer[i].1,
                                "{name} r{rep}: {} побитово отличается от эталона 1×2",
                                p.display()
                            );
                        }
                    }
                }
                let (vram_max, util_avg, cpu_avg) = {
                    let st = stats.lock().unwrap();
                    (
                        st.0,
                        if st.2 > 0 {
                            st.1 as f64 / st.2 as f64
                        } else {
                            0.0
                        },
                        if st.4 > 0 {
                            st.3 as f64 / st.4 as f64
                        } else {
                            0.0
                        },
                    )
                };
                let kind = if rep == 0 { "warmup" } else { "measured" };
                let line = format!(
                    "BENCH {name} {kind}: cold {cold:.2} s | warm {} | аудио {audio_s:.1} с | \
                     throughput {:.2} реч-с/с (cold) | VRAM max {vram_max} МБ | GPU util \
                     {util_avg:.0}% | CPU {cpu_avg:.0}% | Pocket {} | процессов {} | {}",
                    warm.map(|w| format!("{w:.2} s (всего {:.2} реч-с/с)", audio_s / w as f64))
                        .unwrap_or_else(|| "-".into()),
                    audio_s / cold as f64,
                    report
                        .pool_loads
                        .map(|v| v.to_string())
                        .unwrap_or("-".into()),
                    report
                        .pool_processes
                        .map(|v| v.to_string())
                        .unwrap_or("-".into()),
                    report.summary()
                );
                eprintln!("{line}");
                if rep > 0 {
                    table.push(line);
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        eprintln!("==== ИТОГИ G6.2 BENCH (3 измеренных повтора) ====");
        for line in &table {
            eprintln!("{line}");
        }
    }
}
