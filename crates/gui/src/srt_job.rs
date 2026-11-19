//! SRT.6: исполнение SRT-заданий GUI — одиночный файл и каталог (рекурсивно).
//! Переиспользует подготовку/безопасность путей/ORT/постоянный пул
//! обычного TXT-пути (`job.rs`) и SRT-конвейер app-core (`srt_pipeline`).
//!
//! Отличия от TXT: источник — только `.srt` (байты читает вызывающий код,
//! декодирование utf-8-sig/lossy — `decode_srt_bytes`); выход — один
//! WAV/MP3/Opus на файл с выравниванием по таймкодам; лог чанков для SRT —
//! лог ТАЙМИНГОВ (`write_srt_timing_log`); мультипроцессный chunkpipe для
//! SRT-заданий не используется (документированное ограничение этапа: P>1
//! обрабатывается однопроцессным пулом с предупреждением в журнале).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use app_core::persistent::{live_slots, pooled_real_worker_factory, spawn_workers, WorkerSlot};
use app_core::pipeline::{RunStatus, StreamError};
use app_core::sink::make_sink_with_overwrite;
use app_core::soundtouch::SoundTouchLib;
use app_core::srt::SrtTimingParams;
use app_core::srt_pipeline::{synthesize_srt_stream, write_srt_timing_log, SrtOptions, SrtOutcome};
use app_core::worker::real_worker_factory;

use crate::batch::{BatchReport, FileReport, FileStatus};
use crate::job::{
    create_dir_all_no_reparse, ensure_output_path_safe, load_char_rules, output_format,
    pipeline_options, resolve_model_dir, setup_ort_environment, CharRulesLoad, JobEvent, JobResult,
    JobStatus,
};
use crate::resources;
use crate::settings::GuiSettings;

/// Конфигурация одиночного SRT-задания (байты читает GUI до запуска).
#[derive(Debug, Clone)]
pub struct SrtJobConfig {
    pub settings: GuiSettings,
    /// Сырые байты .srt (BOM/битые байты обрабатывает `decode_srt_bytes`).
    pub srt_bytes: Vec<u8>,
    /// Имя исходного файла для журналов (не путь).
    pub source_name: String,
    /// Путь результата (stem задаёт и имя лога таймингов).
    pub out_path: PathBuf,
}

/// SRT-настройки конвейера из GUI-настроек (диапазоны уже проверены
/// `GuiSettings::clamped`).
pub fn srt_options(s: &GuiSettings) -> SrtOptions {
    SrtOptions {
        timing: SrtTimingParams {
            sample_rate: 24_000,
            max_tempo: s.srt_max_tempo,
            min_pause_ms: s.srt_min_pause_ms,
        },
        internal_pause_ms: s.srt_internal_pause_ms,
        comma_pause_ms: s.srt_comma_pause_ms,
        max_cue_pcm_samples: 15 * 60 * 24_000,
    }
}

/// Одиночный SRT-файл: собственный пул воркеров на задание (зеркало
/// `execute_job` TXT-пути: безопасность путей до создания каталогов,
/// повторная валидация, skip/overwrite семантика G5).
/// Общий тип эмиттера событий (как `job::execute_batch`).
pub type SharedEmit = Arc<dyn Fn(JobEvent) + Send + Sync>;

pub fn execute_srt_job(
    cfg: &SrtJobConfig,
    cancel: &Arc<AtomicBool>,
    emit: &SharedEmit,
) -> JobResult {
    let t0 = Instant::now();
    let s = &cfg.settings;
    let progress = std::cell::Cell::new((0usize, 0usize));
    let fail = |summary: String| {
        let mut r = JobResult::failed(summary);
        r.progress = progress.get();
        r
    };

    let model_dir = match resolve_model_dir(s) {
        Ok(d) => d,
        Err(e) => return fail(format!("{e:#}")),
    };
    emit(JobEvent::Log(format!("модель: {}", model_dir.display())));
    let char_rules = match load_char_rules(&model_dir) {
        CharRulesLoad::Loaded(rules) => Some(rules),
        CharRulesLoad::Absent => None,
        CharRulesLoad::Warning(w) => {
            emit(JobEvent::Log(format!("ВНИМАНИЕ: {w}")));
            None
        }
    };
    if let Err(e) = setup_ort_environment(s) {
        return fail(format!("{e:#}"));
    }
    if let Err(e) = ensure_output_path_safe(s, &cfg.out_path, &model_dir) {
        return fail(format!("{e:#}"));
    }
    if let Some(parent) = cfg.out_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            if let Err(e) = create_dir_all_no_reparse(parent) {
                return fail(format!("{e:#}"));
            }
        }
    }
    if let Err(e) = ensure_output_path_safe(s, &cfg.out_path, &model_dir) {
        return fail(format!("{e:#}"));
    }
    let overwrite = s.overwrite_existing;
    let existed_before = cfg.out_path.exists();
    if existed_before && !overwrite {
        return JobResult::skipped(format!(
            "Пропущено: файл уже существует: {} (перезапись выключена)",
            cfg.out_path.display()
        ));
    }
    if existed_before {
        emit(JobEvent::Log(format!(
            "перезапись разрешена: {} — старый файл будет заменён только после успешного синтеза",
            cfg.out_path.display()
        )));
    }

    let soundtouch = load_soundtouch(emit);
    let par = s.parallel_config();
    if par.processes > 1 {
        emit(JobEvent::Log(format!(
            "ВНИМАНИЕ: SRT-задания используют однопроцессный пул (запрошено процессов: {})",
            par.processes
        )));
    }
    let mut pool_settings = s.clone();
    pool_settings.threads = par.ort_threads;
    let pool_opts = pipeline_options(&pool_settings, model_dir, char_rules);
    let pockets = 1.max(par.workers_per_process.min(par.max_concurrent_chunks) as usize);
    emit(JobEvent::Log(format!(
        "постоянный чанк-пул: загрузка {pockets} Pocket (W={}, C={}, T={})…",
        par.workers_per_process, par.max_concurrent_chunks, par.ort_threads
    )));
    let t_pool = Instant::now();
    let spawned = spawn_workers(pockets, real_worker_factory(&pool_opts, 42), 42);
    if !spawned.failures.is_empty() {
        let msg = spawned
            .failures
            .iter()
            .map(|(w, e)| format!("воркер {w}: {e}"))
            .collect::<Vec<_>>()
            .join("; ");
        return fail(format!("пул не запущен: {msg}"));
    }
    emit(JobEvent::Log(format!(
        "постоянный чанк-пул: {pockets} Pocket готовы за {:.1} с",
        t_pool.elapsed().as_secs_f32()
    )));
    let slots: Arc<Vec<WorkerSlot<app_core::worker::RealWorker>>> = Arc::new(spawned.slots);

    let res = run_srt_file_on_pool(
        s,
        &slots,
        &pool_opts,
        &cfg.srt_bytes,
        &cfg.source_name,
        &cfg.out_path,
        soundtouch.as_ref(),
        existed_before,
        cancel,
        &|done, total| {
            progress.set((done, total));
            emit(JobEvent::Progress { done, total });
        },
        emit,
    );
    let mut res = res;
    res.progress = progress.get();
    let _ = t0;
    res
}

/// Один файл на ГОТОВОМ постоянном пуле (используется и одиночным заданием,
/// и пакетом). Возвращает JobResult с фазами/логом таймингов.
#[allow(clippy::too_many_arguments)]
fn run_srt_file_on_pool(
    s: &GuiSettings,
    slots: &Arc<Vec<WorkerSlot<app_core::worker::RealWorker>>>,
    pool_opts: &app_core::pipeline::PipelineOptions,
    srt_bytes: &[u8],
    source_name: &str,
    out_path: &Path,
    soundtouch: Option<&SoundTouchLib>,
    existed_before: bool,
    cancel: &AtomicBool,
    progress: &dyn Fn(usize, usize),
    emit: &SharedEmit,
) -> JobResult {
    let content = text_processing::decode_srt_bytes(srt_bytes);
    let ffmpeg_path = if s.ffmpeg_path.trim().is_empty() {
        None
    } else {
        Some(resources::resolve_user_path(&s.ffmpeg_path))
    };
    let preset = s.preset_for_format();
    let sink = match make_sink_with_overwrite(
        out_path,
        24_000,
        output_format(s.format),
        preset,
        ffmpeg_path.as_deref(),
        s.overwrite_existing,
    ) {
        Ok(sink) => sink,
        Err(e) => return JobResult::failed(format!("{e:#}")),
    };
    let srt_opts = srt_options(s);
    let factory = pooled_real_worker_factory(Arc::clone(slots));
    let live = live_slots(slots);

    let run = synthesize_srt_stream(
        &content,
        pool_opts,
        &srt_opts,
        live,
        sink,
        soundtouch,
        progress,
        cancel,
        &|ph| emit(JobEvent::Phase(ph.into())),
        factory,
    );
    match run {
        Ok(o) => finish_srt_outcome(s, o, source_name, out_path, existed_before),
        Err(e) => finish_srt_error(&e),
    }
}

/// Итог SRT-прогона → JobResult (семантика сообщений G5/G5.1).
fn finish_srt_outcome(
    s: &GuiSettings,
    o: SrtOutcome,
    source_name: &str,
    out_path: &Path,
    existed_before: bool,
) -> JobResult {
    let track_s = o
        .timing_rows
        .last()
        .map(|d| d.actual_end_samples as f64 / o.sample_rate as f64)
        .unwrap_or(0.0);
    let speech_s = o.speech_samples as f64 / o.sample_rate as f64;
    if o.status == RunStatus::Cancelled {
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
            diag_path: None,
            chunk_log_path: None,
            rtf: None,
            progress: (o.chunks_done, o.total_chunks),
            replaced_existing: false,
            batch: None,
        };
    }
    // Completed
    let (prefix, replaced) = if existed_before {
        ("ФАЙЛ ОБНОВЛЁН (старый заменён)", true)
    } else {
        ("ГОТОВО (создан новый файл)", false)
    };
    let late_cues = o
        .timing_rows
        .iter()
        .filter(|d| d.late_by_samples > 0)
        .count();
    let tempo_cues = o.timing_rows.iter().filter(|d| d.tempo.is_some()).count();
    let mut summary = format!(
        "{prefix}: {} (дорожка {:.2} с: речь {:.2} с; реплик {}/{}; чанков {}; ускорено {}; опозданий {})",
        out_path.display(),
        track_s,
        speech_s,
        o.cues_voiced,
        o.cues_total,
        o.total_chunks,
        tempo_cues,
        late_cues,
    );
    for w in &o.warnings {
        summary.push_str(&format!("; ВНИМАНИЕ: {w}"));
    }
    let mut chunk_log_path = None;
    if s.chunk_log {
        match write_srt_timing_log(out_path, source_name, &o) {
            Ok(p) => chunk_log_path = Some(p),
            Err(e) => summary.push_str(&format!("; ВНИМАНИЕ: лог таймингов не создан: {e:#}")),
        }
    }
    JobResult {
        status: JobStatus::Completed,
        summary,
        final_path: o.path,
        partial_path: None,
        diag_path: None,
        chunk_log_path,
        rtf: None,
        progress: (o.chunks_done, o.total_chunks),
        replaced_existing: replaced,
        batch: None,
    }
}

fn finish_srt_error(e: &anyhow::Error) -> JobResult {
    if let Some(se) = e.downcast_ref::<StreamError>() {
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
            diag_path: None,
            chunk_log_path: None,
            rtf: None,
            progress: (se.chunks_done, se.total),
            replaced_existing: false,
            batch: None,
        };
    }
    JobResult::failed(format!("ОШИБКА: {e:#}"))
}

/// Загрузка SoundTouchDLL: портативно — рядом с exe; разработка —
/// third_party/soundtouch. Отсутствие НЕ фатально (файлы без ускорения
/// обрабатываются), при первом ускорении будет явная ошибка реплики.
fn load_soundtouch(emit: &SharedEmit) -> Option<SoundTouchLib> {
    let explicit = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("SoundTouchDLL.dll")))
        .filter(|p| p.is_file());
    match SoundTouchLib::load(explicit.as_deref()) {
        Ok(lib) => {
            emit(JobEvent::Log(format!(
                "SoundTouch {} (dll {})",
                lib.version_string(),
                lib.version_id()
            )));
            Some(lib)
        }
        Err(e) => {
            emit(JobEvent::Log(format!(
                "ВНИМАНИЕ: SoundTouchDLL не загружена ({e}); ускорение реплик будет невозможно"
            )));
            None
        }
    }
}

/// Рекурсивная обработка каталога .srt: файлы строго последовательно,
/// чанки текущего файла — на ОДНОМ постоянном пуле (как TXT-батч);
/// структура подкаталогов сохраняется в каталоге результата.
pub fn execute_srt_batch(
    settings: &GuiSettings,
    source_dir: &Path,
    cancel: &Arc<AtomicBool>,
    emit: &SharedEmit,
) -> JobResult {
    let s = settings;
    let out_root = resources::resolve_user_path(&s.out_dir);
    // SRT.9B: корень обязан читаться — отказ доступа/отсутствие НЕ
    // маскируются под «нет файлов .srt» (раньше read_dir молча вернул
    // пустой список).
    if let Err(e) = std::fs::read_dir(source_dir) {
        return JobResult::failed(format!(
            "каталог-источник не читается {}: {e}",
            source_dir.display()
        ));
    }
    // SRT.9B: безопасный обход общей инфраструктурой batch::plan_dir_tree —
    // без следования по symlink/junction/reparse-точкам (пропуск с
    // предупреждением), с исключением вложенного выходного каталога и
    // проверкой совпадения источника и выхода.
    let (planned, warnings) = match crate::batch::plan_dir_tree(source_dir, &out_root, is_srt_name)
    {
        Ok(v) => v,
        Err(e) => return JobResult::failed(format!("{e:#}")),
    };
    for w in &warnings {
        emit(JobEvent::Log(format!("ВНИМАНИЕ: {w}")));
    }
    let files: Vec<(PathBuf, PathBuf)> =
        planned.into_iter().map(|f| (f.input, f.relative)).collect();
    if files.is_empty() {
        return JobResult::failed(format!(
            "в каталоге {} нет файлов .srt",
            source_dir.display()
        ));
    }
    let total = files.len();
    emit(JobEvent::BatchStarted { total, workers: 0 });

    let model_dir = match resolve_model_dir(s) {
        Ok(d) => d,
        Err(e) => return JobResult::failed(format!("{e:#}")),
    };
    let char_rules = match load_char_rules(&model_dir) {
        CharRulesLoad::Loaded(rules) => Some(rules),
        CharRulesLoad::Absent => None,
        CharRulesLoad::Warning(w) => {
            emit(JobEvent::Log(format!("ВНИМАНИЕ: {w}")));
            None
        }
    };
    if let Err(e) = setup_ort_environment(s) {
        return JobResult::failed(format!("{e:#}"));
    }
    // Каталоги результата: проверка безопасности ДО создания (G5.1), затем
    // создание без следования по ссылкам и повторная проверка (как в
    // одиночном пути execute_srt_job).
    let mut out_dirs: Vec<PathBuf> = Vec::new();
    for (_, rel) in &files {
        let out_file = out_root.join(rel.with_extension(s.format.extension_str()));
        let dir = out_file.parent().unwrap_or(&out_root).to_path_buf();
        if !out_dirs.contains(&dir) {
            out_dirs.push(dir);
        }
    }
    for dir in &out_dirs {
        if let Err(e) = ensure_output_path_safe(s, dir, &model_dir) {
            return JobResult::failed(format!("небезопасный путь результата: {e:#}"));
        }
        if !dir.as_os_str().is_empty() && !dir.exists() {
            if let Err(e) = create_dir_all_no_reparse(dir) {
                return JobResult::failed(format!("{e:#}"));
            }
        }
        if let Err(e) = ensure_output_path_safe(s, dir, &model_dir) {
            return JobResult::failed(format!("небезопасный путь результата: {e:#}"));
        }
    }

    let soundtouch = load_soundtouch(emit);
    let par = s.parallel_config();
    if par.processes > 1 {
        emit(JobEvent::Log(format!(
            "ВНИМАНИЕ: SRT-пакет использует однопроцессный пул (запрошено процессов: {})",
            par.processes
        )));
    }
    let mut pool_settings = s.clone();
    pool_settings.threads = par.ort_threads;
    let pool_opts = pipeline_options(&pool_settings, model_dir, char_rules);
    let pockets = 1.max(par.workers_per_process.min(par.max_concurrent_chunks) as usize);
    emit(JobEvent::Log(format!(
        "постоянный чанк-пул: загрузка {pockets} Pocket…"
    )));
    let spawned = spawn_workers(pockets, real_worker_factory(&pool_opts, 42), 42);
    if !spawned.failures.is_empty() {
        let msg = spawned
            .failures
            .iter()
            .map(|(w, e)| format!("воркер {w}: {e}"))
            .collect::<Vec<_>>()
            .join("; ");
        return JobResult::failed(format!("пул не запущен: {msg}"));
    }
    emit(JobEvent::BatchWorkersReady { workers: pockets });
    let slots: Arc<Vec<WorkerSlot<app_core::worker::RealWorker>>> = Arc::new(spawned.slots);

    let mut reports: Vec<FileReport> = Vec::new();
    let mut cancelled = false;
    for (idx, (input, rel)) in files.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            cancelled = true;
            reports.push(FileReport {
                input: input.clone(),
                relative: rel.clone(),
                output: None,
                status: FileStatus::Skipped,
                reason: Some("отменено до запуска".into()),
                final_path: None,
                partial_path: None,
                diag_path: None,
                chunk_log_path: None,
                chunks_done: 0,
                chunks_total: 0,
            });
            continue;
        }
        let out_path = out_root.join(rel.with_extension(s.format.extension_str()));
        emit(JobEvent::FileStarted {
            index: idx,
            total,
            input: input.clone(),
            output: out_path.clone(),
        });
        let bytes = match std::fs::read(input) {
            Ok(b) => b,
            Err(e) => {
                let r = JobResult::failed(format!("прочитать {}: {e}", input.display()));
                reports.push(report_from_result(input, rel, None, &r, 0, 0));
                emit_file_done(emit, idx, total, input, rel, &r, 0, 0);
                continue;
            }
        };
        let source_name = input
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("srt")
            .to_string();
        let existed = out_path.exists();
        let res = run_srt_file_on_pool(
            s,
            &slots,
            &pool_opts,
            &bytes,
            &source_name,
            &out_path,
            soundtouch.as_ref(),
            existed,
            cancel,
            &|done, chunks_total| {
                emit(JobEvent::FileProgress {
                    index: idx,
                    total,
                    done,
                    chunks_total,
                })
            },
            &{
                let emit_c = Arc::clone(emit);
                let idx_c = idx;
                let adapter: SharedEmit = Arc::new(move |ev: JobEvent| {
                    if let JobEvent::Phase(ph) = ev {
                        emit_c(JobEvent::FilePhase {
                            index: idx_c,
                            phase: ph,
                        });
                    }
                });
                adapter
            },
        );
        if res.status == JobStatus::Cancelled {
            cancelled = true;
        }
        emit_file_done(
            emit,
            idx,
            total,
            input,
            rel,
            &res,
            res.progress.0,
            res.progress.1,
        );
        reports.push(report_from_result(
            input,
            rel,
            Some(&out_path),
            &res,
            res.progress.0,
            res.progress.1,
        ));
    }

    let batch = BatchReport {
        files: reports,
        conflicts: Vec::new(),
        cancelled,
        pool_loads: Some(pockets),
        pool_workers: Some(pockets),
        pool_processes: None,
    };
    let completed = batch.completed();
    let status = if cancelled && completed < total {
        JobStatus::Cancelled
    } else if completed == total {
        JobStatus::Completed
    } else {
        JobStatus::Failed
    };
    let summary = format!("SRT-пакет: готово {completed} из {total} (пул {pockets} Pocket)");
    JobResult {
        status,
        summary,
        final_path: None,
        partial_path: None,
        diag_path: None,
        chunk_log_path: None,
        rtf: None,
        progress: (completed, total),
        replaced_existing: false,
        batch: Some(batch),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_file_done(
    emit: &SharedEmit,
    idx: usize,
    total: usize,
    input: &Path,
    rel: &Path,
    res: &JobResult,
    done: usize,
    chunks_total: usize,
) {
    let report = report_from_result(input, rel, None, res, done, chunks_total);
    emit(JobEvent::Log(format!(
        "[{}/{}] {} — {}",
        idx + 1,
        total,
        rel.display(),
        res.summary
    )));
    emit(JobEvent::FileDone {
        index: idx,
        total,
        report: Box::new(report),
    });
}

#[allow(clippy::too_many_arguments)]
fn report_from_result(
    input: &Path,
    rel: &Path,
    output: Option<&Path>,
    res: &JobResult,
    done: usize,
    chunks_total: usize,
) -> FileReport {
    let status = match res.status {
        JobStatus::Completed => FileStatus::Completed,
        JobStatus::Skipped => FileStatus::Skipped,
        JobStatus::Cancelled => FileStatus::Cancelled,
        _ => FileStatus::Failed,
    };
    FileReport {
        input: input.to_path_buf(),
        relative: rel.to_path_buf(),
        output: output.map(Path::to_path_buf),
        status,
        reason: if status == FileStatus::Failed {
            Some(res.summary.clone())
        } else {
            None
        },
        final_path: res.final_path.clone(),
        partial_path: res.partial_path.clone(),
        diag_path: res.diag_path.clone(),
        chunk_log_path: res.chunk_log_path.clone(),
        chunks_done: done,
        chunks_total,
    }
}

/// SRT.9B: признак имени файла .srt (без учёта регистра) для
/// `batch::plan_dir_tree`.
fn is_srt_name(name: &std::ffi::OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| x.eq_ignore_ascii_case("srt"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DeviceChoice;

    #[test]
    fn srt_options_from_settings_defaults() {
        let s = GuiSettings::default();
        let o = srt_options(&s);
        assert_eq!(o.timing.max_tempo, 2.0);
        assert_eq!(o.timing.min_pause_ms, 100);
        assert_eq!(o.internal_pause_ms, 500);
        assert_eq!(o.comma_pause_ms, 100);
        assert_eq!(o.max_cue_pcm_samples, 15 * 60 * 24_000);
    }

    #[test]
    fn srt_settings_roundtrip_and_clamp() {
        let s = GuiSettings {
            srt_max_tempo: 2.5,
            srt_min_pause_ms: 200,
            srt_internal_pause_ms: 300,
            srt_comma_pause_ms: 50,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("srt_max_tempo"));
        let back: GuiSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.srt_max_tempo, 2.5);
        assert_eq!(back.srt_min_pause_ms, 200);
        // отсутствующие ключи — дефолты (старый обычный конфиг)
        let base = r#"{"text_file": "x.txt"}"#;
        let old: GuiSettings = serde_json::from_str(base).unwrap();
        assert_eq!(old.srt_max_tempo, 2.0);
        assert_eq!(old.srt_comma_pause_ms, 100);
        // клампы
        let mut bad = GuiSettings {
            srt_max_tempo: 99.0,
            srt_min_pause_ms: 9999,
            ..Default::default()
        };
        bad.clamped();
        assert_eq!(bad.srt_max_tempo, 3.0);
        assert_eq!(bad.srt_min_pause_ms, 500);
    }

    #[test]
    fn collect_srt_files_recursive_sorted() {
        // SRT.9B: обычный рекурсивный обход через безопасный plan_dir_tree
        // (.srt/.SRT, детерминированный порядок как в TXT-пакете).
        let dir = std::env::temp_dir().join(format!("srt6_collect_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("z.srt"), b"1").unwrap();
        std::fs::write(dir.join("b").join("m.srt"), b"2").unwrap();
        std::fs::write(dir.join("a").join("n.SRT"), b"3").unwrap();
        std::fs::write(dir.join("ignore.txt"), b"x").unwrap();
        let out = std::env::temp_dir().join(format!("srt6_out_{}", std::process::id()));
        let (files, warnings) =
            crate::batch::plan_dir_tree(&dir, &out, is_srt_name).expect("обход без ссылок");
        assert!(warnings.is_empty(), "{warnings:?}");
        let rels: Vec<String> = files
            .iter()
            .map(|f| f.relative.display().to_string())
            .collect();
        assert_eq!(rels.len(), 3, "{rels:?}");
        // порядок: относительный путь без учёта регистра (a, b, z)
        assert_eq!(
            rels,
            vec![
                format!("a{}n.SRT", std::path::MAIN_SEPARATOR),
                format!("b{}m.srt", std::path::MAIN_SEPARATOR),
                "z.srt".to_string(),
            ]
        );
        // детерминизм: повторный обход даёт тот же список
        let (files2, _) = crate::batch::plan_dir_tree(&dir, &out, is_srt_name).unwrap();
        assert_eq!(files, files2);
    }

    // ===== SRT.9B: безопасный обход (ссылки/junction/выходной каталог) =====

    fn srt9b_tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let d =
            std::env::temp_dir().join(format!("srt9b_{}_{}_{}", std::process::id(), tag, nanos));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

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

    fn rels_of(files: &[crate::batch::BatchFile]) -> Vec<String> {
        files
            .iter()
            .map(|f| f.relative.display().to_string())
            .collect()
    }

    #[test]
    fn srt9b_output_dir_inside_source_is_excluded() {
        // Выходная папка внутри исходной: её .srt НЕ включаются в пакет
        // (результаты прошлых запусков не озвучиваются повторно).
        let d = srt9b_tmpdir("nested_out");
        let src = d.join("src");
        std::fs::create_dir_all(src.join("output")).unwrap();
        std::fs::write(src.join("chapter1.srt"), b"1").unwrap();
        std::fs::write(src.join("chapter2.srt"), b"2").unwrap();
        std::fs::write(src.join("output").join("old1.srt"), b"3").unwrap();
        std::fs::write(src.join("output").join("old2.srt"), b"4").unwrap();
        let (files, _) =
            crate::batch::plan_dir_tree(&src, &src.join("output"), is_srt_name).unwrap();
        assert_eq!(
            rels_of(&files),
            vec!["chapter1.srt".to_string(), "chapter2.srt".to_string()],
            "файлы из output исключены"
        );
    }

    #[test]
    fn srt9b_output_dir_absent_at_start_still_excluded() {
        // Выходного каталога ещё нет — исключение всё равно работает
        // (сравнение по нормализованному пути, не по наличию).
        let d = srt9b_tmpdir("absent_out");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.srt"), b"1").unwrap();
        let out = src.join("result");
        let (files, _) = crate::batch::plan_dir_tree(&src, &out, is_srt_name).unwrap();
        assert_eq!(rels_of(&files), vec!["a.srt".to_string()]);
    }

    #[test]
    fn srt9b_output_equals_source_is_error_case_insensitive() {
        let d = srt9b_tmpdir("same_out");
        let src = d.join("Src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.srt"), b"1").unwrap();
        // точное совпадение
        let err = crate::batch::plan_dir_tree(&src, &src, is_srt_name).unwrap_err();
        assert!(
            format!("{err:#}").contains("физически совпадает"),
            "{err:#}"
        );
        // совпадение в другом регистре (Windows нечувствительна к регистру)
        let lower = d.join("src");
        let err = crate::batch::plan_dir_tree(&src, &lower, is_srt_name).unwrap_err();
        assert!(
            format!("{err:#}").contains("физически совпадает"),
            "регистронезависимое сравнение: {err:#}"
        );
        // пути с . и .. ведут к тому же каталогу
        let dotted = src.join("..").join("Src");
        let err = crate::batch::plan_dir_tree(&src, &dotted, is_srt_name).unwrap_err();
        assert!(
            format!("{err:#}").contains("физически совпадает"),
            "нормализация . и ..: {err:#}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn srt9b_junctions_are_not_followed() {
        let d = srt9b_tmpdir("junction");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("own.srt"), b"1").unwrap();
        // junction на внешний каталог с .srt
        let external = d.join("external");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("outside.srt"), b"2").unwrap();
        let link_out = src.join("to_external");
        if !make_junction(&external, &link_out) {
            eprintln!("SKIP srt9b_junctions_are_not_followed: нет прав на junction");
            return;
        }
        // junction на родительский каталог (цикл, если следовать)
        let link_parent = src.join("to_parent");
        if !make_junction(&d, &link_parent) {
            eprintln!("SKIP srt9b_junctions_are_not_followed: нет прав на junction (родитель)");
            return;
        }
        let out = d.join("out");
        let (files, warnings) = crate::batch::plan_dir_tree(&src, &out, is_srt_name).unwrap();
        assert_eq!(
            rels_of(&files),
            vec!["own.srt".to_string()],
            "только физический файл"
        );
        assert_eq!(
            warnings.len(),
            2,
            "обе junction пропущены с предупреждением: {warnings:?}"
        );
        assert!(warnings
            .iter()
            .all(|w| w.contains("to_external") || w.contains("to_parent")));
    }

    #[cfg(windows)]
    #[test]
    fn srt9b_two_links_to_same_dir_skipped_once_each() {
        let d = srt9b_tmpdir("two_links");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let target = d.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("t.srt"), b"1").unwrap();
        let l1 = src.join("alias1");
        let l2 = src.join("alias2");
        if !make_junction(&target, &l1) || !make_junction(&target, &l2) {
            eprintln!("SKIP srt9b_two_links_to_same_dir_skipped_once_each: нет прав на junction");
            return;
        }
        let out = d.join("out");
        let (files, warnings) = crate::batch::plan_dir_tree(&src, &out, is_srt_name).unwrap();
        assert!(
            files.is_empty(),
            "через ссылки файлы не собираются: {:?}",
            rels_of(&files)
        );
        assert_eq!(
            warnings.len(),
            2,
            "каждая ссылка пропущена отдельно: {warnings:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn srt9b_dir_symlinks_are_not_followed() {
        use std::os::windows::fs::symlink_dir;
        let d = srt9b_tmpdir("symlink");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("own.srt"), b"1").unwrap();
        let sibling = d.join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("s.srt"), b"2").unwrap();
        let link_sibling = src.join("to_sibling");
        if symlink_dir(&sibling, &link_sibling).is_err() {
            eprintln!(
                "SKIP srt9b_dir_symlinks_are_not_followed: нет прав на symlink \
                 (нужен Developer Mode/администратор); junction-путь покрыт отдельным тестом"
            );
            return;
        }
        // symlink на родителя — циклическая ссылка
        let link_parent = src.join("to_parent");
        if symlink_dir(&d, &link_parent).is_err() {
            eprintln!("SKIP srt9b_dir_symlinks_are_not_followed: нет прав на symlink (родитель)");
            return;
        }
        let out = d.join("out");
        let (files, warnings) = crate::batch::plan_dir_tree(&src, &out, is_srt_name).unwrap();
        assert_eq!(rels_of(&files), vec!["own.srt".to_string()]);
        assert_eq!(warnings.len(), 2, "оба symlink пропущены: {warnings:?}");
    }

    #[cfg(windows)]
    #[test]
    fn srt9b_file_symlink_srt_is_skipped() {
        use std::os::windows::fs::symlink_file;
        let d = srt9b_tmpdir("fsym");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("real.srt"), b"1").unwrap();
        let link = src.join("linked.srt");
        if symlink_file(src.join("real.srt"), &link).is_err() {
            eprintln!("SKIP srt9b_file_symlink_srt_is_skipped: нет прав на symlink");
            return;
        }
        let out = d.join("out");
        let (files, warnings) = crate::batch::plan_dir_tree(&src, &out, is_srt_name).unwrap();
        assert_eq!(
            rels_of(&files),
            vec!["real.srt".to_string()],
            "файл-ссылка не собрана"
        );
        assert_eq!(
            warnings.len(),
            1,
            "ссылка пропущена с предупреждением: {warnings:?}"
        );
    }

    #[test]
    fn srt9b_root_errors_are_explicit() {
        let cancel = Arc::new(AtomicBool::new(false));
        let emit: SharedEmit = Arc::new(|_| {});
        // несуществующий корень — честная ошибка доступа, не «нет файлов»
        let missing = std::env::temp_dir().join(format!(
            "srt9b_missing_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let r = execute_srt_batch(&GuiSettings::default(), &missing, &cancel, &emit);
        assert_eq!(r.status, JobStatus::Failed);
        assert!(
            r.summary.contains("не читается"),
            "несуществующий корень: {}",
            r.summary
        );
        assert!(
            !r.summary.contains("нет файлов"),
            "ошибка доступа не маскируется: {}",
            r.summary
        );
        // источник — обычный файл, не каталог
        let d = srt9b_tmpdir("filesrc");
        let f = d.join("not_a_dir.srt");
        std::fs::write(&f, b"1").unwrap();
        let r = execute_srt_batch(&GuiSettings::default(), &f, &cancel, &emit);
        assert_eq!(r.status, JobStatus::Failed);
        assert!(
            r.summary.contains("не читается") || r.summary.contains("не каталог"),
            "файл вместо каталога: {}",
            r.summary
        );
    }

    #[test]
    fn empty_dir_is_failure() {
        let dir = std::env::temp_dir().join(format!("srt6_empty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let emit: SharedEmit = Arc::new(|_| {});
        let r = execute_srt_batch(&GuiSettings::default(), &dir, &cancel, &emit);
        assert_eq!(r.status, JobStatus::Failed);
        assert!(r.summary.contains("нет файлов .srt"));
    }

    // ---- SRT.7: реальные прогоны (модель + голос + SoundTouchDLL) ----
    // Запуск: cargo test -p pockettts-gui-rust --release --lib
    //   srt_job::tests::real_ -- --ignored --nocapture --test-threads=1
    // Устройство: SHTORM_GUI_TEST_DEVICE=cuda (по умолчанию CPU).

    const REAL_ONNX_BASE: &str = r"C:\Users\<user>\Downloads\pocket_tts_android\build\onnx";
    const REAL_VOICE: &str = r"C:\PocketTTS-GUI\examples\Microsoft Brian Speed 1.wav";
    const REAL_FFMPEG: &str = r"C:\PocketTTS-GUI\ffmpeg\ffmpeg.exe";

    /// SRT-корпус: короткая реплика / пустая разметочная / длинная
    /// (несколько чанков) / тесное окно (потребуется ускорение).
    const REAL_SRT: &str = "1\n00:00:00,000 --> 00:00:04,000\nСтрелки часов показывали полночь, а стрелки застыли.\n\n2\n00:00:04,500 --> 00:00:05,000\n<i></i>\n\n3\n00:00:05,200 --> 00:00:11,000\nВторое предложение для надёжности прогона конвейера субтитров: несколько чанков, паузы и выравнивание.\n\n4\n00:00:11,200 --> 00:00:12,000\nТесное окно.\n";

    fn real_srt_settings(fmt: crate::settings::OutputFormatChoice, out_dir: &Path) -> GuiSettings {
        let mut s = GuiSettings {
            model: crate::settings::ModelChoice::Slow,
            // путь моделей в репозитории анонимизирован (<user>); локальный
            // прогон задаётся SRT_TEST_ONNX_BASE
            onnx_base: std::env::var("SRT_TEST_ONNX_BASE")
                .unwrap_or_else(|_| REAL_ONNX_BASE.into()),
            voice_wav: REAL_VOICE.into(),
            device: if std::env::var("SHTORM_GUI_TEST_DEVICE").as_deref() == Ok("cuda") {
                DeviceChoice::Cuda
            } else {
                DeviceChoice::Cpu
            },
            device_id: 0,
            threads: 4,
            seed: 42,
            max_tokens: 40,
            format: fmt,
            out_dir: out_dir.display().to_string(),
            chunk_log: true,
            overwrite_existing: false,
            ..Default::default()
        };
        if fmt.needs_ffmpeg() {
            s.ffmpeg_path = REAL_FFMPEG.into();
            s.mp3_preset = "-V2 (Высокое)".into();
            s.opus_preset = "opus 64k".into();
        }
        s.clamped();
        s
    }

    fn srt_tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("srt7_{}_{}_{}", std::process::id(), tag, nanos));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    #[ignore]
    fn real_srt_wav_and_determinism() {
        let d = srt_tmpdir("wav");
        let s = real_srt_settings(crate::settings::OutputFormatChoice::Wav, &d);
        let run = |out: &Path| {
            let cfg = SrtJobConfig {
                settings: s.clone(),
                srt_bytes: REAL_SRT.as_bytes().to_vec(),
                source_name: "real.srt".into(),
                out_path: out.to_path_buf(),
            };
            let cancel = Arc::new(AtomicBool::new(false));
            let emit: SharedEmit = Arc::new(|_| {});
            execute_srt_job(&cfg, &cancel, &emit)
        };
        let out1 = d.join("a.wav");
        let r1 = run(&out1);
        assert_eq!(r1.status, JobStatus::Completed, "{}", r1.summary);
        assert!(r1.final_path.as_ref().unwrap().is_file());
        // лог таймингов написан рядом и содержит ускорение тесного окна
        let log = d.join("a_log.txt");
        assert!(log.is_file(), "лог таймингов: {}", log.display());
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("Озвучено реплик: 3 из 4"), "{text}");
        assert!(
            text.contains("ускорение"),
            "тесное окно должно ускоряться: {text}"
        );
        // повтор на ДРУГОЙ путь — побитовая идентичность (seed 42)
        let out2 = d.join("b.wav");
        let r2 = run(&out2);
        assert_eq!(r2.status, JobStatus::Completed, "{}", r2.summary);
        let h1 = std::fs::read(&out1).unwrap();
        let h2 = std::fs::read(&out2).unwrap();
        assert_eq!(h1, h2, "повторный прогон побитово идентичен");
    }

    #[test]
    #[ignore]
    fn real_srt_encoder_format() {
        let is_cuda = std::env::var("SHTORM_GUI_TEST_DEVICE").as_deref() == Ok("cuda");
        let fmt = if is_cuda {
            crate::settings::OutputFormatChoice::Opus
        } else {
            crate::settings::OutputFormatChoice::Mp3
        };
        let d = srt_tmpdir("enc");
        let mut s = real_srt_settings(fmt, &d);
        s.srt_max_tempo = 1.5; // отличимый от дефолта потолок
        let out = d.join(format!("enc.{}", s.format.extension_str()));
        let cfg = SrtJobConfig {
            settings: s,
            srt_bytes: REAL_SRT.as_bytes().to_vec(),
            source_name: "real.srt".into(),
            out_path: out.clone(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let emit: SharedEmit = Arc::new(|_| {});
        let r = execute_srt_job(&cfg, &cancel, &emit);
        assert_eq!(r.status, JobStatus::Completed, "{}", r.summary);
        let f = r.final_path.unwrap();
        assert!(f.is_file());
        assert!(std::fs::metadata(&f).unwrap().len() > 1000);
    }

    #[test]
    #[ignore]
    fn real_srt_batch_directory() {
        let d = srt_tmpdir("batch");
        let src = d.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("one.srt"), REAL_SRT).unwrap();
        std::fs::write(src.join("sub").join("two.srt"), REAL_SRT).unwrap();
        std::fs::write(src.join("note.txt"), "не srt").unwrap();
        let out_root = d.join("out");
        let mut s = real_srt_settings(crate::settings::OutputFormatChoice::Wav, &out_root);
        s.batch = true;
        s.text_file = src.display().to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        let emit: SharedEmit = Arc::new(|_| {});
        let r = execute_srt_batch(&s, &src, &cancel, &emit);
        if let Some(b) = &r.batch {
            for line in b.detail_lines() {
                eprintln!("FILE: {line}");
            }
        }
        assert_eq!(r.status, JobStatus::Completed, "{}", r.summary);
        let batch = r.batch.expect("batch report");
        assert_eq!(batch.completed(), 2, "{}", batch.summary());
        assert!(out_root.join("one.wav").is_file());
        assert!(out_root.join("sub").join("two.wav").is_file());
        assert!(!out_root.join("note.wav").exists(), "txt не озвучивается");
    }
}
