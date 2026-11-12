//! G6.7-C: мультипроцессный ЧАНК-конвейер.
//!
//! Диспетчер (GUI-процесс, без ONNX) владеет файлами СТРОГО ПОСЛЕДОВАТЕЛЬНО:
//! следующий TXT начинается только после терминального завершения
//! предыдущего (включая публикацию). Чанки ТЕКУЩЕГО файла распределяются
//! между постоянными Pocket дочерних процессов (P×W, создаются один раз на
//! задание и переиспользуются между файлами); PCM возвращается бинарными
//! кадрами Named Pipe (u32 LE длина + f32 LE сэмплы), упорядоченная сборка
//! аудио — у диспетчера (тот же sink/finish/publish, что в одиночном
//! режиме, семантика G5.1 сохранена).
//!
//! Ограничение C: одновременно в работе не больше `max_chunks` чанков
//! (окно предварительного планирования — ещё до +2 чанка, документировано).
//! Backpressure: буфер готовых-не-по-порядку чанков ограничен окном
//! `max_chunks + 2` — быстрые воркеры не синтезируют десятки будущих чанков
//! из-за одного медленного раннего.
//!
//! Seed: per-file seed назначает диспетчер; `say_chunk` пересеивает RNG как
//! `seed + chunk_index` — аудио чанка не зависит от того, каким Pocket или
//! процессом он выполнен (побитовый паритет, P2B.2 §11).

use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use app_core::pipeline::DenoiseReport;
use app_core::pipeline::{build_file_tasks, PipelineOptions, RunStatus, StreamOutcome};
use app_core::sink::make_sink_with_overwrite;
use shtorm_engine::POCKET_RATE;

use crate::batch::{FileReport, FileStatus};
use crate::hybrid::{denoise_from_tag, send_cmd, ChildEvent, Cmd, Ev, HybridJob, SpawnedChild};
use crate::job::{output_format, JobConfig, JobEvent};
use crate::settings::GuiSettings;

/// Окно предварительного планирования поверх C: фидер может выдать чанки с
/// индексами до `next_emit + max_chunks + PREPLAN`. Ограничено — обходить
/// лимит C нельзя (в работе одновременно всё равно ≤ C).
const PREPLAN: u64 = 2;
/// Ожидание событий в основном цикле (реакция на отмену/таймауты).
const POLL: Duration = Duration::from_millis(200);

/// Параметры мультипроцессного чанк-прогона.
pub struct ChunkPipeParams {
    pub settings: GuiSettings,
    /// Опции конвейера (модель/голоса/EOS/…; threads = T конфигурации).
    pub opts: PipelineOptions,
    /// Файлы в порядке выполнения (строго последовательно).
    pub jobs: Vec<HybridJob>,
    pub workers_per_child: usize,
    pub children: usize,
    /// Предел C — одновременно синтезируемых чанков текущего файла.
    pub max_chunks: usize,
    pub fake: bool,
}

/// Итог чанк-прогона: отчёты по позициям jobs + диагностика пула.
#[derive(Debug)]
pub struct ChunkPipeOutcome {
    pub reports: Vec<Option<FileReport>>,
    pub pocket_loads: usize,
    pub workers_ready: usize,
    pub children_spawned: usize,
}

/// Драйвер чанк-конвейера (выполняется в потоке задания GUI).
pub struct ChunkPipeDriver<'a> {
    pub params: &'a ChunkPipeParams,
    pub cancel: Arc<AtomicBool>,
    /// Запуск дочернего процесса (продакшн: spawn_child_process; тесты —
    /// тестовый харнесс).
    pub spawn: &'a dyn Fn(usize) -> Result<SpawnedChild>,
    pub emit: &'a Arc<dyn Fn(JobEvent) + Send + Sync>,
    /// Вызывается ОДИН раз, когда инициализация процессов устоялась.
    pub init_settled: &'a dyn Fn(usize),
    pub watchdog: Duration,
    pub startup_timeout: Duration,
    pub write_timeout: Duration,
}

struct WState {
    ci: usize,
    wid: usize,
    ready: bool,
    alive: bool,
    /// Индекс чанка текущего файла в работе у этого воркера.
    busy: Option<u64>,
}

struct ChildProc {
    child: std::process::Child,
    writer: Arc<crate::ipc::PipeEnd>,
    _fwd: std::thread::JoinHandle<()>,
}

impl ChildProc {
    fn send(&self, cmd: &Cmd) -> Result<()> {
        send_cmd(&self.writer, cmd)
    }
}

/// Запуск мультипроцессного чанк-конвейера.
pub fn run_chunk_pipeline(d: &ChunkPipeDriver) -> Result<ChunkPipeOutcome> {
    let children = d.params.children;
    let wpc = d.params.workers_per_child;
    anyhow::ensure!(children >= 1, "чанк-конвейер требует минимум 1 процесс");
    anyhow::ensure!(!d.params.jobs.is_empty(), "чанк-конвейер без файлов");
    let total_jobs = d.params.jobs.len();

    // --- запуск процессов: спавнер (с Job Object) передаёт драйвер;
    // FAIL-CLOSED-проверка Job Object — на стороне вызывающего, как G6.2.1 ---

    let mut procs: Vec<Option<ChildProc>> = Vec::with_capacity(children);
    let (tx, rx) = mpsc::channel::<(usize, ChildEvent)>();
    let mut spawned_count = 0usize;
    let mut pocket_loads = 0usize;
    let mut workers_ready = 0usize;
    let mut workers: Vec<WState> = Vec::with_capacity(children * wpc);
    #[allow(clippy::needless_range_loop)]
    for ci in 0..children {
        for wid in 0..wpc {
            workers.push(WState {
                ci,
                wid,
                ready: false,
                alive: true,
                busy: None,
            });
        }
    }
    let mut child_alive = vec![false; children];
    let mut last_seen = vec![Instant::now(); children];
    let startup_deadline = vec![Instant::now() + d.startup_timeout; children];
    let mut cancel_sent = false;

    let mut init_failure: Option<String> = None;
    // индекс ci адресует несколько параллельных векторов состояния
    #[allow(clippy::needless_range_loop)]
    for ci in 0..children {
        match (d.spawn)(ci) {
            Ok(mut sp) => {
                sp.writer.set_write_timeout(d.write_timeout);
                let init = Cmd::Init {
                    settings: d.params.settings.clone(),
                    workers: wpc,
                    fake: d.params.fake,
                    test: Default::default(),
                };
                if let Err(e) = send_cmd(&sp.writer, &init) {
                    let _ = sp.child.kill();
                    let _ = sp.child.wait();
                    init_failure = Some(format!("процесс {ci}: init не доставлен: {e:#}"));
                    break;
                }
                let child = sp.child;
                let writer = Arc::clone(&sp.writer);
                let events = sp.events;
                let tx = tx.clone();
                let fwd = std::thread::spawn(move || {
                    for ev in events {
                        if tx.send((ci, ev)).is_err() {
                            break;
                        }
                    }
                });
                procs.push(Some(ChildProc {
                    child,
                    writer,
                    _fwd: fwd,
                }));
                child_alive[ci] = true;
                spawned_count += 1;
            }
            Err(e) => {
                init_failure = Some(format!("процесс {ci}: {e:#}"));
                procs.push(None);
                break;
            }
        }
    }
    // добиваем незапущенные слоты, чтобы индексация совпадала
    while procs.len() < children {
        procs.push(None);
    }
    if let Some(reason) = init_failure {
        shutdown_all(&mut procs, &mut child_alive, d);
        bail!("чанк-конвейер не запущен: {reason}");
    }
    drop(tx);

    // --- ожидание готовности пула (устойчивая инициализация G6.2.1) ---
    let start_wait = Instant::now();
    loop {
        let can_init = (0..children).any(|ci| {
            child_alive[ci]
                && (0..children * wpc)
                    .any(|wi| workers[wi].ci == ci && workers[wi].alive && !workers[wi].ready)
        });
        let any_ready = workers.iter().any(|w| w.alive && w.ready);
        if !can_init || (any_ready && start_wait.elapsed() > Duration::from_secs(2)) {
            break;
        }
        match rx.recv_timeout(POLL) {
            Ok((ci, ev)) => {
                handle_pool_event(
                    &ev,
                    ci,
                    &mut workers,
                    &mut child_alive,
                    &mut last_seen,
                    &mut pocket_loads,
                    &mut workers_ready,
                    d,
                );
                if matches!(ev, ChildEvent::Closed(_)) {
                    mark_child_dead(ci, &mut workers, &mut child_alive);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // startup timeout: процесс без готовых воркеров слишком молчалив
        for ci in 0..children {
            if child_alive[ci] && Instant::now() > startup_deadline[ci] {
                let has_ready = workers.iter().any(|w| w.ci == ci && w.ready);
                if !has_ready {
                    (d.emit)(JobEvent::Log(format!(
                        "ВНИМАНИЕ: процесс {ci}: startup timeout — процесс завершается"
                    )));
                    if let Some(Some(p)) = procs.get_mut(ci) {
                        let _ = p.child.kill();
                        let _ = p.child.wait();
                    }
                    child_alive[ci] = false;
                    mark_child_dead(ci, &mut workers, &mut child_alive);
                }
            }
        }
    }
    (d.init_settled)(workers_ready);
    let mut reports: Vec<Option<FileReport>> = (0..total_jobs).map(|_| None).collect();

    if workers.iter().filter(|w| w.alive && w.ready).count() == 0 {
        (d.emit)(JobEvent::Log(
            "ВНИМАНИЕ: ни один воркер мультипроцессного пула не готов — файлы завершатся \
             ошибкой"
                .into(),
        ));
        for (ji, job) in d.params.jobs.iter().enumerate() {
            reports[ji] = Some(FileReport::failed(
                &crate::batch::BatchFile {
                    input: job.input.clone(),
                    relative: job.relative.clone(),
                },
                Some(job.out.clone()),
                "мультипроцессный пул не запущен: ни один воркер не готов",
            ));
        }
        shutdown_all(&mut procs, &mut child_alive, d);
        return Ok(ChunkPipeOutcome {
            reports,
            pocket_loads,
            workers_ready,
            children_spawned: spawned_count,
        });
    }

    // --- строго последовательная обработка файлов ---
    let mut stopped = false;
    for (ji, job) in d.params.jobs.iter().enumerate() {
        if stopped || d.cancel.load(Ordering::SeqCst) {
            stopped = true;
            let r = FileReport::skipped(
                &crate::batch::BatchFile {
                    input: job.input.clone(),
                    relative: job.relative.clone(),
                },
                Some(job.out.clone()),
                "отменено до запуска",
            );
            (d.emit)(JobEvent::FileDone {
                index: job.pos + 1,
                total: total_jobs,
                report: Box::new(r.clone()),
            });
            reports[ji] = Some(r);
            continue;
        }
        let live_workers = workers.iter().filter(|w| w.alive && w.ready).count();
        if live_workers == 0 {
            let r = FileReport::failed(
                &crate::batch::BatchFile {
                    input: job.input.clone(),
                    relative: job.relative.clone(),
                },
                Some(job.out.clone()),
                "пул недостоверен: все воркеры дочерних процессов погибли — файл не запускался",
            );
            (d.emit)(JobEvent::FileDone {
                index: job.pos + 1,
                total: total_jobs,
                report: Box::new(r.clone()),
            });
            reports[ji] = Some(r);
            stopped = true;
            continue;
        }
        let report = run_one_file(
            d,
            &rx,
            &mut procs,
            &mut workers,
            &mut child_alive,
            &mut last_seen,
            ji,
            job,
            &mut pocket_loads,
            &mut workers_ready,
            &mut cancel_sent,
        );
        let was_cancel = report.status == FileStatus::Cancelled;
        (d.emit)(JobEvent::Log(format!(
            "[{}/{}] {}",
            job.pos + 1,
            total_jobs,
            report.line()
        )));
        (d.emit)(JobEvent::FileDone {
            index: job.pos + 1,
            total: total_jobs,
            report: Box::new(report.clone()),
        });
        reports[ji] = Some(report);
        if was_cancel || d.cancel.load(Ordering::SeqCst) {
            stopped = true;
        }
    }

    // --- контролируемое завершение своих дочерних процессов ---
    shutdown_all(&mut procs, &mut child_alive, d);
    Ok(ChunkPipeOutcome {
        reports,
        pocket_loads,
        workers_ready,
        children_spawned: spawned_count,
    })
}

/// Обработка события фазы инициализации пула (до файлов).
#[allow(clippy::too_many_arguments)]
fn handle_pool_event(
    ev: &ChildEvent,
    ci: usize,
    workers: &mut [WState],
    child_alive: &mut [bool],
    last_seen: &mut [Instant],
    pocket_loads: &mut usize,
    workers_ready: &mut usize,
    d: &ChunkPipeDriver,
) {
    last_seen[ci] = Instant::now();
    if let ChildEvent::Msg(Ev::Started { pid }) = ev {
        (d.emit)(JobEvent::Log(format!(
            "процесс {ci}: подключился (pid {pid})"
        )));
        return;
    }
    let ChildEvent::Msg(ev) = ev else {
        return;
    };
    match ev {
        Ev::PocketLoaded { worker, load_ms } => {
            *pocket_loads += 1;
            (d.emit)(JobEvent::Log(format!(
                "процесс {ci}: Pocket воркера {worker} загружен за {load_ms} мс"
            )));
        }
        Ev::Ready { worker } => {
            if let Some(w) = workers
                .iter_mut()
                .find(|w| w.ci == ci && w.wid == *worker && !w.ready)
            {
                w.ready = true;
                *workers_ready += 1;
            }
        }
        Ev::WorkerError { worker, error } => {
            (d.emit)(JobEvent::Log(format!(
                "ВНИМАНИЕ: процесс {ci}, воркер {worker}: {error}"
            )));
            for w in workers
                .iter_mut()
                .filter(|w| w.ci == ci && w.wid == *worker)
            {
                w.alive = false;
                w.ready = false;
            }
        }
        Ev::Stopped { .. } | Ev::Exiting => {
            mark_child_dead(ci, workers, child_alive);
        }
        _ => {}
    }
}

fn mark_child_dead(ci: usize, workers: &mut [WState], child_alive: &mut [bool]) {
    child_alive[ci] = false;
    for w in workers.iter_mut().filter(|w| w.ci == ci) {
        w.alive = false;
        w.ready = false;
        w.busy = None;
    }
}

/// Послать Shutdown живым детям, дождаться выхода (убить свой PID по
/// таймауту — осиротевших процессов не остаётся; KILL_ON_JOB_CLOSE —
/// страховка на случай гибели GUI).
fn shutdown_all(procs: &mut [Option<ChildProc>], child_alive: &mut [bool], d: &ChunkPipeDriver) {
    for (ci, p) in procs.iter_mut().enumerate() {
        if !child_alive[ci] {
            continue;
        }
        if let Some(p) = p {
            let _ = p.send(&Cmd::Shutdown);
        }
    }
    let deadline = Instant::now() + crate::hybrid::CHILD_EXIT_TIMEOUT;
    loop {
        let mut pending = false;
        for (ci, p) in procs.iter_mut().enumerate() {
            if !child_alive[ci] {
                continue;
            }
            if let Some(p) = p {
                match p.child.try_wait() {
                    Ok(Some(_)) => child_alive[ci] = false,
                    Ok(None) => {
                        if Instant::now() > deadline {
                            (d.emit)(JobEvent::Log(format!(
                                "ВНИМАНИЕ: дочерний процесс {ci} не завершился — завершается \
                                 принудительно"
                            )));
                            let _ = p.child.kill();
                            let _ = p.child.wait();
                            child_alive[ci] = false;
                        } else {
                            pending = true;
                        }
                    }
                    Err(_) => {
                        let _ = p.child.kill();
                        let _ = p.child.wait();
                        child_alive[ci] = false;
                    }
                }
            }
        }
        if !pending || Instant::now() > deadline + Duration::from_secs(1) {
            break;
        }
        std::thread::sleep(POLL);
    }
}

/// Готовый чанк в буфере диспетчера.
struct Buffered {
    pcm: Vec<f32>,
    pause_s: Option<f32>,
    denoise: String,
}

/// Один файл: чанки → пул → упорядоченная запись в sink → публикация.
/// Фазы G5.1: Generating при синтезе чанков, Finalizing/Publishing — у
/// диспетчера (тот же sink, что в однопроцессных путях).
#[allow(clippy::too_many_arguments)]
fn run_one_file(
    d: &ChunkPipeDriver,
    rx: &mpsc::Receiver<(usize, ChildEvent)>,
    procs: &mut [Option<ChildProc>],
    workers: &mut [WState],
    child_alive: &mut [bool],
    last_seen: &mut [Instant],
    _ji: usize,
    job: &HybridJob,
    pocket_loads: &mut usize,
    workers_ready: &mut usize,
    cancel_sent: &mut bool,
) -> FileReport {
    let total_files = d.params.jobs.len();
    let k = job.pos + 1;
    let file = crate::batch::BatchFile {
        input: job.input.clone(),
        relative: job.relative.clone(),
    };
    let fail = |msg: String| FileReport::failed(&file, Some(job.out.clone()), msg);

    // чтение текста — ошибка файла (движок не при чём)
    let text = match std::fs::read_to_string(&job.input) {
        Ok(t) => t,
        Err(e) => {
            return fail(format!("прочитать {}: {e}", job.input.display()));
        }
    };

    // повторная проверка безопасности пути — ПЕРЕД созданием sink (G6.2 §10)
    let model_dir = Path::new(&d.params.opts.model_dir).to_path_buf();
    if let Err(e) = crate::job::ensure_output_path_safe(&d.params.settings, &job.out, &model_dir) {
        return fail(format!("{e:#}"));
    }
    let s = &d.params.settings;
    let existed_before = job.out.exists();
    let ffmpeg_path = if s.ffmpeg_path.trim().is_empty() {
        None
    } else {
        Some(crate::resources::resolve_user_path(&s.ffmpeg_path))
    };
    let preset = s.preset_for_format();
    let sink = match make_sink_with_overwrite(
        &job.out,
        24_000,
        output_format(s.format),
        preset,
        ffmpeg_path.as_deref(),
        s.overwrite_existing,
    ) {
        Ok(sink) => sink,
        Err(e) => return fail(format!("{e:#}")),
    };
    let mut sink = sink;
    let partial_path = sink.partial_path();

    // план чанков — тот же код, что в однопроцессном конвейере
    let mut file_opts = d.params.opts.clone();
    if file_opts.seed.is_none() {
        file_opts.seed = Some(app_core::pool::SeedPolicy::Random.resolve());
    }
    let seed = file_opts.seed.unwrap();
    let (_texts, tasks) = match build_file_tasks(&text, &file_opts) {
        Ok(t) => t,
        Err(e) => return fail(format!("{e:#}")),
    };
    let total = tasks.len() as u64;

    (d.emit)(JobEvent::FileStarted {
        index: k,
        total: total_files,
        input: job.input.clone(),
        output: job.out.clone(),
    });
    (d.emit)(JobEvent::Log(format!(
        "[{k}/{total_files}] запуск: {} → {} | чанк-конвейер {}×{} (C={})",
        job.input.display(),
        job.out.display(),
        d.params.children,
        d.params.workers_per_child,
        d.params.max_chunks
    )));
    (d.emit)(JobEvent::FilePhase {
        index: k,
        phase: crate::job::JobPhase::Generating,
    });

    let mut next_assign: u64 = 0;
    let mut next_emit: u64 = 0;
    let mut buffer: HashMap<u64, Buffered> = HashMap::new();
    let mut file_err: Option<String> = None;
    let mut sink_err: Option<String> = None;
    let mut done_count = 0usize;
    let mut denoise = DenoiseReport::default();
    let window = d.params.max_chunks as u64 + PREPLAN;
    let mut workers_local_busy: Vec<Option<u64>> = workers.iter().map(|w| w.busy).collect();

    loop {
        let cancelled = d.cancel.load(Ordering::SeqCst);
        if cancelled && !*cancel_sent {
            *cancel_sent = true;
            for p in procs.iter().flatten() {
                let _ = p.send(&Cmd::Cancel);
            }
        }
        // выдача чанков: ≤ C в работе, окно планирования, только свободные
        // готовые воркеры живых процессов
        while file_err.is_none() && sink_err.is_none() && !cancelled && next_assign < total {
            let inflight = workers_local_busy.iter().filter(|b| b.is_some()).count();
            if inflight >= d.params.max_chunks || next_assign >= next_emit + window {
                break;
            }
            let free = workers
                .iter()
                .enumerate()
                .find(|(wi, w)| {
                    w.alive && w.ready && child_alive[w.ci] && workers_local_busy[*wi].is_none()
                })
                .map(|(wi, _)| wi);
            let Some(wi) = free else { break };
            let (w_ci, w_wid) = (workers[wi].ci, workers[wi].wid);
            let task = &tasks[next_assign as usize];
            let cmd = Cmd::AssignChunk {
                worker: w_wid,
                pos: job.pos,
                index: task.index,
                model_text: task.model_text.clone(),
                pause_s: task.pause_s,
                seed,
            };
            let send_res = procs
                .get(w_ci)
                .and_then(|p| p.as_ref())
                .map(|p| p.send(&cmd));
            match send_res {
                Some(Ok(())) => {
                    workers_local_busy[wi] = Some(task.index);
                    workers[wi].busy = Some(task.index);
                    last_seen[w_ci] = Instant::now();
                    next_assign += 1;
                }
                _ => {
                    // процесс не отвечает — завершить СВОЙ PID, воркеры мертвы
                    (d.emit)(JobEvent::Log(format!(
                        "ВНИМАНИЕ: команда чанка процессу {w_ci} не доставлена — процесс \
                         завершается"
                    )));
                    if let Some(Some(p)) = procs.get_mut(w_ci) {
                        let _ = p.child.kill();
                        let _ = p.child.wait();
                    }
                    child_alive[w_ci] = false;
                    mark_child_dead(w_ci, workers, child_alive);
                    for (i2, b) in workers_local_busy.iter_mut().enumerate() {
                        if workers[i2].ci == w_ci {
                            *b = None;
                        }
                    }
                    if file_err.is_none() {
                        file_err = Some(format!(
                            "разрыв IPC с процессом {w_ci} на чанке {next_assign}"
                        ));
                    }
                }
            }
        }

        let inflight = workers_local_busy.iter().filter(|b| b.is_some()).count();
        let file_done = next_emit == total;
        if (file_done && inflight == 0 && buffer.is_empty())
            || (file_err.is_some() && inflight == 0)
            || (sink_err.is_some() && inflight == 0)
            || (cancelled && inflight == 0)
        {
            break;
        }

        match rx.recv_timeout(POLL) {
            Ok((ci, ev)) => {
                last_seen[ci] = Instant::now();
                match ev {
                    ChildEvent::ChunkAudio {
                        worker,
                        pos,
                        index,
                        pause_s,
                        denoise: tag,
                        pcm,
                    } => {
                        if let Some(wi) = workers.iter().position(|w| w.ci == ci && w.wid == worker)
                        {
                            workers_local_busy[wi] = None;
                            workers[wi].busy = None;
                        }
                        if pos != job.pos {
                            continue; // событие прошлого файла (гонка отмены) — игнорируем
                        }
                        if file_err.is_some() || sink_err.is_some() {
                            continue; // файл уже остановлен — результат не нужен
                        }
                        buffer.insert(
                            index,
                            Buffered {
                                pcm,
                                pause_s,
                                denoise: tag.clone(),
                            },
                        );
                        // упорядоченная выдача в sink (строго по индексам)
                        while let Some(b) = buffer.remove(&next_emit) {
                            match denoise_from_tag(&b.denoise) {
                                app_core::pool::DenoiseChunkStatus::Applied => denoise.applied += 1,
                                app_core::pool::DenoiseChunkStatus::Off => denoise.off += 1,
                                app_core::pool::DenoiseChunkStatus::Missing => denoise.missing += 1,
                                app_core::pool::DenoiseChunkStatus::Failed(e) => {
                                    denoise.failed += 1;
                                    if denoise.first_error.is_none() {
                                        denoise.first_error = Some(e);
                                    }
                                }
                            }
                            let pause = if next_emit + 1 == total {
                                None // последний чанк файла — без паузы
                            } else {
                                b.pause_s
                            };
                            if let Err(e) = sink.write_chunk(next_emit as usize, &b.pcm, pause) {
                                sink_err = Some(format!("ошибка записи аудио: {e:#}"));
                                break;
                            }
                            done_count += 1;
                            (d.emit)(JobEvent::FileProgress {
                                index: k,
                                total: total_files,
                                done: done_count,
                                chunks_total: total as usize,
                            });
                            next_emit += 1;
                        }
                    }
                    ChildEvent::Msg(Ev::ChunkFailed {
                        worker,
                        index,
                        error,
                        ..
                    }) => {
                        if let Some(wi) = workers.iter().position(|w| w.ci == ci && w.wid == worker)
                        {
                            workers_local_busy[wi] = None;
                            workers[wi].busy = None;
                        }
                        if error == "отменено" {
                            continue;
                        }
                        if file_err.is_none() {
                            file_err = Some(format!("чанк {}: {error}", index + 1));
                        }
                    }
                    ChildEvent::Msg(Ev::WorkerError { worker, error }) => {
                        (d.emit)(JobEvent::Log(format!(
                            "ВНИМАНИЕ: процесс {ci}, воркер {worker}: {error}"
                        )));
                        let had_busy = workers
                            .iter()
                            .any(|w| w.ci == ci && w.wid == worker && w.busy.is_some());
                        for (wi, w) in workers.iter_mut().enumerate() {
                            if w.ci == ci && w.wid == worker {
                                w.alive = false;
                                w.ready = false;
                                w.busy = None;
                                workers_local_busy[wi] = None;
                            }
                        }
                        if had_busy && file_err.is_none() {
                            file_err =
                                Some(format!("воркер процесса {ci} погиб на чанке: {error}"));
                        }
                        if !workers.iter().any(|w| w.alive && w.ready) && file_err.is_none() {
                            file_err = Some("все воркеры дочерних процессов погибли".into());
                        }
                    }
                    ChildEvent::Msg(Ev::Ready { worker }) => {
                        if let Some(w) = workers
                            .iter_mut()
                            .find(|w| w.ci == ci && w.wid == worker && !w.ready)
                        {
                            w.ready = true;
                            *workers_ready += 1;
                        }
                    }
                    ChildEvent::Msg(Ev::PocketLoaded { .. }) => {
                        *pocket_loads += 1;
                    }
                    ChildEvent::Msg(Ev::Stopped { .. }) | ChildEvent::Msg(Ev::Exiting) => {
                        mark_child_dead(ci, workers, child_alive);
                        for (wi, w) in workers.iter().enumerate() {
                            if w.ci == ci && workers_local_busy[wi].is_some() {
                                workers_local_busy[wi] = None;
                                if file_err.is_none() {
                                    file_err =
                                        Some(format!("процесс {ci} завершился до возврата чанка"));
                                }
                            }
                        }
                    }
                    ChildEvent::Msg(_) => {}
                    ChildEvent::Closed(reason) => {
                        (d.emit)(JobEvent::Log(format!(
                            "ВНИМАНИЕ: дочерний процесс {ci} завершился: {reason}"
                        )));
                        mark_child_dead(ci, workers, child_alive);
                        for (wi, w) in workers.iter().enumerate() {
                            if w.ci == ci && workers_local_busy[wi].is_some() {
                                workers_local_busy[wi] = None;
                                if file_err.is_none() {
                                    file_err = Some(format!(
                                        "дочерний процесс {ci} погиб до возврата чанка: {reason}"
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                for ci in 0..child_alive.len() {
                    if child_alive[ci] {
                        mark_child_dead(ci, workers, child_alive);
                    }
                }
                if file_err.is_none() {
                    file_err = Some("IPC всех дочерних процессов закрыт".into());
                }
            }
        }

        // watchdog: молчащий процесс с чанком в работе
        for ci in 0..child_alive.len() {
            if child_alive[ci]
                && workers.iter().any(|w| w.ci == ci && w.busy.is_some())
                && last_seen[ci].elapsed() > d.watchdog
            {
                (d.emit)(JobEvent::Log(format!(
                    "ВНИМАНИЕ: watchdog: процесс {ci} молчит >{:?} — процесс завершается",
                    d.watchdog
                )));
                if let Some(Some(p)) = procs.get_mut(ci) {
                    let _ = p.child.kill();
                    let _ = p.child.wait();
                }
                child_alive[ci] = false;
                mark_child_dead(ci, workers, child_alive);
                if file_err.is_none() {
                    file_err = Some(format!("процесс {ci} завис (watchdog)"));
                }
            }
        }
    }

    // --- финализация/публикация (тот же sink и та же семантика G5.1) ---
    let cfg = JobConfig {
        settings: d.params.settings.clone(),
        text: text.clone(),
        out_path: job.out.clone(),
    };
    let speech_samples: u64 = 0; // детальные строки журнала чанков недоступны у диспетчера
    let outcome_like = |status: RunStatus, path: Option<std::path::PathBuf>| StreamOutcome {
        status,
        path,
        partial_path: partial_path.clone(),
        workers: d.params.max_chunks,
        base_seed: seed,
        max_buffered: window as usize,
        max_buffered_bytes: 0,
        chunks_done: done_count,
        total_chunks: total as usize,
        sample_rate: POCKET_RATE,
        speech_samples,
        pause_samples: 0,
        denoise_report: denoise.clone(),
        chunks: Vec::new(),
    };
    let res = if let Some(err) = file_err.or(sink_err) {
        let se = app_core::pipeline::StreamError {
            message: err,
            partial: partial_path.clone(),
            chunks_done: done_count,
            total: total as usize,
            base_seed: seed,
            publish_failed: false,
        };
        crate::job::finish_from_error(&cfg, &anyhow::Error::new(se), 0.0)
    } else if next_emit == total {
        // точка решения пройдена: финализация и публикация до конца (G5.1)
        (d.emit)(JobEvent::FilePhase {
            index: k,
            phase: crate::job::JobPhase::Finalizing,
        });
        match sink.finish() {
            Ok(fin) => {
                (d.emit)(JobEvent::FilePhase {
                    index: k,
                    phase: crate::job::JobPhase::Publishing,
                });
                match fin.publish() {
                    Ok(published) => crate::job::finish_from_outcome(
                        &cfg,
                        outcome_like(RunStatus::Completed, Some(published)),
                        0.0,
                        &d.cancel,
                        s.overwrite_existing,
                        existed_before,
                    ),
                    Err(e) => {
                        let se = app_core::pipeline::StreamError {
                            message: format!("ошибка публикации: {e:#}"),
                            partial: partial_path.clone(),
                            chunks_done: done_count,
                            total: total as usize,
                            base_seed: seed,
                            publish_failed: true,
                        };
                        crate::job::finish_from_error(&cfg, &anyhow::Error::new(se), 0.0)
                    }
                }
            }
            Err(e) => {
                let se = app_core::pipeline::StreamError {
                    message: format!("не удалось завершить кодирование: {e:#}"),
                    partial: partial_path.clone(),
                    chunks_done: done_count,
                    total: total as usize,
                    base_seed: seed,
                    publish_failed: false,
                };
                crate::job::finish_from_error(&cfg, &anyhow::Error::new(se), 0.0)
            }
        }
    } else {
        // отмена до точки решения — без публикации
        crate::job::finish_from_outcome(
            &cfg,
            outcome_like(RunStatus::Cancelled, None),
            0.0,
            &d.cancel,
            s.overwrite_existing,
            existed_before,
        )
    };
    crate::job::file_report_from_job_result(&job.input, &job.relative, &job.out, res)
}

// — Публичные реэкспорты/обёртки (модульная граница без цикличных зависимостей) —

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::hybrid::{spawn_child_via_test_harness, ChildJobGuard};
    use crate::job::JobPhase;
    use crate::settings::GuiSettings;
    use std::sync::atomic::AtomicBool;

    fn sandbox(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let d =
            std::env::temp_dir().join(format!("g67_pipe_{}_{}_{}", std::process::id(), tag, nanos));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Сериализация E2E с реальными дочерними процессами (как hybrid::tests).
    static E2E_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn e2e_lock() -> std::sync::MutexGuard<'static, ()> {
        E2E_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn make_jobs(d: &Path, out_root: &Path, n: usize, chunks: usize) -> Vec<HybridJob> {
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        (0..n)
            .map(|i| {
                let input = src.join(format!("f{i}.txt"));
                // предложения длиннее лимита чанка (50 байт) — каждое
                // становится отдельным чанком
                let text: String = (0..chunks)
                    .map(|c| format!("Предложение номер {c} файла {i} с дополнительными словами. "))
                    .collect();
                std::fs::write(&input, text).unwrap();
                let out = out_root.join(format!("f{i}.wav"));
                HybridJob {
                    pos: i,
                    input,
                    relative: std::path::PathBuf::from(format!("f{i}.txt")),
                    out,
                    seed: 42 + i as u64,
                }
            })
            .collect()
    }

    fn fake_settings(out_root: &Path) -> GuiSettings {
        GuiSettings {
            out_dir: out_root.display().to_string(),
            ..Default::default()
        }
    }

    fn opts_for(settings: &GuiSettings) -> PipelineOptions {
        crate::job::pipeline_options(settings, std::path::PathBuf::from("fake-model"), None)
    }

    #[derive(Clone, PartialEq, Debug)]
    enum Mark {
        Started(usize),
        Done(usize),
        PhaseGen(usize),
        PhaseFin(usize),
        PhasePub(usize),
        Progress(usize, usize),
    }

    #[allow(clippy::too_many_arguments)]
    fn run_pipe(
        d: &Path,
        out_root: &Path,
        files: usize,
        chunks: usize,
        children: usize,
        wpc: usize,
        c: usize,
        cancel_after_first_done: bool,
    ) -> (ChunkPipeOutcome, Vec<Mark>) {
        let jobs = make_jobs(d, out_root, files, chunks);
        let settings = fake_settings(out_root);
        let params = ChunkPipeParams {
            settings: settings.clone(),
            opts: opts_for(&settings),
            jobs: jobs.clone(),
            workers_per_child: wpc,
            children,
            max_chunks: c,
            fake: true,
        };
        let marks: Arc<std::sync::Mutex<Vec<Mark>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = {
            let marks = Arc::clone(&marks);
            let cancel = Arc::clone(&cancel);
            Arc::new(move |ev: JobEvent| {
                let mut m = marks.lock().unwrap();
                match ev {
                    JobEvent::FileStarted { index, .. } => m.push(Mark::Started(index)),
                    JobEvent::FileDone { index, .. } => {
                        m.push(Mark::Done(index));
                        if cancel_after_first_done {
                            cancel.store(true, Ordering::SeqCst);
                        }
                    }
                    JobEvent::FilePhase { index, phase } => match phase {
                        JobPhase::Generating => m.push(Mark::PhaseGen(index)),
                        JobPhase::Finalizing => m.push(Mark::PhaseFin(index)),
                        JobPhase::Publishing => m.push(Mark::PhasePub(index)),
                    },
                    JobEvent::FileProgress { index, done, .. } => {
                        m.push(Mark::Progress(index, done))
                    }
                    _ => {}
                }
            })
        };
        let job_guard = ChildJobGuard::new();
        let spawn = |ci: usize| spawn_child_via_test_harness(&job_guard, ci);
        let driver = ChunkPipeDriver {
            params: &params,
            cancel: Arc::clone(&cancel),
            spawn: &spawn,
            emit: &emit,
            init_settled: &|_n| {},
            watchdog: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
        };
        let outcome = run_chunk_pipeline(&driver).expect("чанк-конвейер");
        let out_marks = marks.lock().unwrap().clone();
        (outcome, out_marks)
    }

    /// E2E: 2×2, C=3, 4 файла по 3 чанка — все файлы готовы, следующий файл
    /// начинается только после терминального завершения предыдущего, фазы
    /// файла идут Generating→Finalizing→Publishing, PCM реально вернулся
    /// бинарными кадрами (WAV опубликован).
    #[test]
    fn chunkpipe_e2e_fake_sequential_files_ordered() {
        let _serial = e2e_lock();
        let d = sandbox("seq");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let (outcome, marks) = run_pipe(&d, &out_root, 4, 3, 2, 2, 3, false);
        assert_eq!(outcome.children_spawned, 2);
        assert!(
            outcome.workers_ready >= 2,
            "готовы воркеры: {}",
            outcome.workers_ready
        );
        for (i, r) in outcome.reports.iter().enumerate() {
            let r = r.as_ref().unwrap_or_else(|| panic!("файл {i} без отчёта"));
            assert_eq!(r.status, FileStatus::Completed, "файл {i}: {:?}", r.reason);
        }
        // строго последовательные файлы: Started(k+1) строго после Done(k)
        for k in 1..4usize {
            let started_k = marks
                .iter()
                .position(|m| *m == Mark::Started(k + 1))
                .unwrap_or_else(|| panic!("нет Started({})", k + 1));
            let done_k = marks
                .iter()
                .position(|m| *m == Mark::Done(k))
                .unwrap_or_else(|| panic!("нет Done({k})"));
            assert!(
                started_k > done_k,
                "файл {} начался ({started_k}) до терминального завершения {k} ({done_k})",
                k + 1
            );
        }
        // фазы каждого файла: Gen < Fin < Pub <= Done
        for k in 1..=4usize {
            let pos = |want: &Mark| {
                marks
                    .iter()
                    .position(|m| m == want)
                    .unwrap_or_else(|| panic!("нет {:?} для {k}", want))
            };
            let g = pos(&Mark::PhaseGen(k));
            let f = pos(&Mark::PhaseFin(k));
            let pb = pos(&Mark::PhasePub(k));
            let dn = pos(&Mark::Done(k));
            assert!(g < f && f < pb && pb <= dn, "фазы {k}: {g}<{f}<{pb}<={dn}");
        }
        // прогресс чанков каждого файла дошёл до фактического числа чанков
        // (не менее 2 — параллелизм чанков реален)
        for k in 1..=4usize {
            let total_chunks = outcome.reports[k - 1].as_ref().unwrap().chunks_total;
            assert!(total_chunks >= 2, "файл {k}: чанков {total_chunks} < 2");
            assert!(
                marks.contains(&Mark::Progress(k, total_chunks)),
                "файл {k}: нет прогресса {total_chunks}/{}; marks={marks:?}",
                total_chunks
            );
        }
        // WAV реально опубликованы (PCM пришёл по IPC бинарным кадром)
        for i in 0..4 {
            let bytes = std::fs::read(out_root.join(format!("f{i}.wav"))).unwrap();
            assert!(bytes.starts_with(b"RIFF"), "f{i}.wav");
        }
    }

    /// E2E: C=1 при 2×2 — конвейер работает и с минимальным окном.
    #[test]
    fn chunkpipe_e2e_fake_c1_still_completes() {
        let _serial = e2e_lock();
        let d = sandbox("c1");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let (outcome, _) = run_pipe(&d, &out_root, 2, 3, 2, 2, 1, false);
        for (i, r) in outcome.reports.iter().enumerate() {
            let r = r.as_ref().unwrap();
            assert_eq!(r.status, FileStatus::Completed, "файл {i}: {:?}", r.reason);
        }
        assert!(out_root.join("f0.wav").is_file());
        assert!(out_root.join("f1.wav").is_file());
    }

    /// E2E отмена после первого готового файла: новый файл не начинается,
    /// оставшиеся — Skipped, опубликованное цело.
    #[test]
    fn chunkpipe_e2e_fake_cancel_between_files() {
        let _serial = e2e_lock();
        let d = sandbox("cancel");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let (outcome, marks) = run_pipe(&d, &out_root, 3, 2, 1, 2, 2, true);
        let completed = outcome
            .reports
            .iter()
            .filter(|r| {
                r.as_ref()
                    .map(|r| r.status == FileStatus::Completed)
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(completed, 1, "готов ровно первый файл");
        let done1 = marks.iter().position(|m| *m == Mark::Done(1)).unwrap();
        assert!(
            marks
                .iter()
                .enumerate()
                .all(|(i, m)| i < done1 || !matches!(m, Mark::Started(3))),
            "файл 3 не стартует после отмены"
        );
        assert!(out_root.join("f0.wav").is_file());
        let skipped = outcome
            .reports
            .iter()
            .filter(|r| {
                r.as_ref()
                    .map(|r| r.status == FileStatus::Skipped)
                    .unwrap_or(false)
            })
            .count();
        assert!(skipped >= 1, "незапущенные файлы помечены Skipped");
    }
}
