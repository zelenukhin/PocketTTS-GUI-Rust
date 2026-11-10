//! G6.2: производственный гибридный пакетный CUDA — 2 дочерних процесса
//! синтеза × 2 постоянных CUDA-воркера в каждом (топология H22 из
//! CUDA-P2B.3: 38.88 с warm против 43.57 с у 1×2 на main12).
//!
//! ## Архитектура
//!
//! - Основной GUI-процесс — ДИСПЕТЧЕР: владеет планом пакета, preflight и
//!   ОБЩЕЙ очередью файлов; сам ONNX не загружает (CUDA-контекстов в GUI
//!   процессе нет).
//! - Два ДОЧЕРНИХ процесса — тот же `shtorm-gui.exe` во внутреннем режиме
//!   `--internal-batch-worker <имя_канала>` (окна нет, конфигурация приходит
//!   по IPC; exe-относительный резолвер портативной сборки работает
//!   идентично — это тот же exe). Каждый создаёт 2 постоянных `Pocket`
//!   (5 ONNX-сессий на Pocket; всего 4 Pocket / 20 сессий на пакет).
//! - IPC — Windows Named Pipes (`ipc.rs`): duplex, кадр = u32 LE + JSON,
//!   идентичность дочернего процесса проверяется по PID клиента канала,
//!   сетевые клиенты запрещены; stdout/stderr в протоколе не участвуют.
//! - Дочерние процессы приписываются к Windows Job Object с
//!   KILL_ON_JOB_CLOSE: гибель GUI-процесса не оставляет сирот.
//!
//! ## Гарантии
//!
//! - Один файл целиком обрабатывает один Pocket (чанки между процессами не
//!   делятся); очередь общая — освободившийся воркер ЛЮБОГО процесса берёт
//!   следующий файл (статического закрепления половины пакета нет).
//! - Каждый файл имеет ровно один терминальный результат: state machine
//!   Pending → Assigned → Generating → Finalizing/Publishing →
//!   Completed/Skipped/Failed/Cancelled; повторный file_finished того же
//!   файла игнорируется с диагностикой (двойной публикации нет).
//! - Гибель дочернего процесса (включая CUDA OOM) обнаруживается по разрыву
//!   IPC + коду выхода: его незавершённые файлы получают ЯВНЫЙ Failed с
//!   диагнозом (автоматического повтора нет — состояние публикации может
//!   быть неопределённым), очередь доделывает выживший процесс.
//! - Отмена: диспетчер прекращает выдачу, шлёт Cancel обоим процессам;
//!   генерация прерывается между чанками, файлы в Finalizing/Publishing
//!   дорабатывают публикацию (семантика единой точки решения G5.1);
//!   незапущенные файлы — Skipped «отменено до запуска».
//! - Безопасность файлов G5/G5.1 не меняется: preflight до воркеров,
//!   дочерний процесс ПОВТОРНО проверяет безопасность пути перед записью,
//!   уникальный `.partial`, атомарная публикация без замены по умолчанию.
//!
//! Диагностический `diag_p2b3.exe` движком НЕ является; здесь отдельный
//! production-протокол (задание G6.2 §6). Рекурсивных удалений нет.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::batch::{FileReport, FileStatus};
use crate::ipc::{self, wire, PipeEnd, PipeServer};
use crate::job::{
    ensure_output_path_safe, load_char_rules, output_format, pipeline_options, CharRulesLoad,
};
use crate::settings::GuiSettings;

use app_core::persistent::{loaned_real_worker_factory, WorkerSlot};
use app_core::pipeline::{
    synthesize_stream_with_worker_factory, PipelineOptions, RunPhase, RunStatus,
};
use app_core::pool::panic_text;
use app_core::sink::make_sink_with_overwrite;
use app_core::worker::{real_worker_factory, RealWorker};
use shtorm_engine::POCKET_RATE;

/// Таймаут подключения дочернего процесса к каналу (нормально — доли
/// секунды; запас на холодный старт exe).
pub const CHILD_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Ожидание ПОСЛЕ Shutdown, прежде чем принудительно завершить СВОЙ
/// дочерний процесс (Pocket освобождается не мгновенно).
pub const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(15);
/// Watchdog: нет событий от процесса, у которого есть НАЗНАЧЕННЫЙ файл,
/// так долго — процесс считается зависшим (kill, файлы — Failed).
/// Чанк ≈ 0.7–2.4 с; финализация ffmpeg ≈ секунды; 300 с — щедрый запас.
pub const CHILD_WATCHDOG: Duration = Duration::from_secs(300);

// ================================ протокол ================================

/// Команды диспетчера дочернему процессу.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
// протокольные сообщения: крупная нагрузка (GuiSettings) — это данные кадра,
// не горячий путь; Box только усложнил бы сериализацию
#[allow(clippy::large_enum_variant)]
pub enum Cmd {
    /// Конфигурация процесса (первое сообщение после подключения).
    Init {
        settings: GuiSettings,
        workers: usize,
        /// Тестовый режим: движок-заглушка (тишина) вместо ONNX — проверка
        /// IPC/жизненного цикла/публикации без загрузки моделей. Ставит
        /// ТОЛЬКО диспетчер (диагностический флаг SHTORM_G62_FAKE в
        /// GUI-процессе); сам воркер своё окружение не читает.
        fake: bool,
    },
    /// Назначение файла конкретному воркеру этого процесса.
    Assign {
        worker: usize,
        pos: usize,
        input: PathBuf,
        relative: PathBuf,
        out: PathBuf,
        seed: u64,
    },
    /// Отмена пакета: генерация прерывается между чанками; файлы в фазах
    /// Finalizing/Publishing дорабатывают публикацию до конца.
    Cancel,
    /// Файлов больше не будет: освободить Pocket и завершиться.
    Shutdown,
}

/// Фаза файла для IPC (зеркалирует RunPhase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseMsg {
    Generating,
    Finalizing,
    Publishing,
}

impl From<RunPhase> for PhaseMsg {
    fn from(p: RunPhase) -> Self {
        match p {
            RunPhase::Generating => PhaseMsg::Generating,
            RunPhase::Finalizing => PhaseMsg::Finalizing,
            RunPhase::Publishing => PhaseMsg::Publishing,
        }
    }
}

/// События дочернего процесса диспетчеру. `worker` — номер воркера ВНУТРИ
/// процесса (`usize::MAX` — фатальная ошибка всего процесса), `pos` —
/// позиция файла в очереди синтеза (0-based).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
// FileReport — терминальный результат файла; одно сообщение на файл
#[allow(clippy::large_enum_variant)]
pub enum Ev {
    /// Процесс запустился и подключился (PID для контроля).
    Started {
        pid: u32,
    },
    PocketLoaded {
        worker: usize,
        load_ms: u64,
    },
    /// Воркер загрузил Pocket и готов брать файлы.
    Ready {
        worker: usize,
    },
    FileStarted {
        worker: usize,
        pos: usize,
    },
    FileProgress {
        worker: usize,
        pos: usize,
        done: usize,
        total: usize,
    },
    FilePhase {
        worker: usize,
        pos: usize,
        phase: PhaseMsg,
    },
    /// Терминальный результат файла (ровно один на файл).
    FileFinished {
        worker: usize,
        pos: usize,
        report: FileReport,
    },
    /// Воркер освободился — можно назначать следующий файл.
    Idle {
        worker: usize,
    },
    /// Воркер погиб (ошибка движка/CUDA): его Pocket освобождён, файлы ему
    /// больше не назначаются; процесс работает остальными воркерами.
    WorkerError {
        worker: usize,
        error: String,
    },
    /// Воркер корректно завершился (после Shutdown / закрытия канала).
    Stopped {
        worker: usize,
    },
    /// Процесс завершается (все воркеры остановлены).
    Exiting,
}

fn cmd_to_value(cmd: &Cmd) -> Result<serde_json::Value> {
    serde_json::to_value(cmd).map_err(|e| anyhow!("сериализация команды: {e}"))
}

fn ev_to_value(ev: &Ev) -> Result<serde_json::Value> {
    serde_json::to_value(ev).map_err(|e| anyhow!("сериализация события: {e}"))
}

fn send_cmd(end: &PipeEnd, cmd: &Cmd) -> Result<()> {
    wire::send_json(end, &cmd_to_value(cmd)?).map_err(|e| anyhow!("отправка команды IPC: {e}"))
}

fn send_ev(end: &PipeEnd, ev: &Ev) -> Result<()> {
    wire::send_json(end, &ev_to_value(ev)?).map_err(|e| anyhow!("отправка события IPC: {e}"))
}

fn recv_cmd(end: &PipeEnd) -> Result<Option<Cmd>> {
    let Some(v) = wire::recv_json(end).map_err(|e| anyhow!("приём команды IPC: {e}"))?
    else {
        return Ok(None);
    };
    serde_json::from_value(v)
        .map_err(|e| anyhow!("разбор команды IPC: {e}"))
        .map(Some)
}

fn recv_ev(end: &PipeEnd) -> Result<Option<Ev>> {
    let Some(v) = wire::recv_json(end).map_err(|e| anyhow!("приём события IPC: {e}"))?
    else {
        return Ok(None);
    };
    serde_json::from_value(v)
        .map_err(|e| anyhow!("разбор события IPC: {e}"))
        .map(Some)
}

// ========================= дочерний worker-процесс =========================

/// Задание одного файла, пришедшее командой Assign (всё, что нужно воркеру;
/// план пакета процессу не передаётся).
#[derive(Debug, Clone)]
pub struct AssignJob {
    pub pos: usize,
    pub input: PathBuf,
    pub relative: PathBuf,
    pub out: PathBuf,
    pub seed: u64,
}

/// Управляющие сообщения потока воркера (Assign адресуется конкретному
/// воркеру; Cancel — общий AtomicBool процесса; Shutdown/закрытие канала
/// завершает цикл).
enum WorkerCmd {
    Assign(AssignJob),
    Shutdown,
}

/// Разделённое окружение дочернего процесса: модель/правила/ORT (без Pocket —
/// Pocket создаёт поток каждого воркера).
struct WorkerEnv {
    opts: PipelineOptions,
    model_dir: PathBuf,
}

/// Запуск внутреннего worker-режима (`--internal-batch-worker <канал>`):
/// headless, окно не открывается, сетевого сервера нет. Возврат — код
/// выхода процесса (0 — штатное завершение после Shutdown/закрытия канала).
pub fn worker_main(pipe_name: &str) -> i32 {
    match worker_main_impl(pipe_name) {
        Ok(code) => code,
        Err(e) => {
            // диагностика — в stderr родителя (управляющим протоколом НЕ является)
            eprintln!("ВНУТРЕННИЙ WORKER: {e:#}");
            1
        }
    }
}

fn worker_main_impl(pipe_name: &str) -> Result<i32> {
    let end = Arc::new(ipc::connect(pipe_name, Duration::from_secs(30))?);
    send_ev(
        &end,
        &Ev::Started {
            pid: std::process::id(),
        },
    )?;

    // чтение команд — выделенный поток (блокирующее ReadFile)
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let read_end = Arc::clone(&end);
    let reader = std::thread::spawn(move || loop {
        match recv_cmd(&read_end).transpose() {
            Some(Ok(c)) => {
                if cmd_tx.send(c).is_err() {
                    break;
                }
            }
            Some(Err(_)) => break, // битый кадр — канал считается разорванным
            None => break,         // EOF
        }
    });

    // первая команда — Init (всё остальное до неё — протокольная ошибка)
    let (settings, workers, fake) = match cmd_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Cmd::Init {
            settings,
            workers,
            fake,
        }) => (settings, workers, fake),
        Ok(other) => bail!("первой командой обязан быть init, получено {other:?}"),
        Err(_) => bail!("канал закрыт до init"),
    };
    anyhow::ensure!(
        (1..=8).contains(&workers),
        "некорректное число воркеров {workers}"
    );

    // события — общий Writer-поток (строго последовательная запись в канал)
    let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
    let write_end = Arc::clone(&end);
    let writer = std::thread::spawn(move || {
        while let Ok(ev) = ev_rx.recv() {
            if send_ev(&write_end, &ev).is_err() {
                break; // диспетчер ушёл
            }
        }
    });

    // отмена одна на процесс (команда Cancel)
    let cancel = Arc::new(AtomicBool::new(false));
    // модель/ORT — В ПРОЦЕССЕ ВОРКЕРА (не в GUI); fake — заглушка без ONNX
    let shared: Option<Arc<WorkerEnv>> = if fake {
        None
    } else {
        match build_worker_env(&settings) {
            Ok(env) => Some(Arc::new(env)),
            Err(e) => {
                // фатально для всего процесса: модель/ORT не поднялись вовсе
                let _ = ev_tx.send(Ev::WorkerError {
                    worker: usize::MAX,
                    error: format!("подготовка движка процесса: {e:#}"),
                });
                let _ = ev_tx.send(Ev::Exiting);
                drop(ev_tx);
                let _ = writer.join();
                let _ = reader.join();
                return Ok(1);
            }
        }
    };

    // per-worker каналы назначений; маршрутизация — в главном потоке
    let mut assign_tx = Vec::with_capacity(workers);
    let mut handles = Vec::with_capacity(workers);
    for wid in 0..workers {
        let (tx, rx) = mpsc::channel::<WorkerCmd>();
        assign_tx.push(tx);
        let cancel = Arc::clone(&cancel);
        let ev_tx = ev_tx.clone();
        let settings = settings.clone();
        let shared = shared.clone();
        handles.push(
            std::thread::Builder::new()
                .name(format!("hybrid-worker-{wid}"))
                .spawn(move || worker_loop(wid, rx, &cancel, &ev_tx, &settings, shared.as_deref()))
                .map_err(|e| anyhow!("запуск потока воркера {wid}: {e}"))?,
        );
    }
    // Writer завершится, когда закроются и воркерные клоны, и этот sender:
    // дропаем ПОСЛЕ маршрутизации (недоставленное назначение тоже шлёт событие)

    // маршрутизация команд: Assign — адресату, Cancel — флаг процесса,
    // Shutdown — всем воркерам; выход — по закрытию канала диспетчером
    for cmd in cmd_rx {
        match cmd {
            Cmd::Cancel => cancel.store(true, Ordering::SeqCst),
            Cmd::Shutdown => {
                for tx in &assign_tx {
                    let _ = tx.send(WorkerCmd::Shutdown);
                }
                break;
            }
            Cmd::Assign {
                worker,
                pos,
                input,
                relative,
                out,
                seed,
            } => {
                let job = AssignJob {
                    pos,
                    input: input.clone(),
                    relative: relative.clone(),
                    out: out.clone(),
                    seed,
                };
                let delivered =
                    worker < workers && assign_tx[worker].send(WorkerCmd::Assign(job)).is_ok();
                if !delivered {
                    // файл НЕ теряется: диспетчер обязан получить терминальный
                    // результат, даже если воркер уже мёртв
                    let _ = ev_tx.send(Ev::FileFinished {
                        worker,
                        pos,
                        report: FileReport::failed(
                            &crate::batch::BatchFile { input, relative },
                            Some(out),
                            "воркер дочернего процесса недоступен — файл не запускался",
                        ),
                    });
                }
            }
            Cmd::Init { .. } => {
                eprintln!("ВНУТРЕННИЙ WORKER: повторный init проигнорирован");
            }
        }
    }
    drop(assign_tx);
    drop(ev_tx); // Writer завершится, когда выйдут все воркеры
    for h in handles {
        let _ = h.join();
    }
    // Последнее событие — Exiting; отправляется напрямую (pipe writes не
    // буферизуются), после чего процесс завершается НЕМЕДЛЕННО: поток чтения
    // заблокирован в ReadFile до закрытия канала диспетчером, и его join()
    // никогда бы не вернулся (дедлок завершения, найден E2E-тестом).
    let _ = send_ev(&end, &Ev::Exiting);
    let _ = reader; // не join: заблокирован в ReadFile
    let _ = writer; // завершается сам по закрытию каналов событий
    std::process::exit(0);
}

fn build_worker_env(settings: &GuiSettings) -> Result<WorkerEnv> {
    let model_dir = crate::job::resolve_model_dir(settings)?;
    let mut char_rules = None;
    match load_char_rules(&model_dir) {
        CharRulesLoad::Absent => {}
        CharRulesLoad::Loaded(rules) => char_rules = Some(rules),
        CharRulesLoad::Warning(w) => eprintln!("ВНУТРЕННИЙ WORKER: ВНИМАНИЕ: {w}"),
    }
    crate::job::setup_ort_environment(settings)?;
    let opts = pipeline_options(settings, model_dir.clone(), char_rules);
    Ok(WorkerEnv { opts, model_dir })
}

/// Цикл одного воркера дочернего процесса: загрузка Pocket → Ready →
/// (Assign → файл целиком → FileFinished → Idle)* → Shutdown → Stopped.
/// Ошибка движка отравляет воркер (файл Failed, воркер останавливается) —
/// каскадных падений следующих файлов нет.
fn worker_loop(
    wid: usize,
    cmds: mpsc::Receiver<WorkerCmd>,
    cancel: &AtomicBool,
    ev_tx: &mpsc::Sender<Ev>,
    settings: &GuiSettings,
    shared: Option<&WorkerEnv>,
) {
    // --- загрузка постоянного Pocket (один раз на пакет) ---
    let slot: WorkerSlot<RealWorker> = match shared {
        Some(env) => {
            let t0 = Instant::now();
            let base_seed = env.opts.seed.unwrap_or(42);
            match real_worker_factory(&env.opts, base_seed)(wid, base_seed) {
                Ok(w) => {
                    let _ = ev_tx.send(Ev::PocketLoaded {
                        worker: wid,
                        load_ms: t0.elapsed().as_millis() as u64,
                    });
                    Arc::new(Mutex::new(Some(w)))
                }
                Err(e) => {
                    let _ = ev_tx.send(Ev::WorkerError {
                        worker: wid,
                        error: format!("загрузка Pocket: {e:#}"),
                    });
                    return; // воркер не стартовал; процесс жив остальными
                }
            }
        }
        None => Arc::new(Mutex::new(None)), // fake-режим: Pocket нет
    };

    let _ = ev_tx.send(Ev::Ready { worker: wid });
    loop {
        let job = match cmds.recv() {
            Ok(WorkerCmd::Assign(job)) => {
                if cancel.load(Ordering::SeqCst) {
                    // после отмены новые файлы не начинаются: честный отказ,
                    // файл не «повисает» у диспетчера
                    let _ = ev_tx.send(Ev::FileFinished {
                        worker: wid,
                        pos: job.pos,
                        report: FileReport::skipped(
                            &crate::batch::BatchFile {
                                input: job.input.clone(),
                                relative: job.relative.clone(),
                            },
                            Some(job.out.clone()),
                            "отменено до запуска",
                        ),
                    });
                    let _ = ev_tx.send(Ev::Idle { worker: wid });
                    continue;
                }
                job
            }
            Ok(WorkerCmd::Shutdown) => break,
            Err(_) => break, // диспетчер закрыл канал
        };
        let _ = ev_tx.send(Ev::FileStarted {
            worker: wid,
            pos: job.pos,
        });
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process_file(settings, shared, &slot, &job, cancel, ev_tx, wid)
        }));
        let (report, engine_broken) = match run {
            Ok(r) => r,
            Err(p) => (
                FileReport::failed(
                    &crate::batch::BatchFile {
                        input: job.input.clone(),
                        relative: job.relative.clone(),
                    },
                    Some(job.out.clone()),
                    format!("фатальный сбой воркера: {}", panic_text(p.as_ref())),
                ),
                true,
            ),
        };
        let _ = ev_tx.send(Ev::FileFinished {
            worker: wid,
            pos: job.pos,
            report,
        });
        if engine_broken {
            // Pocket в неопределённом состоянии — освободить и не переиспользовать
            if let Ok(mut guard) = slot.lock() {
                *guard = None;
            }
            let _ = ev_tx.send(Ev::WorkerError {
                worker: wid,
                error: "ошибка движка — Pocket этого воркера не переиспользуется".into(),
            });
            break;
        }
        let _ = ev_tx.send(Ev::Idle { worker: wid });
    }
    // Pocket освобождается ЗДЕСЬ — в потоке воркера, до выхода потока
    if let Ok(mut guard) = slot.lock() {
        *guard = None;
    }
    // назначения, пришедшие до закрытия канала (гонка с WorkerError),
    // честно отвергаются — файлы не зависают у мёртвого воркера
    while let Ok(WorkerCmd::Assign(job)) = cmds.try_recv() {
        let _ = ev_tx.send(Ev::FileFinished {
            worker: wid,
            pos: job.pos,
            report: FileReport::failed(
                &crate::batch::BatchFile {
                    input: job.input.clone(),
                    relative: job.relative.clone(),
                },
                Some(job.out.clone()),
                "воркер погиб до запуска файла",
            ),
        });
    }
    let _ = ev_tx.send(Ev::Stopped { worker: wid });
}

/// Один файл на постоянном Pocket (порт `execute_batch_file_persistent`
/// G6.1 с явным per-file seed от диспетчера). Повторная проверка
/// безопасности пути — ПЕРЕД созданием sink (не полагаемся только на
/// preflight GUI-процесса). Возвращает (отчёт, движок_сломан).
#[allow(clippy::too_many_arguments)]
fn process_file(
    settings: &GuiSettings,
    shared: Option<&WorkerEnv>,
    slot: &WorkerSlot<RealWorker>,
    job: &AssignJob,
    cancel: &AtomicBool,
    ev_tx: &mpsc::Sender<Ev>,
    wid: usize,
) -> (FileReport, bool) {
    let t0 = Instant::now();
    let fake = shared.is_none();
    let file = crate::batch::BatchFile {
        input: job.input.clone(),
        relative: job.relative.clone(),
    };
    let fail = |msg: String| {
        (
            FileReport::failed(&file, Some(job.out.clone()), msg.clone()),
            false,
        )
    };

    // чтение текста — ошибка файла, движок не при чём
    let text = match std::fs::read_to_string(&job.input) {
        Ok(t) => t,
        Err(e) => {
            return (
                FileReport::failed(
                    &file,
                    Some(job.out.clone()),
                    format!("прочитать {}: {e}", job.input.display()),
                ),
                false,
            )
        }
    };

    // G6.2 §10: повторная проверка применимых ограничений безопасности
    // непосредственно перед записью — В ЭТОМ процессе (модель для fake-режима
    // не резолвится: используется базовый путь onnx_base как каталог модели)
    let model_dir = match shared {
        Some(env) => env.model_dir.clone(),
        None => crate::resources::resolve_user_path(&settings.onnx_base),
    };
    if let Err(e) = ensure_output_path_safe(settings, &job.out, &model_dir) {
        return fail(format!("{e:#}"));
    }

    let existed_before = job.out.exists();
    let ffmpeg_path = if settings.ffmpeg_path.trim().is_empty() {
        None
    } else {
        Some(crate::resources::resolve_user_path(&settings.ffmpeg_path))
    };
    let preset = settings.preset_for_format();
    let overwrite = settings.overwrite_existing;
    let sink = match make_sink_with_overwrite(
        &job.out,
        24_000,
        output_format(settings.format),
        preset,
        ffmpeg_path.as_deref(),
        overwrite,
    ) {
        Ok(s) => s,
        Err(e) => return fail(format!("{e:#}")),
    };
    let _partial_path = sink.partial_path();

    let result: Result<app_core::pipeline::StreamOutcome, anyhow::Error> = if fake {
        fake_engine_run(sink, job, cancel, ev_tx, wid)
    } else {
        // боевой путь: файл целиком на одном постоянном Pocket, per-file seed
        let env = shared.expect("env в боевом режиме");
        let mut opts = env.opts.clone();
        opts.seed = Some(job.seed);
        synthesize_stream_with_worker_factory(
            &text,
            &opts,
            sink,
            |done, total| {
                let _ = ev_tx.send(Ev::FileProgress {
                    worker: wid,
                    pos: job.pos,
                    done,
                    total,
                });
            },
            cancel,
            &|ph| {
                let _ = ev_tx.send(Ev::FilePhase {
                    worker: wid,
                    pos: job.pos,
                    phase: ph.into(),
                });
            },
            loaned_real_worker_factory(Arc::clone(slot)),
        )
    };

    let elapsed = t0.elapsed().as_secs_f32();
    let cfg = crate::job::JobConfig {
        settings: settings.clone(),
        text,
        out_path: job.out.clone(),
    };
    // единая с G6.1 классификация: отмена/ошибка движка травят Pocket,
    // ошибка публикации — нет (кодирование прошло успешно)
    match result {
        Ok(outcome) => {
            let res = crate::job::finish_from_outcome(
                &cfg,
                outcome,
                elapsed,
                cancel,
                overwrite,
                existed_before,
            );
            let broken = matches!(res.status, crate::job::JobStatus::Cancelled);
            (
                crate::job::file_report_from_job_result(&job.input, &job.relative, &job.out, res),
                broken,
            )
        }
        Err(e) => {
            let publish_failed = e
                .downcast_ref::<app_core::pipeline::StreamError>()
                .map(|se| se.publish_failed)
                .unwrap_or(false);
            let res = crate::job::finish_from_error(&cfg, &e, elapsed);
            (
                crate::job::file_report_from_job_result(&job.input, &job.relative, &job.out, res),
                !publish_failed,
            )
        }
    }
}

/// Движок-заглушка fake-режима: два коротких чанка тишины через НАСТОЯЩИЙ
/// sink — жизненный цикл файла (частичный/публикация/отмена/перезапись)
/// идёт по боевым путям без ONNX. Только по указанию диспетчера (тесты).
fn fake_engine_run(
    sink: Box<dyn app_core::sink::AudioSink>,
    job: &AssignJob,
    cancel: &AtomicBool,
    ev_tx: &mpsc::Sender<Ev>,
    wid: usize,
) -> Result<app_core::pipeline::StreamOutcome> {
    let mut sink = sink;
    let partial_path = sink.partial_path();
    let total = 2usize;
    let mut done = 0usize;
    for i in 0..total {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let chunk = vec![0.0f32; POCKET_RATE as usize / 10]; // 0.1 с тишины
        let pause = if i + 1 == total { None } else { Some(0.05) };
        sink.write_chunk(i, &chunk, pause)?;
        done += 1;
        let _ = ev_tx.send(Ev::FileProgress {
            worker: wid,
            pos: job.pos,
            done,
            total,
        });
    }
    if done < total {
        return Ok(app_core::pipeline::StreamOutcome {
            status: RunStatus::Cancelled,
            path: None,
            partial_path,
            workers: 1,
            base_seed: job.seed,
            max_buffered: 0,
            max_buffered_bytes: 0,
            chunks_done: done,
            total_chunks: total,
            sample_rate: POCKET_RATE,
            speech_samples: (done as u64) * (POCKET_RATE as u64 / 10),
            pause_samples: 0,
            denoise_report: Default::default(),
            chunks: Vec::new(),
        });
    }
    let _ = ev_tx.send(Ev::FilePhase {
        worker: wid,
        pos: job.pos,
        phase: PhaseMsg::Finalizing,
    });
    let fin = sink.finish()?;
    let _ = ev_tx.send(Ev::FilePhase {
        worker: wid,
        pos: job.pos,
        phase: PhaseMsg::Publishing,
    });
    let published = fin.publish()?;
    Ok(app_core::pipeline::StreamOutcome {
        status: RunStatus::Completed,
        path: Some(published),
        partial_path,
        workers: 1,
        base_seed: job.seed,
        max_buffered: 0,
        max_buffered_bytes: 0,
        chunks_done: total,
        total_chunks: total,
        sample_rate: POCKET_RATE,
        speech_samples: (total as u64) * (POCKET_RATE as u64 / 10),
        pause_samples: 0,
        denoise_report: Default::default(),
        chunks: Vec::new(),
    })
}

// ============================== Job Object ==============================

/// Windows Job Object с KILL_ON_JOB_CLOSE: если GUI-процесс погиб, ОС
/// завершит всех ЕГО дочерних процессов — осиротевших CUDA-воркеров нет.
pub struct ChildJobGuard {
    handle: Mutex<Option<isize>>, // HANDLE; None — не создан (не фатально)
}

impl Default for ChildJobGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl ChildJobGuard {
    pub fn new() -> Self {
        Self {
            handle: Mutex::new(create_job_object()),
        }
    }

    /// Приписать дочерний процесс к job. Ошибка не фатальна (страховка ОС
    /// ухудшается, пакет продолжается) — диагностируется вызывающим.
    pub fn assign(&self, pid: u32) -> Result<()> {
        let guard = self
            .handle
            .lock()
            .map_err(|_| anyhow!("мьютекс Job Object"))?;
        assign_to_job(*guard, pid)
    }
}

impl Drop for ChildJobGuard {
    fn drop(&mut self) {
        if let Some(h) = self.handle.lock().ok().and_then(|mut g| g.take()) {
            #[cfg(windows)]
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(h as _);
            }
        }
    }
}

#[cfg(windows)]
fn create_job_object() -> Option<isize> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            windows_sys::Win32::Foundation::CloseHandle(job);
            return None;
        }
        Some(job as isize)
    }
}

#[cfg(windows)]
fn assign_to_job(handle: Option<isize>, pid: u32) -> Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    let Some(job) = handle else {
        return Ok(()); // job не создан — работаем без страховки ОС
    };
    unsafe {
        let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if proc.is_null() {
            return Err(anyhow!(
                "OpenProcess({pid}): {}",
                std::io::Error::last_os_error()
            ));
        }
        let ok = AssignProcessToJobObject(job as _, proc);
        CloseHandle(proc);
        if ok == 0 {
            return Err(anyhow!(
                "AssignProcessToJobObject({pid}): {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn create_job_object() -> Option<isize> {
    None
}

#[cfg(not(windows))]
fn assign_to_job(_handle: Option<isize>, _pid: u32) -> Result<()> {
    Ok(())
}

// ============================ спавн процессов ============================

/// Событие от дочернего процесса, доставленное диспетчеру.
#[allow(clippy::large_enum_variant)]
pub enum ChildEvent {
    Msg(Ev),
    /// Канал закрыт (процесс умер/завершился) с диагностикой причины.
    Closed(String),
}

/// Запущенный дочерний процесс с подключённым каналом: writer — конец
/// диспетчера duplex-канала, events — поток событий до EOF.
pub struct SpawnedChild {
    pub child: std::process::Child,
    pub writer: Arc<PipeEnd>,
    pub events: mpsc::Receiver<ChildEvent>,
}

/// Подключить канал к УЖЕ запущенному процессу: принять соединение с
/// таймаутом, проверить PID клиента, гонять события до EOF. Writer
/// возвращается диспетчеру (duplex: те же байты, другое направление).
pub fn connect_child(server: PipeServer, child: std::process::Child) -> Result<SpawnedChild> {
    let pid = child.id();
    let (tx, rx) = mpsc::channel::<ChildEvent>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<Arc<PipeEnd>, String>>();
    std::thread::spawn(move || {
        let end = match server.accept(CHILD_CONNECT_TIMEOUT) {
            Ok(e) => e,
            Err(e) => {
                let _ = ready_tx.send(Err(format!(
                    "не подключился к каналу за {:?}: {e}",
                    CHILD_CONNECT_TIMEOUT
                )));
                return;
            }
        };
        // проверяемая идентичность: клиент канала == запущенный процесс
        match end.peer_pid() {
            Ok(p) if p == pid => {}
            other => {
                let _ = ready_tx.send(Err(format!(
                    "к каналу подключился посторонний процесс (pid {other:?}, ожидался {pid})"
                )));
                return;
            }
        }
        let end = Arc::new(end);
        if ready_tx.send(Ok(Arc::clone(&end))).is_err() {
            return; // диспетчер уже ушёл
        }
        let read_end = Arc::clone(&end);
        loop {
            match recv_ev(&read_end).transpose() {
                Some(Ok(ev)) => {
                    if tx.send(ChildEvent::Msg(ev)).is_err() {
                        break;
                    }
                }
                Some(Err(e)) => {
                    let _ = tx.send(ChildEvent::Closed(format!("разрыв IPC: {e}")));
                    break;
                }
                None => {
                    let _ = tx.send(ChildEvent::Closed("канал закрыт (EOF)".into()));
                    break;
                }
            }
        }
        // end дропается здесь: канал диспетчер—воркер закрывается с этой
        // стороны только по факту EOF/ошибки
    });
    let writer = match ready_rx.recv_timeout(CHILD_CONNECT_TIMEOUT + Duration::from_secs(5)) {
        Ok(Ok(w)) => w,
        Ok(Err(e)) => return Err(anyhow!("дочерний процесс {pid}: {e}")),
        Err(_) => return Err(anyhow!("подключение дочернего процесса {pid} зависло")),
    };
    Ok(SpawnedChild {
        child,
        writer,
        events: rx,
    })
}

/// Тестовый спавнер (только lib-тесты): дочерний worker-процесс — ТОТ ЖЕ
/// тестовый бинарник, харнесс которого запускает единственный child-entry
/// тест (env SHTORM_G62_TEST_PIPE). Нужен real-тестам гибрида внутри
/// `cargo test` (там current_exe — харнесс, а не shtorm-gui.exe).
#[cfg(test)]
pub fn spawn_child_via_test_harness(job: &ChildJobGuard, ci: usize) -> Result<SpawnedChild> {
    let server = PipeServer::create_unique(&format!("g62j{ci}"))?;
    let exe = std::env::current_exe()?;
    let child = std::process::Command::new(&exe)
        .args([
            "--exact",
            "hybrid::tests::hybrid_child_process_entry",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("SHTORM_G62_TEST_PIPE", server.name())
        .spawn()
        .map_err(|e| anyhow!("spawn test child: {e}"))?;
    let pid = child.id();
    if let Err(e) = job.assign(pid) {
        eprintln!("ГИБРИД: Job Object не назначен (pid {pid}): {e:#}");
    }
    connect_child(server, child)
}

/// Продакшн-спавнер: канал создаётся ДО запуска процесса; дочерний процесс —
/// тот же exe (`--internal-batch-worker <канал>`), приписывается к Job Object.
pub fn spawn_child_process(
    job: &ChildJobGuard,
    exe: &Path,
    child_idx: usize,
) -> Result<SpawnedChild> {
    let server = PipeServer::create_unique(&format!("g62c{child_idx}"))
        .map_err(|e| anyhow!("создать канал дочернего процесса: {e}"))?;
    let child = std::process::Command::new(exe)
        .arg("--internal-batch-worker")
        .arg(server.name())
        .spawn()
        .map_err(|e| anyhow!("запустить дочерний процесс {}: {e}", exe.display()))?;
    let pid = child.id();
    if let Err(e) = job.assign(pid) {
        eprintln!("ГИБРИД: Job Object не назначен (pid {pid}): {e:#}");
    }
    let spawned = connect_child(server, child)?;
    Ok(spawned)
}

// ============================== диспетчер ==============================

/// Задание диспетчера: позиция в очереди синтеза + все данные файла.
#[derive(Debug, Clone)]
pub struct HybridJob {
    pub pos: usize,
    pub input: PathBuf,
    pub relative: PathBuf,
    pub out: PathBuf,
    pub seed: u64,
}

/// Параметры гибридного прогона.
pub struct HybridParams {
    pub settings: GuiSettings,
    pub jobs: Vec<HybridJob>,
    /// Постоянных воркеров (Pocket) в каждом процессе (2×2 → 2).
    pub workers_per_child: usize,
    /// Дочерних процессов CUDA-инференса (2×2 → 2).
    pub children: usize,
    /// Тестовый fake-движок (тишина вместо ONNX).
    pub fake: bool,
}

/// Итог гибридного прогона: отчёты по ПОЗИЦИЯМ jobs (в исходном порядке
/// плана), число загруженных Pocket и готовых воркеров.
pub struct HybridOutcome {
    pub reports: Vec<Option<FileReport>>,
    pub pocket_loads: usize,
    pub workers_ready: usize,
    /// Сколько дочерних процессов реально запустилось и работало.
    pub children_spawned: usize,
}

/// Действие state machine, выполняемое драйвером (IO у драйвера, решения —
/// здесь: машина тестируется без процессов).
#[derive(Debug, PartialEq)]
enum Action {
    Assign {
        child: usize,
        worker: usize,
        pos: usize,
    },
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Phase {
    Pending,
    Assigned,
    Generating,
    Finalizing,
    Publishing,
    Done,
}

struct Slot {
    phase: Phase,
    child: Option<usize>,
    report: Option<FileReport>,
}

struct WState {
    child: usize,
    wid: usize,
    ready: bool,
    alive: bool,
    busy: Option<usize>,
}

/// State machine диспетчера: общая очередь, назначения, отмена, гибель
/// процессов/воркеров, детекция завершения. Не делает IO — только решения.
struct Dispatch {
    queue: VecDeque<usize>,
    slots: Vec<Slot>,
    workers: Vec<WState>,
    wpc: usize,
    cancelled: bool,
    pocket_loads: usize,
    workers_ready: usize,
    logs: Vec<String>,
}

impl Dispatch {
    fn new(jobs: usize, children: usize, wpc: usize) -> Self {
        let mut workers = Vec::with_capacity(children * wpc);
        for c in 0..children {
            for w in 0..wpc {
                workers.push(WState {
                    child: c,
                    wid: w,
                    ready: false,
                    alive: true,
                    busy: None,
                });
            }
        }
        Self {
            queue: (0..jobs).collect(),
            slots: (0..jobs)
                .map(|_| Slot {
                    phase: Phase::Pending,
                    child: None,
                    report: None,
                })
                .collect(),
            workers,
            wpc,
            cancelled: false,
            pocket_loads: 0,
            workers_ready: 0,
            logs: Vec::new(),
        }
    }

    fn widx(&self, child: usize, wid: usize) -> usize {
        child * self.wpc + wid
    }

    fn log(&mut self, msg: impl Into<String>) {
        self.logs.push(msg.into());
    }

    fn drain_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.logs)
    }

    /// Есть ли у процесса назначенный (незавершённый) файл — условие
    /// watchdog: молчащий БЕЗ работы процесс легитимно ждёт Shutdown.
    fn child_has_busy(&self, child: usize) -> bool {
        self.workers
            .iter()
            .any(|w| w.child == child && w.alive && w.busy.is_some())
    }

    fn all_done(&self) -> bool {
        self.slots.iter().all(|s| s.report.is_some())
    }

    /// Условие выхода драйвера: все файлы получили результат ИЛИ (отмена и
    /// остались только НЕНАЗНАЧЕННЫЕ файлы — их закроет reconcile).
    fn complete_for_shutdown(&self) -> bool {
        self.slots
            .iter()
            .all(|s| s.report.is_some() || (self.cancelled && s.phase == Phase::Pending))
    }

    fn try_assign(&mut self, widx: usize) -> Option<Action> {
        if self.cancelled || self.queue.is_empty() {
            return None;
        }
        let w = &mut self.workers[widx];
        if !w.alive || !w.ready || w.busy.is_some() {
            return None;
        }
        let pos = self.queue.pop_front()?;
        let child = w.child;
        w.busy = Some(pos);
        self.slots[pos].phase = Phase::Assigned;
        self.slots[pos].child = Some(child);
        Some(Action::Assign {
            child,
            worker: w.wid,
            pos,
        })
    }

    /// Обработка события от процесса `ci`. Возвращает действия драйвера.
    fn on_event(&mut self, ci: usize, ev: &Ev) -> Vec<Action> {
        match ev {
            Ev::Started { pid } => {
                self.log(format!("процесс {ci}: подключился (pid {pid})"));
                Vec::new()
            }
            Ev::PocketLoaded { worker, load_ms } => {
                self.pocket_loads += 1;
                self.log(format!(
                    "процесс {ci}: Pocket воркера {worker} загружен за {load_ms} мс"
                ));
                Vec::new()
            }
            Ev::Ready { worker } => {
                let i = self.widx(ci, *worker);
                if !self.workers[i].ready {
                    self.workers[i].ready = true;
                    self.workers_ready += 1;
                }
                self.try_assign(i).into_iter().collect()
            }
            Ev::FileStarted { worker, pos } => {
                if let Some(slot) = self.slots.get_mut(*pos) {
                    if slot.phase == Phase::Assigned {
                        slot.phase = Phase::Generating;
                    }
                }
                let _ = worker;
                Vec::new()
            }
            Ev::FileProgress { .. } => Vec::new(),
            Ev::FilePhase { pos, phase, .. } => {
                if let Some(slot) = self.slots.get_mut(*pos) {
                    slot.phase = match phase {
                        PhaseMsg::Finalizing => Phase::Finalizing,
                        PhaseMsg::Publishing => Phase::Publishing,
                        PhaseMsg::Generating => Phase::Generating,
                    };
                }
                Vec::new()
            }
            Ev::FileFinished {
                worker,
                pos,
                report,
            } => {
                let i = self.widx(ci, *worker);
                if self.slots[*pos].report.is_some() {
                    // протокольная аномалия: второй результат того же файла
                    // не учитывается (двойной публикации быть не может)
                    self.log(format!(
                        "ВНИМАНИЕ (протокол): повторное завершение файла {pos} не учтено"
                    ));
                    return Vec::new();
                }
                self.slots[*pos].report = Some(report.clone());
                self.slots[*pos].phase = Phase::Done;
                if self.workers[i].busy == Some(*pos) {
                    self.workers[i].busy = None;
                }
                self.try_assign(i).into_iter().collect()
            }
            Ev::Idle { worker } => {
                let i = self.widx(ci, *worker);
                if self.workers[i].busy.is_none() {
                    return self.try_assign(i).into_iter().collect();
                }
                Vec::new()
            }
            Ev::WorkerError {
                worker: usize::MAX,
                error,
            } => {
                // фатально для всего процесса: ни один Pocket не поднялся
                self.log(format!(
                    "ВНИМАНИЕ: процесс {ci}: {error} — его файлы завершатся ошибкой"
                ));
                self.child_dead(ci, &format!("процесс {ci}: {error}"));
                Vec::new()
            }
            Ev::WorkerError { worker, error } => {
                let i = self.widx(ci, *worker);
                self.log(format!("ВНИМАНИЕ: процесс {ci}, воркер {worker}: {error}"));
                self.workers[i].alive = false;
                // если воркер владел файлом и не выдал результат — файл Failed
                if let Some(pos) = self.workers[i].busy.take() {
                    if self.slots[pos].report.is_none() {
                        let reason = format!("воркер погиб до завершения файла: {error}");
                        self.slots[pos].report = Some(failed_report(pos, &reason));
                        self.slots[pos].phase = Phase::Done;
                    }
                }
                Vec::new()
            }
            Ev::Stopped { worker } => {
                let i = self.widx(ci, *worker);
                self.workers[i].alive = false;
                Vec::new()
            }
            Ev::Exiting => Vec::new(),
        }
    }

    /// Гибель/завершение процесса: его воркеры мертвы, незавершённые
    /// НАЗНАЧЕННЫЕ файлы получают явный Failed (автоповтора нет — состояние
    /// публикации может быть неопределённым).
    fn on_child_dead(&mut self, ci: usize, reason: &str) {
        self.log(format!(
            "ВНИМАНИЕ: дочерний процесс {ci} завершился: {reason}"
        ));
        self.child_dead(ci, reason);
    }

    fn child_dead(&mut self, ci: usize, reason: &str) {
        for i in 0..self.workers.len() {
            if self.workers[i].child == ci {
                self.workers[i].alive = false;
                self.workers[i].ready = false;
                if let Some(pos) = self.workers[i].busy.take() {
                    if self.slots[pos].report.is_none() {
                        self.slots[pos].report = Some(failed_report(
                            pos,
                            &format!("дочерний процесс погиб до завершения файла: {reason}"),
                        ));
                        self.slots[pos].phase = Phase::Done;
                    }
                }
            }
        }
    }

    fn on_cancel(&mut self) {
        if !self.cancelled {
            self.cancelled = true;
            self.log("отмена: новые файлы не назначаются; активные дорабатывают по семантике G5.1");
        }
    }

    /// Все воркеры погибли, а файлы остались — файлы получают Failed
    /// (задание не исчезает молча).
    fn fail_remaining_no_workers(&mut self) {
        for pos in 0..self.slots.len() {
            if self.slots[pos].report.is_none() {
                self.slots[pos].report = Some(failed_report(
                    pos,
                    "не запущено: воркеры дочерних процессов завершились",
                ));
                self.slots[pos].phase = Phase::Done;
            }
        }
    }

    /// Финальное свёртывание: файлы без результата (отмена до запуска).
    fn reconcile(&mut self) {
        for pos in 0..self.slots.len() {
            if self.slots[pos].report.is_none() {
                let report = match (self.slots[pos].phase, self.cancelled) {
                    (Phase::Pending, true) => FileReport::skipped(
                        &crate::batch::BatchFile {
                            input: PathBuf::new(),
                            relative: PathBuf::new(),
                        },
                        None,
                        "отменено до запуска",
                    ),
                    (Phase::Pending, false) => {
                        failed_report(pos, "не запущено: воркеры дочерних процессов завершились")
                    }
                    _ => failed_report(pos, "не завершён: диспетчеризация прервана"),
                };
                self.slots[pos].report = Some(report);
                self.slots[pos].phase = Phase::Done;
            }
        }
    }
}

/// Failed-отчёт по позиции (вход/выход драйвер подставит из jobs).
fn failed_report(pos: usize, reason: &str) -> FileReport {
    FileReport {
        input: PathBuf::new(),
        relative: PathBuf::new(),
        output: None,
        status: FileStatus::Failed,
        reason: Some(format!("файл #{pos}: {reason}")),
        final_path: None,
        partial_path: None,
        diag_path: None,
        chunk_log_path: None,
        chunks_done: 0,
        chunks_total: 0,
    }
}

/// Драйвер гибридного прогона. Выполняется в потоке задания GUI (UI-поток
/// не блокируется). `spawn` — запуск дочернего процесса с каналом
/// (продакшн: `spawn_child_process`; тесты — свой exe/заглушки).
pub struct HybridDriver<'a> {
    pub params: &'a HybridParams,
    pub cancel: Arc<AtomicBool>,
    pub spawn: &'a dyn Fn(usize) -> Result<SpawnedChild>,
    pub log: &'a dyn Fn(String),
    pub event: &'a dyn Fn(&Ev),
    /// Watchdog молчания процесса с назначенным файлом (переопределяется
    /// тестами).
    pub watchdog: Duration,
}

pub fn run_hybrid(d: &HybridDriver) -> Result<HybridOutcome> {
    let children = d.params.children;
    anyhow::ensure!(children >= 1, "гибрид требует минимум 1 процесс");
    anyhow::ensure!(
        !d.params.jobs.is_empty(),
        "гибрид без файлов не запускается"
    );

    // --- запуск процессов (любой сбой — чистый откат уже запущенных) ---
    let mut procs: Vec<Option<SpawnedChild>> = Vec::with_capacity(children);
    for ci in 0..children {
        match (d.spawn)(ci) {
            Ok(mut sp) => {
                let init = Cmd::Init {
                    settings: d.params.settings.clone(),
                    workers: d.params.workers_per_child,
                    fake: d.params.fake,
                };
                if let Err(e) = send_cmd(&sp.writer, &init) {
                    let _ = sp.child.kill();
                    let _ = sp.child.wait();
                    procs.push(None);
                    (d.log)(format!("ВНИМАНИЕ: процесс {ci}: {e:#}"));
                    continue;
                }
                procs.push(Some(sp));
            }
            Err(e) => {
                (d.log)(format!("ВНИМАНИЕ: запуск процесса {ci}: {e:#}"));
                procs.push(None);
            }
        }
    }
    let alive_count = procs.iter().filter(|p| p.is_some()).count();
    if alive_count == 0 {
        // ни одного процесса — чистый выход без заданий (файлы пометит вызывающий)
        for p in procs.iter_mut().flatten() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
        bail!("ни один дочерний процесс не запустился");
    }
    if alive_count < children {
        (d.log)(format!(
            "ВНИМАНИЕ: работают {alive_count} из {children} процессов — очередь обработают живые"
        ));
    }

    // --- слитие событий всех процессов в один канал ---
    type Forwarder = std::thread::JoinHandle<()>;
    struct ChildProc {
        child: std::process::Child,
        writer: Arc<PipeEnd>,
        fwd: Option<Forwarder>,
    }
    let (tx, rx) = mpsc::channel::<(usize, ChildEvent)>();
    // procs_map[ci] — процесс ci (None — не запущен/умер)
    let mut procs_map: Vec<Option<ChildProc>> = (0..children).map(|_| None).collect();
    for (ci, sp) in procs.iter_mut().enumerate() {
        let Some(sp) = sp.take() else { continue };
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
        procs_map[ci] = Some(ChildProc {
            child,
            writer,
            fwd: Some(fwd),
        });
    }
    drop(tx);

    // --- основной цикл диспетчеризации ---
    let mut dispatch = Dispatch::new(d.params.jobs.len(), children, d.params.workers_per_child);
    let mut child_alive: Vec<bool> = (0..children).map(|ci| procs_map[ci].is_some()).collect();
    let mut last_seen: Vec<Instant> = (0..children).map(|_| Instant::now()).collect();
    let watchdog = d.watchdog;
    let mut cancel_sent = false;

    loop {
        if dispatch.complete_for_shutdown() {
            break;
        }
        // отмена: прекращаем выдачу, сообщаем живым процессам
        if !dispatch.cancelled && d.cancel.load(Ordering::SeqCst) {
            dispatch.on_cancel();
        }
        if dispatch.cancelled && !cancel_sent {
            cancel_sent = true;
            for (ci, p) in procs_map.iter().enumerate() {
                if child_alive[ci] {
                    if let Some(p) = p {
                        let _ = send_cmd(&p.writer, &Cmd::Cancel);
                    }
                }
            }
        }
        // ни одного живого процесса, а файлы остались — честный Failed
        if !dispatch.all_done() && child_alive.iter().all(|a| !a) {
            dispatch.fail_remaining_no_workers();
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok((ci, ChildEvent::Msg(ev))) => {
                last_seen[ci] = Instant::now();
                (d.event)(&ev);
                let actions = dispatch.on_event(ci, &ev);
                for a in actions {
                    let Action::Assign { child, worker, pos } = a;
                    let Some(job) = d.params.jobs.get(pos) else {
                        continue;
                    };
                    let cmd = Cmd::Assign {
                        worker,
                        pos,
                        input: job.input.clone(),
                        relative: job.relative.clone(),
                        out: job.out.clone(),
                        seed: job.seed,
                    };
                    if let Some(Some(p)) = procs_map.get(child) {
                        if let Err(e) = send_cmd(&p.writer, &cmd) {
                            (d.log)(format!(
                                "ВНИМАНИЕ: назначение файла {pos} процессу {child}: {e:#}"
                            ));
                        }
                    }
                }
            }
            Ok((ci, ChildEvent::Closed(reason))) => {
                if child_alive[ci] {
                    child_alive[ci] = false;
                    // контроль кода завершения: если процесс уже вышел —
                    // приложить его статус к диагнозу
                    let reason = match procs_map[ci].as_mut() {
                        Some(p) => match p.child.try_wait() {
                            Ok(Some(status)) => {
                                format!("{reason}; код завершения процесса {ci}: {status}")
                            }
                            _ => reason,
                        },
                        None => reason,
                    };
                    dispatch.on_child_dead(ci, &reason);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // watchdog: живой процесс с назначенным файлом молчит дольше
                // допустимого — kill (только свой процесс) и явный Failed
                let now = Instant::now();
                for ci in 0..children {
                    if child_alive[ci]
                        && dispatch.child_has_busy(ci)
                        && now.duration_since(last_seen[ci]) > watchdog
                    {
                        if let Some(p) = procs_map[ci].as_mut() {
                            let _ = p.child.kill();
                            let _ = p.child.wait();
                        }
                        dispatch.on_child_dead(
                            ci,
                            &format!("watchdog: нет событий {watchdog:?} при назначенном файле"),
                        );
                        child_alive[ci] = false;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        for l in dispatch.drain_logs() {
            (d.log)(l);
        }
    }
    for l in dispatch.drain_logs() {
        (d.log)(l);
    }
    dispatch.reconcile();

    // --- контролируемое завершение СВОИХ дочерних процессов ---
    for p in procs_map.iter().flatten() {
        let _ = send_cmd(&p.writer, &Cmd::Shutdown);
    }
    let deadline = Instant::now() + CHILD_EXIT_TIMEOUT;
    for p in procs_map.iter_mut().flatten() {
        let mut exited = None;
        while Instant::now() < deadline {
            match p.child.try_wait() {
                Ok(Some(st)) => {
                    exited = Some(st);
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        match exited {
            Some(st) => {
                if !st.success() {
                    (d.log)(format!(
                        "ВНИМАНИЕ: дочерний процесс завершился с кодом {st} (не 0)"
                    ));
                }
            }
            None => {
                // эскалация только для собственных дочерних процессов
                (d.log)(
                    "ВНИМАНИЕ: дочерний процесс не завершился за CHILD_EXIT_TIMEOUT — \
                     принудительное завершение"
                        .into(),
                );
                let _ = p.child.kill();
                let _ = p.child.wait();
            }
        }
    }
    for p in procs_map.iter_mut().flatten() {
        if let Some(f) = p.fwd.take() {
            let _ = f.join();
        }
    }

    Ok(HybridOutcome {
        reports: dispatch.slots.into_iter().map(|s| s.report).collect(),
        pocket_loads: dispatch.pocket_loads,
        workers_ready: dispatch.workers_ready,
        children_spawned: procs_map.iter().filter(|p| p.is_some()).count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "shtorm_g62_{}_{}_{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn report(pos: usize, status: FileStatus) -> FileReport {
        FileReport {
            input: PathBuf::from(format!("in{pos}.txt")),
            relative: PathBuf::from(format!("r{pos}.txt")),
            output: Some(PathBuf::from(format!("o{pos}.wav"))),
            status,
            reason: Some("мок".into()),
            final_path: None,
            partial_path: None,
            diag_path: None,
            chunk_log_path: None,
            chunks_done: 1,
            chunks_total: 2,
        }
    }

    fn ev_samples() -> Vec<Ev> {
        vec![
            Ev::Started { pid: 42 },
            Ev::PocketLoaded {
                worker: 0,
                load_ms: 1234,
            },
            Ev::Ready { worker: 1 },
            Ev::FileProgress {
                worker: 0,
                pos: 2,
                done: 4,
                total: 7,
            },
            Ev::FilePhase {
                worker: 0,
                pos: 2,
                phase: PhaseMsg::Publishing,
            },
            Ev::Idle { worker: 0 },
            Ev::WorkerError {
                worker: usize::MAX,
                error: "нет VRAM".into(),
            },
            Ev::Stopped { worker: 1 },
            Ev::Exiting,
        ]
    }

    // ---------------------------- протокол ----------------------------

    /// Сообщения протокола переживают JSON round-trip: кадрирование устойчиво
    /// к путям с кириллицей/пробелами и вложенным структурам FileReport.
    #[test]
    fn protocol_round_trip() {
        let ev = Ev::FileFinished {
            worker: 1,
            pos: 3,
            report: report(3, FileStatus::Completed),
        };
        let v = ev_to_value(&ev).unwrap();
        let back: Ev = serde_json::from_value(v).unwrap();
        match back {
            Ev::FileFinished { report, .. } => {
                assert_eq!(report.status, FileStatus::Completed);
                assert_eq!(report.input, PathBuf::from("in3.txt"));
            }
            other => panic!("не тот вариант: {other:?}"),
        }
        for ev in ev_samples() {
            let v = ev_to_value(&ev).unwrap();
            let back: Ev = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(ev_to_value(&back).unwrap(), v, "round-trip через JSON");
        }
        let cmd = Cmd::Assign {
            worker: 1,
            pos: 2,
            input: PathBuf::from(r"C:\текст с пробелом\файл 1.txt"),
            relative: PathBuf::from("sub/файл 1.txt"),
            out: PathBuf::from(r"D:\out\файл 1.wav"),
            seed: 42,
        };
        let v = cmd_to_value(&cmd).unwrap();
        let back: Cmd = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(cmd_to_value(&back).unwrap(), v, "round-trip через JSON");
        let init = Cmd::Init {
            settings: GuiSettings::default(),
            workers: 2,
            fake: true,
        };
        let back: Cmd = serde_json::from_value(cmd_to_value(&init).unwrap()).unwrap();
        assert!(matches!(back, Cmd::Init { fake: true, .. }));
    }

    // ------------------------- state machine -------------------------

    /// Полный прогон 5 файлов на 2×2: готовность → назначения → завершения →
    /// новые назначения; отчёты полные, фазы терминальны, очередь динамическая.
    #[test]
    fn dispatch_full_run() {
        let mut d = Dispatch::new(5, 2, 2);
        let mut assign_count = 0usize;
        let finish = |d: &mut Dispatch, ci: usize, w: usize, pos: usize| -> usize {
            let rep = report(pos, FileStatus::Completed);
            d.on_event(
                ci,
                &Ev::FileFinished {
                    worker: w,
                    pos,
                    report: rep,
                },
            )
            .len()
        };
        for (ci, w) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
            assign_count += d.on_event(ci, &Ev::Ready { worker: w }).len();
        }
        assert_eq!(assign_count, 4, "4 воркера взяли 4 файла");
        assert_eq!(d.workers_ready, 4);
        // завершение файла 0 освобождает воркера → файл 4
        assert_eq!(finish(&mut d, 0, 0, 0), 1);
        assert_eq!(finish(&mut d, 0, 1, 1), 0, "файлов больше нет");
        assert_eq!(finish(&mut d, 1, 0, 2), 0);
        assert_eq!(finish(&mut d, 1, 1, 3), 0);
        assert_eq!(finish(&mut d, 0, 0, 4), 0);
        assert!(d.all_done());
        assert!(d
            .slots
            .iter()
            .all(|s| s.report.as_ref().unwrap().status == FileStatus::Completed));
        assert!(d.slots.iter().all(|s| s.phase == Phase::Done));
    }

    /// Повторный FileFinished того же файла — протокольная аномалия: второй
    /// результат НЕ заменяет первый (двойной публикации нет), лог диагностирует.
    #[test]
    fn dispatch_duplicate_finish_ignored() {
        let mut d = Dispatch::new(1, 1, 1);
        d.on_event(0, &Ev::Ready { worker: 0 });
        d.on_event(
            0,
            &Ev::FileFinished {
                worker: 0,
                pos: 0,
                report: report(0, FileStatus::Completed),
            },
        );
        d.on_event(
            0,
            &Ev::FileFinished {
                worker: 0,
                pos: 0,
                report: report(0, FileStatus::Failed),
            },
        );
        assert_eq!(
            d.slots[0].report.as_ref().unwrap().status,
            FileStatus::Completed
        );
        assert!(d
            .drain_logs()
            .iter()
            .any(|l| l.contains("повторное завершение")));
    }

    /// Гибель процесса: назначенные ему незавершённые файлы — Failed с
    /// диагнозом; очередь доделывает выживший; ни один файл не потерян.
    #[test]
    fn dispatch_child_death_fails_its_files_survivor_finishes() {
        let mut d = Dispatch::new(4, 2, 1); // 2 процесса × 1 воркер
        assert_eq!(d.on_event(0, &Ev::Ready { worker: 0 }).len(), 1); // файл 0 → proc0
        assert_eq!(d.on_event(1, &Ev::Ready { worker: 0 }).len(), 1); // файл 1 → proc1
                                                                      // процесс 1 погиб, не завершив файл 1
        d.on_child_dead(1, "код выхода 3 (CUDA OOM)");
        assert_eq!(
            d.slots[1].report.as_ref().unwrap().status,
            FileStatus::Failed
        );
        let reason = d.slots[1].report.as_ref().unwrap().reason.clone().unwrap();
        assert!(reason.contains("CUDA OOM"), "{reason}");
        // воркер процесса 0 после файла 0 берёт файл 2 (погибший файл 1 уже
        // закрыт Failed'ом — повторного запуска нет)
        assert_eq!(
            d.on_event(
                0,
                &Ev::FileFinished {
                    worker: 0,
                    pos: 0,
                    report: report(0, FileStatus::Completed),
                }
            )
            .len(),
            1
        );
        d.on_event(
            0,
            &Ev::FileFinished {
                worker: 0,
                pos: 2,
                report: report(2, FileStatus::Completed),
            },
        );
        d.on_event(
            0,
            &Ev::FileFinished {
                worker: 0,
                pos: 3,
                report: report(3, FileStatus::Completed),
            },
        );
        assert!(d.all_done());
        let failed = d
            .slots
            .iter()
            .filter(|s| s.report.as_ref().unwrap().status == FileStatus::Failed)
            .count();
        assert_eq!(failed, 1);
        let completed = d
            .slots
            .iter()
            .filter(|s| s.report.as_ref().unwrap().status == FileStatus::Completed)
            .count();
        assert_eq!(completed, 3);
    }

    /// Отмена: выдача прекращается, незапущенные файлы — Skipped «отменено
    /// до запуска»; уже назначенные завершаются воркерами по своей семантике.
    #[test]
    fn dispatch_cancel_stops_assignments() {
        let mut d = Dispatch::new(4, 1, 2);
        assert_eq!(d.on_event(0, &Ev::Ready { worker: 0 }).len(), 1);
        assert_eq!(d.on_event(0, &Ev::Ready { worker: 1 }).len(), 1);
        d.on_cancel();
        // свободившийся воркер НЕ получает новый файл
        assert_eq!(
            d.on_event(
                0,
                &Ev::FileFinished {
                    worker: 0,
                    pos: 0,
                    report: report(0, FileStatus::Completed),
                }
            )
            .len(),
            0
        );
        assert!(d.queue.iter().any(|p| *p > 0));
        d.reconcile();
        assert!(d.all_done());
        let skipped = d
            .slots
            .iter()
            .filter(|s| s.report.as_ref().unwrap().status == FileStatus::Skipped)
            .count();
        assert_eq!(skipped, 2, "незапущенные файлы — Skipped");
        assert!(d.slots[2]
            .report
            .as_ref()
            .unwrap()
            .reason
            .as_deref()
            .unwrap()
            .contains("отменено до запуска"));
        // назначенный, но не завершённый файл — Failed, а не «отменено до запуска»
        assert_eq!(
            d.slots[1].report.as_ref().unwrap().status,
            FileStatus::Failed
        );
        assert!(d.slots[1]
            .report
            .as_ref()
            .unwrap()
            .reason
            .as_deref()
            .unwrap()
            .contains("не завершён"));
    }

    /// Фатальная ошибка процесса (модель/ORT не поднялись): назначенный файл
    /// Failed, оставшиеся — Failed «воркеры завершились».
    #[test]
    fn dispatch_process_fatal_error() {
        let mut d = Dispatch::new(2, 1, 1);
        assert_eq!(d.on_event(0, &Ev::Ready { worker: 0 }).len(), 1);
        d.on_event(
            0,
            &Ev::WorkerError {
                worker: usize::MAX,
                error: "подготовка движка процесса: CUDA недоступна".into(),
            },
        );
        assert_eq!(
            d.slots[0].report.as_ref().unwrap().status,
            FileStatus::Failed
        );
        d.reconcile();
        let reason = d.slots[1].report.as_ref().unwrap().reason.clone().unwrap();
        assert!(reason.contains("завершились"), "{reason}");
        assert!(d.all_done());
    }

    /// Ошибка ОДНОГО воркера (движок): его файл уже закрыт Failed'ом по
    /// протоколу, воркер мёртв, файлы доделывает сосед.
    #[test]
    fn dispatch_single_worker_error_poisons_only_him() {
        let mut d = Dispatch::new(4, 1, 2);
        for w in 0..2 {
            assert_eq!(d.on_event(0, &Ev::Ready { worker: w }).len(), 1);
        }
        // реальный порядок протокола: FileFinished(ошибка движка) -> воркер
        // свободен -> диспетчер успевает назначить файл 2 -> WorkerError
        assert_eq!(
            d.on_event(
                0,
                &Ev::FileFinished {
                    worker: 0,
                    pos: 0,
                    report: report(0, FileStatus::Failed),
                }
            )
            .len(),
            1,
            "файл 2 назначен только что освободившемуся воркеру 0"
        );
        d.on_event(
            0,
            &Ev::WorkerError {
                worker: 0,
                error: "ошибка движка".into(),
            },
        );
        assert!(!d.workers[0].alive, "воркер 0 отравлен");
        assert!(d.workers[1].alive);
        // файл 2, назначенный умершему в гонке, — Failed с диагнозом
        assert_eq!(
            d.slots[2].report.as_ref().unwrap().status,
            FileStatus::Failed
        );
        assert!(d.slots[2]
            .report
            .as_ref()
            .unwrap()
            .reason
            .as_deref()
            .unwrap()
            .contains("воркер погиб"));
        // сосед доделывает свою работу и берёт последний файл
        assert_eq!(
            d.on_event(
                0,
                &Ev::FileFinished {
                    worker: 1,
                    pos: 1,
                    report: report(1, FileStatus::Completed),
                }
            )
            .len(),
            1,
            "файл 3 назначен живому воркеру 1"
        );
        d.on_event(
            0,
            &Ev::FileFinished {
                worker: 1,
                pos: 3,
                report: report(3, FileStatus::Completed),
            },
        );
        d.reconcile();
        assert!(d.all_done());
        let completed = d
            .slots
            .iter()
            .filter(|s| s.report.as_ref().unwrap().status == FileStatus::Completed)
            .count();
        assert_eq!(completed, 2);
    }

    /// Все процессы погибли, файлы остались — явный Failed, а не молчание.
    #[test]
    fn dispatch_no_workers_remaining_fails_files() {
        let mut d = Dispatch::new(2, 1, 1);
        assert_eq!(d.on_event(0, &Ev::Ready { worker: 0 }).len(), 1);
        d.on_child_dead(0, "упал");
        d.fail_remaining_no_workers();
        assert!(d.all_done());
        assert!(d.slots[1]
            .report
            .as_ref()
            .unwrap()
            .reason
            .as_deref()
            .unwrap()
            .contains("завершились"));
    }

    // --------------------------- worker_loop ---------------------------

    /// Цикл воркера в fake-режиме (без ONNX): настоящий sink и публикация,
    /// события по протоколу; Shutdown останавливает.
    #[test]
    fn worker_loop_fake_engine_end_to_end() {
        let d = sandbox("wloop");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "текст").unwrap();
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let mut settings = GuiSettings {
            out_dir: out_root.display().to_string(),
            format: crate::settings::OutputFormatChoice::Wav,
            ..Default::default()
        };
        settings.clamped();
        let out = out_root.join("a.wav");

        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
        let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
        let cancel = AtomicBool::new(false);
        let s2 = settings.clone();
        let handle = std::thread::spawn(move || {
            worker_loop(0, cmd_rx, &cancel, &ev_tx, &s2, None);
        });
        match ev_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Ev::Ready { worker: 0 } => {}
            other => panic!("ожидался Ready, пришло {other:?}"),
        }
        cmd_tx
            .send(WorkerCmd::Assign(AssignJob {
                pos: 0,
                input: src.join("a.txt"),
                relative: PathBuf::from("a.txt"),
                out: out.clone(),
                seed: 42,
            }))
            .unwrap();
        let mut saw_progress = false;
        let mut saw_publishing = false;
        let mut final_report = None;
        for _ in 0..16 {
            match ev_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Ev::FileStarted { .. } => {}
                Ev::FileProgress { done, total, .. } => {
                    assert_eq!(total, 2);
                    saw_progress = done >= 1;
                }
                Ev::FilePhase { phase, .. } => saw_publishing |= phase == PhaseMsg::Publishing,
                Ev::FileFinished { report, .. } => {
                    final_report = Some(report);
                    break;
                }
                other => panic!("неожиданное событие: {other:?}"),
            }
        }
        let rep = final_report.expect("FileFinished");
        assert_eq!(rep.status, FileStatus::Completed, "{:?}", rep.reason);
        assert!(saw_progress && saw_publishing);
        let bytes = std::fs::read(&out).expect("опубликованный файл");
        assert!(bytes.starts_with(b"RIFF"));
        match ev_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Ev::Idle { worker: 0 } => {}
            other => panic!("ожидался Idle: {other:?}"),
        }
        cmd_tx.send(WorkerCmd::Shutdown).unwrap();
        match ev_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Ev::Stopped { worker: 0 } => {}
            other => panic!("ожидался Stopped: {other:?}"),
        }
        handle.join().unwrap();
    }

    /// После Cancel воркер НЕ начинает новый файл: честный Skipped «отменено
    /// до запуска» + Idle (файл не висит у диспетчера).
    #[test]
    fn worker_loop_cancel_rejects_new_assignment() {
        let d = sandbox("wcancel");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "т").unwrap();
        let mut settings = GuiSettings {
            out_dir: d.join("out").display().to_string(),
            ..Default::default()
        };
        settings.clamped();

        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
        let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
        let cancel = AtomicBool::new(true); // уже отменено
        let s2 = settings.clone();
        std::thread::spawn(move || {
            worker_loop(1, cmd_rx, &cancel, &ev_tx, &s2, None);
        });
        match ev_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Ev::Ready { worker: 1 } => {}
            other => panic!("{other:?}"),
        }
        cmd_tx
            .send(WorkerCmd::Assign(AssignJob {
                pos: 7,
                input: src.join("a.txt"),
                relative: PathBuf::from("a.txt"),
                out: d.join("out").join("a.wav"),
                seed: 1,
            }))
            .unwrap();
        match ev_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Ev::FileFinished { report, .. } => {
                assert_eq!(report.status, FileStatus::Skipped);
                assert!(report.reason.unwrap().contains("отменено до запуска"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            ev_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Ev::Idle { .. }
        ));
    }

    // ------------- E2E: диспетчер + НАСТОЯЩИЕ дочерние процессы -------------

    /// Точка входа дочернего процесса в E2E-тестах: этот «тест» исполняется
    /// в ОТДЕЛЬНОМ процессе тестового бинарника (его запускает драйвер
    /// теста с env SHTORM_G62_TEST_PIPE), подключается к каналу и отрабатывает
    /// worker-роль. При обычном прогоне тестов переменной нет — тихий выход.
    #[test]
    fn hybrid_child_process_entry() {
        let pipe = match std::env::var("SHTORM_G62_TEST_PIPE") {
            Ok(p) => p,
            Err(_) => return, // обычный прогон тестов — не child-режим
        };
        let code = worker_main(&pipe);
        assert_eq!(code, 0, "child-процесс обязан завершаться кодом 0");
    }

    fn fake_settings(out_dir: &Path) -> GuiSettings {
        let mut s = GuiSettings {
            out_dir: out_dir.display().to_string(),
            format: crate::settings::OutputFormatChoice::Wav,
            workers: None,
            ..Default::default()
        };
        s.clamped();
        s
    }

    fn make_jobs(dir: &Path, out_root: &Path, n: usize) -> Vec<HybridJob> {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        (0..n)
            .map(|i| {
                let input = src.join(format!("f{i}.txt"));
                std::fs::write(&input, format!("текст {i}")).unwrap();
                HybridJob {
                    pos: i,
                    input,
                    relative: PathBuf::from(format!("f{i}.txt")),
                    out: out_root.join(format!("f{i}.wav")),
                    seed: 42,
                }
            })
            .collect()
    }

    /// Спавнер теста: тот же тестовый бинарник в роли дочернего процесса
    /// (харнесс запускает единственный child-entry «тест»).
    fn spawn_test_child(job: &ChildJobGuard, ci: usize) -> Result<SpawnedChild> {
        spawn_child_via_test_harness(job, ci)
    }

    /// E2E: полный гибридный прогон 2×2 в fake-режиме на настоящих дочерних
    /// процессах и настоящем Named Pipe IPC — без GPU/моделей. Проверяет:
    /// готовность 4 воркеров, динамическую очередь, публикацию файлов,
    /// контролируемое завершение детей и полноту отчётов.
    #[test]
    fn hybrid_e2e_fake_processes_two_by_two() {
        let d = sandbox("e2e22");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 5);
        let params = HybridParams {
            settings: fake_settings(&out_root),
            jobs: jobs.clone(),
            workers_per_child: 2,
            children: 2,
            fake: true,
        };
        let logs: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let log = |m: String| {
            logs.lock().unwrap().push(m);
        };
        let events: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let event = |ev: &Ev| {
            events
                .lock()
                .unwrap()
                .push(serde_json::to_string(&ev_to_value(ev).unwrap()).unwrap());
        };
        let job_guard = ChildJobGuard::new();
        let spawn = |ci: usize| spawn_test_child(&job_guard, ci);
        let driver = HybridDriver {
            params: &params,
            cancel: Arc::new(AtomicBool::new(false)),
            spawn: &spawn,
            log: &log,
            event: &event,
            watchdog: Duration::from_secs(60),
        };
        let outcome = run_hybrid(&driver).expect("гибридный прогон");
        assert_eq!(outcome.reports.len(), 5);
        assert!(outcome.reports.iter().all(|r| r.is_some()));
        for (i, r) in outcome.reports.iter().enumerate() {
            let r = r.as_ref().unwrap();
            assert_eq!(r.status, FileStatus::Completed, "файл {i}: {:?}", r.reason);
        }
        assert_eq!(outcome.workers_ready, 4, "4 воркера 2×2 готовы");
        assert_eq!(outcome.pocket_loads, 0, "fake-режим без Pocket");
        for job in &jobs {
            let bytes = std::fs::read(&job.out).expect("опубликованный файл");
            assert!(bytes.starts_with(b"RIFF"), "{}", job.out.display());
        }
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("file_started")));
        assert!(logs.lock().unwrap().iter().any(|l| l.contains("процесс 0")));
    }

    /// E2E отмена: после первого готового файла ставим общий cancel — новые
    /// файлы не назначаются, незапущенные Skipped «отменено до запуска»;
    /// опубликованное до отмены цело.
    #[test]
    fn hybrid_e2e_fake_cancel() {
        let d = sandbox("e2cancel");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 6);
        let params = HybridParams {
            settings: fake_settings(&out_root),
            jobs: jobs.clone(),
            workers_per_child: 2,
            children: 2,
            fake: true,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_c = Arc::clone(&cancel);
        let seen_done = std::sync::Mutex::new(0usize);
        let event = move |ev: &Ev| {
            if let Ev::FileFinished { report, .. } = ev {
                if report.status == FileStatus::Completed {
                    let mut g = seen_done.lock().unwrap();
                    *g += 1;
                    if *g >= 1 {
                        cancel_c.store(true, Ordering::SeqCst);
                    }
                }
            }
        };
        let log = |_m: String| {};
        let job_guard = ChildJobGuard::new();
        let spawn = |ci: usize| spawn_test_child(&job_guard, ci);
        let driver = HybridDriver {
            params: &params,
            cancel: Arc::clone(&cancel),
            spawn: &spawn,
            log: &log,
            event: &event,
            watchdog: Duration::from_secs(60),
        };
        let outcome = run_hybrid(&driver).expect("прогон с отменой");
        let completed = outcome
            .reports
            .iter()
            .filter(|r| r.as_ref().unwrap().status == FileStatus::Completed)
            .count();
        let skipped = outcome
            .reports
            .iter()
            .filter(|r| r.as_ref().unwrap().status == FileStatus::Skipped)
            .count();
        let cancelled = outcome
            .reports
            .iter()
            .filter(|r| r.as_ref().unwrap().status == FileStatus::Cancelled)
            .count();
        assert!(completed >= 1, "готовое до отмены сохранено");
        assert_eq!(
            completed + skipped + cancelled,
            6,
            "все файлы получили результат"
        );
        assert!(skipped >= 1, "есть незапущенные после отмены");
        for (i, r) in outcome.reports.iter().enumerate() {
            let r = r.as_ref().unwrap();
            if r.status == FileStatus::Skipped {
                assert!(
                    r.reason.as_deref().unwrap().contains("отменено до запуска"),
                    "файл {i}: {:?}",
                    r.reason
                );
            }
        }
        for job in &jobs {
            if job.out.exists() {
                assert!(std::fs::read(&job.out).unwrap().starts_with(b"RIFF"));
            }
        }
    }

    /// E2E гибель дочернего процесса: принудительное завершение PID одного
    /// ребёнка во время пакета — его незавершённые файлы получают Failed с
    /// диагнозом гибели, очередь доделывает выживший; ничего не теряется.
    /// Управляемое внедрение сбоя: taskkill СВОЕГО дочернего PID.
    #[test]
    fn hybrid_e2e_fake_child_killed() {
        let d = sandbox("e2kill");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 6);
        let params = HybridParams {
            settings: fake_settings(&out_root),
            jobs: jobs.clone(),
            workers_per_child: 2,
            children: 2,
            fake: true,
        };
        let pids: Arc<std::sync::Mutex<Vec<u32>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_all: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let kill_done = Arc::new(AtomicBool::new(false));
        let job_guard = ChildJobGuard::new();
        let pids_c = Arc::clone(&pids);
        let log_c = Arc::clone(&log_all);
        // Управляемое внедрение сбоя: на ПЕРВОМ FileStarted (пакет уже идёт)
        // принудительно завершаем СВОЙ дочерний процесс №1 по PID —
        // никаких сторонних процессов не трогаем.
        let kill_c = Arc::clone(&kill_done);
        let pids_k = Arc::clone(&pids);
        let event = move |ev: &Ev| {
            if matches!(ev, Ev::FileStarted { .. }) && !kill_c.swap(true, Ordering::SeqCst) {
                let guard = pids_k.lock().unwrap();
                if let Some(&victim) = guard.get(1) {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &victim.to_string(), "/F"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        };
        let run_thread = std::thread::spawn(move || {
            let spawn = move |ci: usize| {
                let sp = spawn_test_child(&job_guard, ci)?;
                pids_c.lock().unwrap().push(sp.child.id());
                Ok(sp)
            };
            let log = |m: String| log_c.lock().unwrap().push(m);
            let driver = HybridDriver {
                params: &params,
                cancel: Arc::new(AtomicBool::new(false)),
                spawn: &spawn,
                log: &log,
                event: &event,
                watchdog: Duration::from_secs(60),
            };
            run_hybrid(&driver)
        });
        let outcome = run_thread
            .join()
            .expect("поток прогона")
            .expect("прогон пережил гибель процесса");
        assert!(
            kill_done.load(Ordering::SeqCst),
            "сбой внедрён во время пакета"
        );
        assert_eq!(outcome.reports.len(), 6);
        assert!(outcome.reports.iter().all(|r| r.is_some()));
        let failed: Vec<&FileReport> = outcome
            .reports
            .iter()
            .filter_map(|r| r.as_ref().filter(|r| r.status == FileStatus::Failed))
            .collect();
        let completed = outcome
            .reports
            .iter()
            .filter(|r| r.as_ref().unwrap().status == FileStatus::Completed)
            .count();
        assert!(completed >= 1, "выживший процесс доделал свою часть");
        for r in &failed {
            let reason = r.reason.as_deref().unwrap_or_default();
            assert!(
                reason.contains("процесс") || reason.contains("воркер"),
                "диагноз гибели: {reason}"
            );
        }
        let logs = log_all.lock().unwrap().join("\n");
        assert!(
            logs.contains("дочерний процесс 1"),
            "диагностика в журнале: {logs}"
        );
        for job in &jobs {
            if job.out.exists() {
                assert!(std::fs::read(&job.out).unwrap().starts_with(b"RIFF"));
            }
        }
    }
}
