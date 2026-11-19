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
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new(); // (input, relative)
    collect_srt_files(source_dir, source_dir, &mut files);
    files.sort();
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
    let out_root = resources::resolve_user_path(&s.out_dir);
    for (_, rel) in &files {
        // безопасность каждого каталога результата — до создания (G5.1);
        // сами файлы проверяются в run_srt_file_on_pool через sink.
        let dir = out_root.join(rel.parent().unwrap_or(Path::new("")));
        if let Err(e) = ensure_output_path_safe(s, &dir, &model_dir) {
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

/// Рекурсивный сбор .srt с относительными путями (сортировка вызывающим).
fn collect_srt_files(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for e in items {
        let path = e.path();
        if path.is_dir() {
            collect_srt_files(root, &path, out);
        } else if path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("srt"))
        {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            out.push((path.clone(), rel.to_path_buf()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = std::env::temp_dir().join(format!("srt6_collect_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("z.srt"), b"1").unwrap();
        std::fs::write(dir.join("b").join("m.srt"), b"2").unwrap();
        std::fs::write(dir.join("a").join("n.SRT"), b"3").unwrap();
        std::fs::write(dir.join("ignore.txt"), b"x").unwrap();
        let mut files = Vec::new();
        collect_srt_files(&dir, &dir, &mut files);
        let rels: Vec<String> = files.iter().map(|(_, r)| r.display().to_string()).collect();
        assert_eq!(rels.len(), 3, "{rels:?}");
        assert!(rels.contains(&"z.srt".to_string()));
        assert!(rels.contains(&format!("a{}n.SRT", std::path::MAIN_SEPARATOR)));
        assert!(rels.contains(&format!("b{}m.srt", std::path::MAIN_SEPARATOR)));
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
}
