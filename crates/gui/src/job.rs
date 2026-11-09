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

use app_core::pipeline::{
    log_path_for, synthesize_stream, write_chunk_log, write_diag_log, ChunkLogMeta, DiagMeta,
    PipelineOptions, RunStatus, StreamError,
};
use app_core::sink::{make_sink, OutputFormat};
use shtorm_engine::Device;
use text_processing::CharRules;

use crate::batch::{
    plan_batch, run_batch, BatchEvent, BatchReport, BatchStatus, FileReport, FileStatus,
};
use crate::resources;
use crate::settings::{DeviceChoice, GuiSettings, ModelChoice, OutputFormatChoice};

// G4: единый резолвер ресурсов; реэкспорт сохраняет существующий API/тесты.
pub use crate::resources::{find_project_root, resolve_ort_dylib, resolve_ort_dylib_in};

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
    /// Пакетный режим: прогресс чанков текущего файла + номер файла K/N.
    FileProgress {
        index: usize,
        total: usize,
        done: usize,
        chunks_total: usize,
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
}

impl JobStatus {
    pub fn label(&self) -> &'static str {
        match self {
            JobStatus::Completed => "Завершено",
            JobStatus::Partial => "Частично",
            JobStatus::Cancelled => "Отменено",
            JobStatus::Failed => "Ошибка",
            JobStatus::NoWork => "Нет работы",
        }
    }
}

/// Каталог модели по выбору Fast/Slow/Custom; проверка обязательных файлов
/// движка — как валидация G1, но тем же кодом для реального запуска.
pub fn resolve_model_dir(s: &GuiSettings) -> Result<PathBuf> {
    let dir = match s.model {
        ModelChoice::Fast => resources::resolve_model_base(s)?.join("ru-v2-fast"),
        ModelChoice::Slow => resources::resolve_model_base(s)?.join("ru-v2-slow"),
        ModelChoice::Custom => {
            let p = resources::resolve_user_path(&s.model_dir);
            anyhow::ensure!(
                !p.as_os_str().is_empty(),
                "для модели Custom не задан каталог model_dir"
            );
            p
        }
    };
    anyhow::ensure!(
        dir.join("flow_lm_main.onnx").is_file(),
        "в каталоге модели нет flow_lm_main.onnx: {}",
        dir.display()
    );
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
    if !ort::init().with_name("shtorm-gui").commit() {
        // init не состоялся — не оставляем процесс с чужим DLL/путём
        match old_dylib {
            Some(v) => std::env::set_var("ORT_DYLIB_PATH", v),
            None => std::env::remove_var("ORT_DYLIB_PATH"),
        }
        match old_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
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
    // папка результата может не существовать — создаём (безопасно, без удалений)
    if let Some(parent) = cfg.out_path.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return attach_warning(
                    fail(format!(
                        "создать папку результата {}: {e}",
                        parent.display()
                    )),
                    &char_warning,
                );
            }
        }
    }
    if cfg.out_path.exists() {
        return attach_warning(
            fail(format!(
                "итоговый файл уже существует: {} — не перезаписываем (переименуйте или смените папку)",
                cfg.out_path.display()
            )),
            &char_warning,
        );
    }
    let ffmpeg_path = if s.ffmpeg_path.trim().is_empty() {
        None
    } else {
        // относительный путь из настроек — от каталога exe
        Some(resources::resolve_user_path(&s.ffmpeg_path))
    };
    let preset = s.preset_for_format();
    let sink = match make_sink(
        &cfg.out_path,
        24_000,
        output_format(s.format),
        preset,
        ffmpeg_path.as_deref(),
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
    attach_warning(res, &char_warning)
}

/// G3: пакетный прогон. Снимок файлов строится ДО первого синтеза; каждый
/// файл идёт через `execute_job` (пул воркеров создаётся/освобождается на
/// файл — существующий жизненный цикл). Ошибки изолированы, отмена
/// останавливает текущий файл и не запускает следующий.
pub fn execute_batch(
    settings: &GuiSettings,
    source: &Path,
    cancel: &Arc<AtomicBool>,
    emit: &dyn Fn(JobEvent),
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
            batch: Some(report),
        };
    }

    let ext = settings.format.extension_str().to_string();
    let report = run_batch(
        &plan,
        &ext,
        cancel,
        &|ev| match ev {
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
                emit(JobEvent::Log(format!(
                    "[{index}/{total}] старт: {} → {}",
                    input.display(),
                    output.display()
                )));
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
        },
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
                JobEvent::Log(line) => {
                    emit(JobEvent::Log(format!("[{}] {line}", file.input.display())));
                }
                _ => {}
            });
            let mut rep = FileReport::new(file, Some(out_path.to_path_buf()));
            rep.chunks_done = res.progress.0;
            rep.chunks_total = res.progress.1;
            rep.final_path = res.final_path;
            rep.partial_path = res.partial_path;
            rep.diag_path = res.diag_path;
            rep.chunk_log_path = res.chunk_log_path;
            match res.status {
                JobStatus::Completed => rep.status = FileStatus::Completed,
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
                JobStatus::NoWork => {
                    rep.status = FileStatus::Skipped;
                    rep.reason = Some(res.summary);
                }
            }
            rep
        },
    );

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
        batch: Some(report),
    }
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
            batch: None,
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
        batch: None,
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
        let res = execute_batch(&s, &src, &cancel, &|_| {});
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
        let res = execute_batch(&s, &src, &cancel, &|_| {});
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
        let res = execute_batch(&s, &src, &cancel, &|_| {});
        assert_eq!(res.status, JobStatus::Cancelled, "{}", res.summary);
        assert!(res.summary.contains("ОТМЕНЕНО"), "{}", res.summary);
        let report = res.batch.expect("отчёт");
        assert!(report.cancelled);
        assert_eq!(report.skipped(), 2);
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
        let events = std::cell::RefCell::new(Vec::new());
        let res = execute_batch(&s, &src, &cancel, &|ev| events.borrow_mut().push(ev));
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
        let events = events.into_inner();
        let file_starts = events
            .iter()
            .filter(|e| matches!(e, JobEvent::FileProgress { .. }))
            .count();
        assert!(file_starts >= 3, "прогресс чанков по файлам: {file_starts}");
        assert!(events
            .iter()
            .any(|e| matches!(e, JobEvent::FileDone { .. })));
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
    /// cargo test -p shtorm-gui --release --lib real_eos_parity -- --ignored --nocapture
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
}
