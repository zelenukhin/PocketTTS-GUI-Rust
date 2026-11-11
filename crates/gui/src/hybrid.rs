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
/// G6.2.1: ограниченный startup/initialization timeout процесса. Модель и ORT
/// в дочернем процессе поднимаются за секунды (4 Pocket параллельно max
/// ~10–20 с на медленном диске); 120 с — щедрый предел, после которого
/// молчащая инициализация считается зависшей. Процесс без готовых воркеров
/// завершается (только свой PID), воркеры — Failed-диагностика.
pub const CHILD_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
/// G6.2.1: ожидание слива очереди событий единственным writer'ом дочернего
/// процесса. Сообщения крошечные (килобайты), канал 1 МиБ — 5 с достаточно;
/// при разорванном IPC writer завершается по ошибке записи.
const CHILD_WRITER_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
/// G6.2.1: ожидание после CancelIoEx, чтобы поток чтения/записи вернулся из
/// отменённой overlapped-операции. Не освобождаем буферы, пока поток жив;
/// если и это не удалось — процесс завершается принудительно (ОС освободит
/// всё), но НЕ ждёт бесконечно.
const CHILD_IO_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Watchdog: нет событий от процесса, у которого есть НАЗНАЧЕННЫЙ файл,
/// так долго — процесс считается зависшим (kill, файлы — Failed).
/// Чанк ≈ 0.7–2.4 с; финализация ffmpeg ≈ секунды; 300 с — щедрый запас.
pub const CHILD_WATCHDOG: Duration = Duration::from_secs(300);

// ================================ протокол ================================

/// G6.2.1: управляемое внедрение сбоев ТОЛЬКО для fake-режима/тестов
/// (диспетчер передаёт его командой Init). Сам дочерний процесс окружение
/// не читает: без этого поля все значения нулевые и поведение боевое.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestFaults {
    /// Фатальная ошибка подготовки движка процесса (build_worker_env) до
    /// Ready — процесс обязан завершиться ненулевым кодом.
    #[serde(default)]
    pub fail_init: bool,
    /// Задержка инициализации: тест успевает принудительно завершить только
    /// свой дочерний PID до Ready.
    #[serde(default)]
    pub init_delay_ms: u64,
    /// Воркеры, чья «загрузка Pocket» срывается: WorkerError до Ready,
    /// процесс продолжает работу остальными воркерами (частичная готовность).
    #[serde(default)]
    pub fail_workers: Vec<usize>,
}

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
        /// G6.2.1: управляемое внедрение сбоев жизненного цикла (тестовый
        /// режим fake; в бою — Default: все поля нулевые).
        #[serde(default)]
        test: TestFaults,
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
    if let Err(e) = send_ev(
        &end,
        &Ev::Started {
            pid: std::process::id(),
        },
    ) {
        // Диспетчер уже разорвал канал: доставлять события некому.
        eprintln!("ВНУТРЕННИЙ WORKER: стартовое событие не доставлено: {e:#}");
        return Ok(1);
    }

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
    let (settings, workers, fake, test) = match cmd_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Cmd::Init {
            settings,
            workers,
            fake,
            test,
        }) => (settings, workers, fake, test),
        Ok(other) => bail!("первой командой обязан быть init, получено {other:?}"),
        Err(_) => bail!("канал закрыт до init"),
    };
    anyhow::ensure!(
        (1..=8).contains(&workers),
        "некорректное число воркеров {workers}"
    );
    // тестовый сценарий «процесс убит во время инициализации»: даём
    // диспетчеру время принудительно завершить ТОЛЬКО свой PID до Ready
    if test.init_delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(test.init_delay_ms));
    }

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
                // фатально для всего процесса: модель/ORT не поднялись вовсе.
                // Сообщаем ДО Exiting; завершение — ограниченное (finish_child).
                let _ = ev_tx.send(Ev::WorkerError {
                    worker: usize::MAX,
                    error: format!("подготовка движка процесса: {e:#}"),
                });
                let _ = ev_tx.send(Ev::Exiting);
                return Ok(finish_child(&end, ev_tx, writer, reader, 1));
            }
        }
    };
    if test.fail_init {
        // управляемое внедрение: тот же терминальный путь, что и реальный
        // сбой build_worker_env (до создания воркеров)
        let _ = ev_tx.send(Ev::WorkerError {
            worker: usize::MAX,
            error: "тестовое внедрение: подготовка движка процесса".into(),
        });
        let _ = ev_tx.send(Ev::Exiting);
        return Ok(finish_child(&end, ev_tx, writer, reader, 1));
    }

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
        let test = test.clone();
        handles.push(
            std::thread::Builder::new()
                .name(format!("hybrid-worker-{wid}"))
                .spawn(move || {
                    worker_loop(
                        wid,
                        rx,
                        &cancel,
                        &ev_tx,
                        &settings,
                        shared.as_deref(),
                        &test,
                    )
                })
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
    for h in handles {
        let _ = h.join();
    }
    // G6.2.1: Exiting идёт ЧЕРЕЗ ТОТ ЖЕ канал событий и строго ПОСЛЕ всех
    // FileFinished/WorkerError/Stopped: все клоны ev_tx у воркеров уже
    // дропнуты, очередь FIFO — writer не может обогнать ранее поставленные
    // события. Прямая параллельная запись в канал удалена.
    let _ = ev_tx.send(Ev::Exiting);
    Ok(finish_child(&end, ev_tx, writer, reader, 0))
}

/// G6.2.1: ограниченное завершение дочернего IPC. Единственный writer
/// дописывает очередь событий (включая финальный Exiting), затем ожидающее
/// чтение снимается `CancelIoEx`: reader не ждёт, пока диспетчер закроет
/// канал. Буферы и OVERLAPPED освобождаются только после возврата потока;
/// при невосстановимом зависании процесс выходит принудительно (ОС освободит
/// ресурсы), но НЕ ждёт бесконечно.
fn finish_child(
    end: &Arc<PipeEnd>,
    ev_tx: mpsc::Sender<Ev>,
    writer: std::thread::JoinHandle<()>,
    reader: std::thread::JoinHandle<()>,
    code: i32,
) -> i32 {
    let mut writer = Some(writer);
    let mut reader = Some(reader);
    drop(ev_tx); // очередь закрыта: writer дописывает всё и выходит
    if !join_bounded(&mut writer, CHILD_WRITER_FLUSH_TIMEOUT) {
        let _ = end.cancel_io(); // канал забит/разорван — снять записи
        if !join_bounded(&mut writer, CHILD_IO_JOIN_TIMEOUT) {
            eprintln!("ВНУТРЕННИЙ WORKER: writer не завершился — принудительный выход");
            std::process::exit(code);
        }
    }
    let _ = end.cancel_io();
    if !join_bounded(&mut reader, CHILD_IO_JOIN_TIMEOUT) {
        eprintln!("ВНУТРЕННИЙ WORKER: reader не завершился — принудительный выход");
        std::process::exit(code);
    }
    code
}

/// Ограниченное ожидание завершения потока. При таймауте handle остаётся
/// живым (вызывающий решает, что делать), join не вызывается.
fn join_bounded(h: &mut Option<std::thread::JoinHandle<()>>, timeout: Duration) -> bool {
    let Some(handle) = h.as_ref() else {
        return true;
    };
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Some(h) = h.take() {
        let _ = h.join();
    }
    true
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
    test: &TestFaults,
) {
    // G6.2.1: управляемое внедрение «сбой загрузки Pocket» — воркер
    // терминально мёртв ДО Ready; процесс продолжает работу остальными
    // воркерами (частично инициализированный процесс). Без внедрения
    // ветка не выполняется.
    if test.fail_workers.contains(&wid) {
        let _ = ev_tx.send(Ev::WorkerError {
            worker: wid,
            error: "тестовое внедрение: загрузка Pocket".into(),
        });
        return;
    }
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
///
/// G6.2.1 — FAIL-CLOSED: отсутствие защиты больше не считается «нефатальным».
/// Гибридный запуск обязан проверить `ready()` до первого процесса и
/// прервать запуск, если Job Object не создан; `assign()` для неназначенного
/// процесса возвращает ошибку, и вызывающий обязан завершить уже созданные
/// собственные процессы, не начиная синтез.
pub struct ChildJobGuard {
    handle: Mutex<Option<isize>>, // HANDLE; None — не создан (запуск запрещён)
    creation_error: Mutex<Option<String>>,
    /// Тестовое внедрение: первый assign отвергается (E2E-H).
    fail_assign_once: AtomicBool,
}

impl Default for ChildJobGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl ChildJobGuard {
    pub fn new() -> Self {
        Self::create(false, false)
    }

    /// G6.2.1: управляемое внедрение отказов Job Object (только тесты).
    #[cfg(test)]
    pub fn with_faults_for_test(fail_create: bool, fail_assign_once: bool) -> Self {
        Self::create(fail_create, fail_assign_once)
    }

    fn create(fail_create: bool, fail_assign_once: bool) -> Self {
        let (handle, creation_error) = if fail_create {
            (
                None,
                Some("тестовое внедрение: CreateJobObject/SetInformationJobObject".to_string()),
            )
        } else {
            match create_job_object() {
                Ok(h) => (Some(h), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        };
        Self {
            handle: Mutex::new(handle),
            creation_error: Mutex::new(creation_error),
            fail_assign_once: AtomicBool::new(fail_assign_once),
        }
    }

    /// Проверить, что KILL_ON_JOB_CLOSE реально действует. Ошибка означает,
    /// что гибридный запуск обязан отказаться от старта.
    pub fn ready(&self) -> Result<()> {
        if let Some(e) = self
            .creation_error
            .lock()
            .map_err(|_| anyhow!("мьютекс ошибки Job Object"))?
            .clone()
        {
            bail!("Windows Job Object KILL_ON_JOB_CLOSE не создан: {e}");
        }
        if self
            .handle
            .lock()
            .map_err(|_| anyhow!("мьютекс Job Object"))?
            .is_none()
        {
            bail!("Windows Job Object KILL_ON_JOB_CLOSE недоступен");
        }
        Ok(())
    }

    /// Приписать дочерний процесс к job. FAIL-CLOSED: без действующего job
    /// процесс не запускается; вызывающий обязан завершить уже созданные
    /// собственные процессы.
    pub fn assign(&self, pid: u32) -> Result<()> {
        if self.fail_assign_once.swap(false, Ordering::SeqCst) {
            bail!("тестовое внедрение: AssignProcessToJobObject({pid}) отвергнут");
        }
        let guard = self
            .handle
            .lock()
            .map_err(|_| anyhow!("мьютекс Job Object"))?;
        let Some(job) = *guard else {
            bail!("Job Object не создан — дочерний процесс {pid} без KILL_ON_JOB_CLOSE не запускается");
        };
        assign_to_job(job, pid)
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
fn create_job_object() -> Result<isize> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            bail!("CreateJobObjectW: {}", std::io::Error::last_os_error());
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
            let e = std::io::Error::last_os_error();
            windows_sys::Win32::Foundation::CloseHandle(job);
            bail!("SetInformationJobObject(KILL_ON_JOB_CLOSE): {e}");
        }
        Ok(job as isize)
    }
}

#[cfg(windows)]
fn assign_to_job(job: isize, pid: u32) -> Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
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
        let e = std::io::Error::last_os_error();
        CloseHandle(proc);
        if ok == 0 {
            return Err(anyhow!("AssignProcessToJobObject({pid}): {e}"));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn create_job_object() -> Result<isize> {
    bail!("Windows Job Object поддерживается только на Windows")
}

#[cfg(not(windows))]
fn assign_to_job(_job: isize, _pid: u32) -> Result<()> {
    bail!("Windows Job Object поддерживается только на Windows")
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
        Ok(Err(e)) => {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("дочерний процесс {pid}: {e}"));
        }
        Err(_) => {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("подключение дочернего процесса {pid} зависло"));
        }
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
    let mut child = std::process::Command::new(&exe)
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
        // FAIL-CLOSED: незащищённый процесс не допускается к синтезу
        let _ = child.kill();
        let _ = child.wait();
        return Err(anyhow!(
            "тестовый дочерний процесс {pid}: Job Object не назначен: {e:#}"
        ));
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
    let mut child_cmd = std::process::Command::new(exe);
    child_cmd.arg("--internal-batch-worker").arg(server.name());
    // G6.2.3: явный запрет консольного окна у дочернего процесса —
    // GUI-подсистема не выделяет консоль сама, флаг закрепляет это и для
    // любого другого режима запуска этого exe.
    app_core::hide_console(&mut child_cmd);
    let mut child = child_cmd
        .spawn()
        .map_err(|e| anyhow!("запустить дочерний процесс {}: {e}", exe.display()))?;
    let pid = child.id();
    if let Err(e) = job.assign(pid) {
        // G6.2.1 FAIL-CLOSED: процесс без KILL_ON_JOB_CLOSE не допускается
        // к синтезу; уже созданный СВОЙ процесс сразу завершается.
        let _ = child.kill();
        let _ = child.wait();
        return Err(anyhow!(
            "дочерний процесс {pid}: Job Object не назначен: {e:#}"
        ));
    }
    connect_child(server, child)
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
    /// G6.2.1: внедрение сбоев ПО ПРОЦЕССАМ (пусто/короче — Default для
    /// остальных). В боевом режиме не заполняется.
    pub test: Vec<TestFaults>,
}

/// Итог гибридного прогона: отчёты по ПОЗИЦИЯМ jobs (в исходном порядке
/// плана), число загруженных Pocket и готовых воркеров.
#[derive(Debug)]
pub struct HybridOutcome {
    pub reports: Vec<Option<FileReport>>,
    pub pocket_loads: usize,
    pub workers_ready: usize,
    /// Сколько дочерних процессов реально запустилось и работало.
    pub children_spawned: usize,
    /// G6.2.1: код завершения каждого процесса (None — не запускался).
    pub child_exit_codes: Vec<Option<i32>>,
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

/// G6.2.1: жизненный цикл ДОЧЕРНЕГО ПРОЦЕССА. «PID существует» и «воркер
/// способен выполнять работу» — разные понятия: процесс может быть жив, но
/// после фатального WorkerError больше не примет ни одного файла.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcState {
    /// Процесс создан, но событие Started ещё не пришло.
    Starting,
    /// Started получен, не все воркеры сообщили Ready.
    Initializing,
    /// Все воркеры процесса готовы, свободных назначений нет.
    Ready,
    /// Есть хотя бы один занятый файлом воркер.
    Busy,
    /// Shutdown/Exiting/Stopped: процесс завершает работу.
    Stopping,
    /// Фатальный WorkerError процесса или все его воркеры погибли.
    Failed,
    /// Процесс завершился (PID/канал).
    Exited,
}

/// State machine диспетчера: общая очередь, назначения, отмена, гибель
/// процессов/воркеров, детекция завершения. Не делает IO — только решения.
struct Dispatch {
    queue: VecDeque<usize>,
    slots: Vec<Slot>,
    workers: Vec<WState>,
    wpc: usize,
    /// G6.2.1: состояние каждого процесса (Starting…Exited).
    proc: Vec<ProcState>,
    /// G6.2.1: предел инициализации процесса без готовых воркеров.
    startup_timeout: Duration,
    startup_deadline: Vec<Instant>,
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
            proc: vec![ProcState::Starting; children],
            startup_timeout: CHILD_STARTUP_TIMEOUT,
            startup_deadline: vec![Instant::now() + CHILD_STARTUP_TIMEOUT; children],
            cancelled: false,
            pocket_loads: 0,
            workers_ready: 0,
            logs: Vec::new(),
        }
    }

    /// G6.2.1: переопределить startup timeout (тесты / диагностика).
    fn set_startup_timeout(&mut self, timeout: Duration) {
        self.startup_timeout = timeout;
        self.startup_deadline = vec![Instant::now() + timeout; self.proc.len()];
    }

    fn widx(&self, child: usize, wid: usize) -> usize {
        child * self.wpc + wid
    }

    /// G6.2.1: у процесса есть живой и ещё НЕ готовый воркер, способный
    /// позже прислать Ready. TERMINAL-состояния процесса — не основание ждать.
    /// Важно: процесс с ОДНИМ busy-воркером и другим ещё инициализирующимся
    /// остаётся способным добавить Ready (это не «уже Ready»).
    fn child_can_initialize(&self, child: usize) -> bool {
        if matches!(
            self.proc.get(child),
            Some(ProcState::Failed | ProcState::Exited | ProcState::Stopping) | None
        ) {
            return false;
        }
        self.workers
            .iter()
            .any(|w| w.child == child && w.alive && !w.ready)
    }

    /// Хотя бы один воркер готов брать файлы (ready подразумевает alive).
    fn worker_can_work(&self) -> bool {
        self.workers.iter().any(|w| w.alive && w.ready)
    }

    fn child_has_ready(&self, child: usize) -> bool {
        self.workers
            .iter()
            .any(|w| w.child == child && w.alive && w.ready)
    }

    /// G6.2.1: пересчитать состояние процесса по состояниям его воркеров.
    /// Терминальные Failed/Exited/Stopping не перетираются.
    fn refresh_proc_state(&mut self, child: usize) {
        match self.proc.get(child) {
            Some(ProcState::Failed | ProcState::Exited | ProcState::Stopping) | None => return,
            _ => {}
        }
        let ws: Vec<&WState> = self.workers.iter().filter(|w| w.child == child).collect();
        if ws.iter().all(|w| !w.alive) {
            self.proc[child] = ProcState::Stopping;
        } else if ws.iter().any(|w| w.alive && w.busy.is_some()) {
            self.proc[child] = ProcState::Busy;
        } else if ws.iter().all(|w| !w.alive || w.ready) {
            self.proc[child] = ProcState::Ready;
        } else {
            self.proc[child] = ProcState::Initializing;
        }
    }

    /// G6.2.1: startup timeout истёк у процессов БЕЗ готовых воркеров —
    /// возвращает индексы процессов, которые нужно завершить (только свои).
    fn startup_timeout_expired(&self, now: Instant) -> Vec<usize> {
        (0..self.proc.len())
            .filter(|&ci| {
                self.child_can_initialize(ci)
                    && !self.child_has_ready(ci)
                    && now >= self.startup_deadline[ci]
            })
            .collect()
    }

    /// G6.2.1: инициализация «устоялась» — ни один процесс больше не может
    /// добавить Ready-воркера (все Ready/Failed/Exited/Stopping). Нужно для
    /// честного BatchWorkersReady при частичной инициализации.
    fn init_settled(&self) -> bool {
        !(0..self.proc.len()).any(|ci| self.child_can_initialize(ci))
    }

    /// G6.2.1: нет ни готовых воркеров, ни процессов, способных завершить
    /// инициализацию, а файлы ещё остались → терминальный Failed, пакет
    /// завершается. Пока инициализация допустима — НЕ спешим.
    fn fail_if_no_capacity(&mut self) -> bool {
        if self.all_done() || self.cancelled {
            return false;
        }
        if self.worker_can_work() {
            return false;
        }
        if (0..self.proc.len()).any(|ci| self.child_can_initialize(ci)) {
            return false;
        }
        self.fail_remaining_no_workers();
        true
    }

    /// Процесс терминально мёртв до Ready: Failed + незавершённые файлы Failed.
    fn fail_child(&mut self, child: usize, reason: &str) {
        if let Some(s) = self.proc.get_mut(child) {
            *s = ProcState::Failed;
        }
        self.child_dead(child, reason);
    }

    /// Процесс завершился (PID/канал): Exited + незавершённые файлы Failed.
    fn mark_exited(&mut self, child: usize, reason: &str) {
        if let Some(s) = self.proc.get_mut(child) {
            *s = ProcState::Exited;
        }
        self.child_dead(child, reason);
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
                if self.proc.get(ci) == Some(&ProcState::Starting) {
                    self.proc[ci] = ProcState::Initializing;
                }
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
                let actions: Vec<Action> = self.try_assign(i).into_iter().collect();
                self.refresh_proc_state(ci);
                actions
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
                let actions: Vec<Action> = self.try_assign(i).into_iter().collect();
                self.refresh_proc_state(ci);
                actions
            }
            Ev::Idle { worker } => {
                let i = self.widx(ci, *worker);
                if self.workers[i].busy.is_none() {
                    let actions: Vec<Action> = self.try_assign(i).into_iter().collect();
                    self.refresh_proc_state(ci);
                    return actions;
                }
                Vec::new()
            }
            Ev::WorkerError {
                worker: usize::MAX,
                error,
            } => {
                // фатально для всего процесса: ни один Pocket не поднялся.
                // Терминальное состояние протокола — Failed, а не «живой PID».
                self.log(format!(
                    "ВНИМАНИЕ: процесс {ci}: {error} — его файлы завершатся ошибкой"
                ));
                self.fail_child(ci, &format!("процесс {ci}: {error}"));
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
                // все воркеры процесса погибли — процесс терминально Failed,
                // даже если его PID ещё существует
                if self.workers.iter().all(|w| w.child != ci || !w.alive) {
                    self.fail_child(ci, &format!("процесс {ci}: погибли все воркеры"));
                } else {
                    self.refresh_proc_state(ci);
                }
                Vec::new()
            }
            Ev::Stopped { worker } => {
                let i = self.widx(ci, *worker);
                self.workers[i].alive = false;
                self.workers[i].ready = false;
                if self.workers.iter().all(|w| w.child != ci || !w.alive)
                    && !matches!(
                        self.proc.get(ci),
                        Some(ProcState::Failed | ProcState::Exited)
                    )
                {
                    self.proc[ci] = ProcState::Stopping;
                } else {
                    self.refresh_proc_state(ci);
                }
                Vec::new()
            }
            Ev::Exiting => {
                if !matches!(
                    self.proc.get(ci),
                    Some(ProcState::Failed | ProcState::Exited)
                ) {
                    self.proc[ci] = ProcState::Stopping;
                }
                Vec::new()
            }
        }
    }

    /// Гибель/завершение процесса: его воркеры мертвы, незавершённые
    /// НАЗНАЧЕННЫЕ файлы получают явный Failed (автоповтора нет — состояние
    /// публикации может быть неопределённым).
    fn on_child_dead(&mut self, ci: usize, reason: &str) {
        self.log(format!(
            "ВНИМАНИЕ: дочерний процесс {ci} завершился: {reason}"
        ));
        self.mark_exited(ci, reason);
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
    /// G6.2.1: вызывается ОДИН раз, когда инициализация устоялась, с числом
    /// реально готовых воркеров (может быть меньше заявленного при частичном
    /// отказе загрузки). Нужен для честного BatchWorkersReady.
    pub init_settled: &'a dyn Fn(usize),
    /// Watchdog молчания процесса с назначенным файлом (переопределяется
    /// тестами).
    pub watchdog: Duration,
    /// G6.2.1: предел инициализации процесса без Ready-воркеров
    /// (переопределяется тестами).
    pub startup_timeout: Duration,
    /// G6.2.2: предел ожидания ОДНОЙ IPC-отправки (assign/cancel/shutdown).
    /// Зависшая запись больше не блокирует цикл диспетчера: send_cmd
    /// возвращает ошибку, процесс классифицируется как неработоспособный.
    /// Боевой путь — ipc::WRITE_WAIT_TIMEOUT; тесты уменьшают.
    pub write_timeout: Duration,
}

pub fn run_hybrid(d: &HybridDriver) -> Result<HybridOutcome> {
    let children = d.params.children;
    anyhow::ensure!(children >= 1, "гибрид требует минимум 1 процесс");
    anyhow::ensure!(
        !d.params.jobs.is_empty(),
        "гибрид без файлов не запускается"
    );

    // --- запуск процессов (любой сбой — чистый откат уже запущенных) ---
    // G6.2.1 FAIL-CLOSED: если процесс не запустился/не привязан к Job Object,
    // синтез не начинается, уже созданные СВОИ процессы завершаются.
    let mut procs: Vec<Option<SpawnedChild>> = Vec::with_capacity(children);
    let mut spawn_failure: Option<String> = None;
    for ci in 0..children {
        match (d.spawn)(ci) {
            Ok(mut sp) => {
                // G6.2.2: зависшая отправка не блокирует цикл диспетчера
                sp.writer.set_write_timeout(d.write_timeout);
                let init = Cmd::Init {
                    settings: d.params.settings.clone(),
                    workers: d.params.workers_per_child,
                    fake: d.params.fake,
                    test: d.params.test.get(ci).cloned().unwrap_or_default(),
                };
                if let Err(e) = send_cmd(&sp.writer, &init) {
                    let _ = sp.child.kill();
                    let _ = sp.child.wait();
                    spawn_failure = Some(format!("процесс {ci}: init не доставлен: {e:#}"));
                    break;
                }
                procs.push(Some(sp));
            }
            Err(e) => {
                spawn_failure = Some(format!("процесс {ci}: {e:#}"));
                break;
            }
        }
    }
    if let Some(reason) = spawn_failure {
        // G6.2.1 FAIL-CLOSED: синтез не начинается; завершаем ТОЛЬКО свои
        // уже созданные процессы и выходим с внятной причиной.
        for p in procs.iter_mut().flatten() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
        bail!("гибридный запуск прерван: {reason} — 2×2 не запускается без Job Object");
    }
    if procs.is_empty() {
        bail!("ни один дочерний процесс не запустился");
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
    dispatch.set_startup_timeout(d.startup_timeout);
    // процесс, который не запустился, не может инициализироваться
    for (ci, p) in procs_map.iter().enumerate() {
        if p.is_none() {
            dispatch.mark_exited(ci, "процесс не запущен");
        }
    }
    let mut child_alive: Vec<bool> = (0..children).map(|ci| procs_map[ci].is_some()).collect();
    let mut last_seen: Vec<Instant> = (0..children).map(|_| Instant::now()).collect();
    let mut exit_seen: Vec<Option<Instant>> = vec![None; children];
    let mut exit_codes: Vec<Option<i32>> = vec![None; children];
    let watchdog = d.watchdog;
    let mut cancel_sent = false;
    let mut init_announced = false;

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
            for (ci, p) in procs_map.iter_mut().enumerate() {
                if !child_alive[ci] {
                    continue;
                }
                let Some(p) = p else { continue };
                if let Err(e) = send_cmd(&p.writer, &Cmd::Cancel) {
                    // G6.2.2: отправка отмены зависла/оборвалась — процесс
                    // не отвечает по IPC: завершить СВОЙ PID, файлы получат
                    // Failed; цикл диспетчера продолжает работу
                    (d.log)(format!(
                        "ВНИМАНИЕ: отмена процессу {ci} не доставлена: {e:#} — процесс завершается"
                    ));
                    let _ = p.child.kill();
                    if let Ok(st) = p.child.wait() {
                        exit_codes[ci] = st.code();
                    }
                    child_alive[ci] = false;
                    dispatch.on_child_dead(ci, &format!("отправка отмены не удалась: {e:#}"));
                }
            }
        }
        // G6.2.1: нет готовых/занятых воркеров и не осталось процессов,
        // способных завершить инициализацию, — честный Failed остальным.
        // Пока инициализация допустима, пакет НЕ завершается преждевременно.
        if dispatch.fail_if_no_capacity() {
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
                    if let Some(Some(p)) = procs_map.get_mut(child) {
                        if let Err(e) = send_cmd(&p.writer, &cmd) {
                            // G6.2.2: назначение не доставлено (зависшая
                            // запись по таймауту или разрыв) — файл НЕ
                            // теряется: процесс завершается (только свой
                            // PID), файл получает Failed, очередь доделывают
                            // остальные процессы; диспетчер не блокируется
                            (d.log)(format!(
                                "ВНИМАНИЕ: назначение файла {pos} процессу {child} не доставлено: \
                                 {e:#} — процесс завершается, файл получит Failed"
                            ));
                            let _ = p.child.kill();
                            if let Ok(st) = p.child.wait() {
                                exit_codes[child] = st.code();
                            }
                            child_alive[child] = false;
                            dispatch.on_child_dead(
                                child,
                                &format!("отправка назначения не удалась: {e:#}"),
                            );
                        }
                    }
                }
                if !init_announced && dispatch.init_settled() {
                    init_announced = true;
                    (d.log)(format!(
                        "инициализация устоялась: готовы {} воркеров",
                        dispatch.workers_ready
                    ));
                    (d.init_settled)(dispatch.workers_ready);
                }
                if dispatch.fail_if_no_capacity() {
                    break;
                }
            }
            Ok((ci, ChildEvent::Closed(reason))) => {
                if child_alive[ci] {
                    child_alive[ci] = false;
                    exit_seen[ci] = None;
                    // контроль кода завершения: если процесс уже вышел —
                    // приложить его статус к диагнозу
                    let reason = match procs_map[ci].as_mut() {
                        Some(p) => match p.child.try_wait() {
                            Ok(Some(status)) => {
                                exit_codes[ci] = status.code();
                                format!("{reason}; код завершения процесса {ci}: {status}")
                            }
                            _ => reason,
                        },
                        None => reason,
                    };
                    dispatch.on_child_dead(ci, &reason);
                }
                if !init_announced && dispatch.init_settled() {
                    init_announced = true;
                    (d.log)(format!(
                        "инициализация устоялась: готовы {} воркеров",
                        dispatch.workers_ready
                    ));
                    (d.init_settled)(dispatch.workers_ready);
                }
                if dispatch.fail_if_no_capacity() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                // G6.2.1: завершение PID проверяется и БЕЗ назначенных файлов
                // (авария во время загрузки модели). Событиям канала даётся
                // короткий шанс дойти до диспетчера (Closed/FileFinished).
                for ci in 0..children {
                    if !child_alive[ci] {
                        continue;
                    }
                    let status = procs_map[ci]
                        .as_mut()
                        .and_then(|p| p.child.try_wait().ok().flatten());
                    match status {
                        Some(status) => match exit_seen[ci] {
                            None => exit_seen[ci] = Some(now),
                            Some(t) if now.duration_since(t) >= Duration::from_secs(1) => {
                                exit_seen[ci] = None;
                                exit_codes[ci] = status.code();
                                child_alive[ci] = false;
                                dispatch.on_child_dead(
                                    ci,
                                    &format!("процесс завершился (код {status})"),
                                );
                            }
                            _ => {}
                        },
                        None => exit_seen[ci] = None,
                    }
                }
                // G6.2.1: отдельный startup/initialization timeout. Процесс
                // без ЕДИНОГО готового воркера, молчащий дольше предела,
                // завершается (только свой PID), его файлы — Failed.
                for ci in dispatch.startup_timeout_expired(now) {
                    if !child_alive[ci] {
                        continue;
                    }
                    if let Some(p) = procs_map[ci].as_mut() {
                        let _ = p.child.kill();
                        if let Ok(st) = p.child.wait() {
                            exit_codes[ci] = st.code();
                        }
                    }
                    dispatch.fail_child(
                        ci,
                        &format!(
                            "startup timeout {:?}: ни один воркер не сообщил Ready",
                            dispatch.startup_timeout
                        ),
                    );
                    child_alive[ci] = false;
                }
                // watchdog: живой процесс с назначенным файлом молчит дольше
                // допустимого — kill (только свой процесс) и явный Failed
                for ci in 0..children {
                    if child_alive[ci]
                        && dispatch.child_has_busy(ci)
                        && now.duration_since(last_seen[ci]) > watchdog
                    {
                        if let Some(p) = procs_map[ci].as_mut() {
                            let _ = p.child.kill();
                            if let Ok(st) = p.child.wait() {
                                exit_codes[ci] = st.code();
                            }
                        }
                        dispatch.on_child_dead(
                            ci,
                            &format!("watchdog: нет событий {watchdog:?} при назначенном файле"),
                        );
                        child_alive[ci] = false;
                    }
                }
                if !init_announced && dispatch.init_settled() {
                    init_announced = true;
                    (d.log)(format!(
                        "инициализация устоялась: готовы {} воркеров",
                        dispatch.workers_ready
                    ));
                    (d.init_settled)(dispatch.workers_ready);
                }
                if dispatch.fail_if_no_capacity() {
                    break;
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
    if !init_announced {
        // пакет завершился раньше устойчивой инициализации (отмена/ранний
        // отказ) — сообщаем фактическое число готовых воркеров
        (d.log)(format!(
            "инициализация завершилась раньше: готовы {} воркеров",
            dispatch.workers_ready
        ));
        (d.init_settled)(dispatch.workers_ready);
    }
    dispatch.reconcile();

    // --- контролируемое завершение СВОИХ дочерних процессов ---
    for p in procs_map.iter().flatten() {
        // G6.2.2: отправка ограничена write_timeout; при ошибке остаётся
        // ограниченное ожидание выхода с эскалацией kill ниже
        if let Err(e) = send_cmd(&p.writer, &Cmd::Shutdown) {
            (d.log)(format!(
                "ВНИМАНИЕ: shutdown не доставлен: {e:#} — ожидание выхода с эскалацией"
            ));
        }
    }
    for (ci, p) in procs_map.iter_mut().enumerate() {
        let Some(p) = p else { continue };
        // G6.2.1: ограничение ожидания — НА КАЖДЫЙ процесс, а не общее:
        // один медленный Pocket не съедает бюджет остальных.
        let deadline = Instant::now() + CHILD_EXIT_TIMEOUT;
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
                exit_codes[ci] = st.code();
                if !st.success() {
                    (d.log)(format!(
                        "ВНИМАНИЕ: дочерний процесс {ci} завершился с кодом {st} (не 0)"
                    ));
                }
            }
            None => {
                // эскалация только для собственных дочерних процессов
                (d.log)(format!(
                    "ВНИМАНИЕ: дочерний процесс {ci} не завершился за CHILD_EXIT_TIMEOUT — \
                     принудительное завершение"
                ));
                let _ = p.child.kill();
                if let Ok(st) = p.child.wait() {
                    exit_codes[ci] = st.code();
                }
            }
        }
    }
    for p in procs_map.iter_mut().flatten() {
        if let Some(f) = p.fwd.take() {
            // ограниченное ожидание: forwarder завершается по EOF/ошибке;
            // зависать на нём нельзя (дочерние процессы уже завершены/убиты)
            let mut f = Some(f);
            if !join_bounded(&mut f, CHILD_IO_JOIN_TIMEOUT) {
                (d.log)("ВНИМАНИЕ: forwarder событий не завершился — поток отсоединён".into());
            }
        }
    }

    Ok(HybridOutcome {
        reports: dispatch.slots.into_iter().map(|s| s.report).collect(),
        pocket_loads: dispatch.pocket_loads,
        workers_ready: dispatch.workers_ready,
        children_spawned: procs_map.iter().filter(|p| p.is_some()).count(),
        child_exit_codes: exit_codes,
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
            test: TestFaults::default(),
        };
        let back: Cmd = serde_json::from_value(cmd_to_value(&init).unwrap()).unwrap();
        assert!(matches!(back, Cmd::Init { fake: true, .. }));
    }

    /// G6.3: выбор INT8-модели доходит до дочернего worker-процесса без
    /// потерь: Init с `ModelChoice::FastInt8` проходит wire round-trip
    /// (Named Pipe передаёт тот же JSON), и build_worker_env снимает с него
    /// Int8-точность + каталог ru-v2-fast.
    #[test]
    fn init_carries_int8_model_choice_over_ipc() {
        let settings = GuiSettings {
            model: crate::settings::ModelChoice::FastInt8,
            ..GuiSettings::default()
        };
        let init = Cmd::Init {
            settings: settings.clone(),
            workers: 2,
            fake: false,
            test: TestFaults::default(),
        };
        let v = cmd_to_value(&init).unwrap();
        assert_eq!(v["cmd"], "init");
        assert_eq!(v["settings"]["model"], "FastInt8");
        let back: Cmd = serde_json::from_value(v).unwrap();
        match back {
            Cmd::Init { settings, .. } => {
                assert_eq!(settings.model, crate::settings::ModelChoice::FastInt8);
                assert_eq!(
                    settings.model.precision(),
                    shtorm_engine::ModelPrecision::Int8
                );
            }
            other => panic!("не тот вариант: {other:?}"),
        }
        // build_worker_env сам модели не грузит; точность снимается в
        // pipeline_options (job.rs, pipeline_options_carries_precision) —
        // здесь проверяем резолв каталога для INT8-варианта
        assert_eq!(settings.model.sub_dir(), Some("ru-v2-fast"));
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
            worker_loop(
                0,
                cmd_rx,
                &cancel,
                &ev_tx,
                &s2,
                None,
                &TestFaults::default(),
            );
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
            worker_loop(
                1,
                cmd_rx,
                &cancel,
                &ev_tx,
                &s2,
                None,
                &TestFaults::default(),
            );
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
        // В дочернем режиме КОД ПРОЦЕССА обязан быть кодом worker_main:
        // 0 — штатно, 1 — инжектированный фатальный сбой инициализации.
        // exit() завершает именно child-процесс (тестовый харнесс запускает
        // этот «тест» как единственную точку входа ребёнка).
        std::process::exit(code);
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

    /// Сериализация E2E-тестов, которые запускают РЕАЛЬНЫЕ дочерние процессы
    /// (тестовый харнесс). При параллельном прогоне всего suite одновременно
    /// живут десятки харнесс-детей; под нагрузкой их старт/инициализация
    /// становятся недетерминированными, и строгие lifecycle-ассерты (точное
    /// число Ready, «3 из 4», порядок финальных событий) могли ложно падать.
    /// Мьютекс делает сценарии E2E последовательными, не ослабляя проверок.
    static E2E_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn e2e_lock() -> std::sync::MutexGuard<'static, ()> {
        E2E_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// E2E: полный гибридный прогон 2×2 в fake-режиме на настоящих дочерних
    /// процессах и настоящем Named Pipe IPC — без GPU/моделей. Проверяет:
    /// готовность 4 воркеров, динамическую очередь, публикацию файлов,
    /// контролируемое завершение детей и полноту отчётов.
    #[test]
    fn hybrid_e2e_fake_processes_two_by_two() {
        let _serial = e2e_lock();
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
            test: Vec::new(),
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
            init_settled: &|_n| {},
            watchdog: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
        };
        let outcome = run_hybrid(&driver).expect("гибридный прогон");
        assert_eq!(outcome.reports.len(), 5);
        assert!(outcome.reports.iter().all(|r| r.is_some()));
        for (i, r) in outcome.reports.iter().enumerate() {
            let r = r.as_ref().unwrap();
            assert_eq!(r.status, FileStatus::Completed, "файл {i}: {:?}", r.reason);
        }
        // Глобального барьера готовности нет (назначения начинаются, как
        // только ЛЮБОЙ воркер готов — это production-семантика общей
        // очереди): на маленьком пакете первый процесс может обработать всё
        // до инициализации второго. Проверяем то, что сценарий 2×2 обязан
        // гарантировать: оба процесса реально запущены, работа опубликована,
        // пакет завершён, оба процесса завершены контролируемо (без сирот).
        assert!(
            outcome.workers_ready >= 2,
            "минимум 2 воркера готовы: {}",
            outcome.workers_ready
        );
        assert_eq!(outcome.children_spawned, 2, "оба процесса запущены");
        assert!(
            outcome.child_exit_codes.iter().all(|c| c.is_some()),
            "оба процесса завершены контролируемо: {:?}",
            outcome.child_exit_codes
        );
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
        let _serial = e2e_lock();
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
            test: Vec::new(),
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
            init_settled: &|_n| {},
            watchdog: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
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
        let _serial = e2e_lock();
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
            test: Vec::new(),
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
                init_settled: &|_n| {},
                watchdog: Duration::from_secs(60),
                startup_timeout: Duration::from_secs(60),
                write_timeout: Duration::from_secs(10),
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

    // ================= G6.2.1: E2E жизненного цикла (A–I) =================

    /// Ограниченное ожидание E2E-прогона: при зависании убивает ТОЛЬКО
    /// собственные дочерние PID и возвращает флаг «таймаут». Тест после
    /// возврата run_hybrid обязан проверить флаг и упасть.
    fn watchdog(
        timeout: Duration,
        pids: Arc<Mutex<Vec<u32>>>,
        done: Arc<AtomicBool>,
    ) -> Arc<AtomicBool> {
        let timed_out = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&timed_out);
        std::thread::spawn(move || {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if !done.load(Ordering::SeqCst) {
                flag.store(true, Ordering::SeqCst);
                for pid in pids.lock().unwrap().iter() {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/F"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        });
        timed_out
    }

    fn process_alive(pid: u32) -> bool {
        match std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
        {
            Ok(out) => String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()),
            Err(_) => false,
        }
    }

    /// E2E-A: injected failure `build_worker_env` ПЕРВОГО процесса. Второй
    /// загружает fake-воркеры и обрабатывает все файлы; первый завершается
    /// ненулевым кодом; программа не зависает; все файлы терминальны.
    #[test]
    fn hybrid_e2e_a_init_failure_first_child() {
        let _serial = e2e_lock();
        let d = sandbox("e2eA");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 6);
        let params = HybridParams {
            settings: fake_settings(&out_root),
            jobs: jobs.clone(),
            workers_per_child: 2,
            children: 2,
            fake: true,
            test: vec![
                TestFaults {
                    fail_init: true,
                    ..Default::default()
                },
                TestFaults::default(),
            ],
        };
        let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let timed_out = watchdog(
            Duration::from_secs(120),
            Arc::clone(&pids),
            Arc::clone(&done),
        );
        let pids_c = Arc::clone(&pids);
        let job_guard = ChildJobGuard::new();
        let spawn = |ci: usize| {
            let sp = spawn_test_child(&job_guard, ci)?;
            pids_c.lock().unwrap().push(sp.child.id());
            Ok(sp)
        };
        let log = |_m: String| {};
        let event = |_e: &Ev| {};
        let init_settled = |_n: usize| {};
        let driver = HybridDriver {
            params: &params,
            cancel: Arc::new(AtomicBool::new(false)),
            spawn: &spawn,
            log: &log,
            event: &event,
            init_settled: &init_settled,
            watchdog: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
        };
        let outcome = run_hybrid(&driver).expect("E2E-A");
        done.store(true, Ordering::SeqCst);
        assert!(!timed_out.load(Ordering::SeqCst), "прогон не завис");
        assert_eq!(outcome.reports.len(), 6);
        assert!(outcome.reports.iter().all(|r| r.is_some()));
        assert!(
            outcome
                .reports
                .iter()
                .all(|r| r.as_ref().unwrap().status == FileStatus::Completed),
            "второй процесс обработал все файлы"
        );
        assert_eq!(
            outcome.workers_ready, 2,
            "готовы воркеры выжившего процесса"
        );
        assert_ne!(
            outcome.child_exit_codes[0],
            Some(0),
            "первый процесс завершился ненулевым кодом"
        );
        assert_eq!(outcome.child_exit_codes[1], Some(0));
    }

    /// E2E-B: оба процесса не смогли инициализироваться. Pending-файлы
    /// получают Failed; диспетчер выходит за ограниченное время; оба PID
    /// завершены; живых дочерних процессов не остаётся.
    #[test]
    fn hybrid_e2e_b_both_init_fail() {
        let _serial = e2e_lock();
        let d = sandbox("e2eB");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 4);
        let params = HybridParams {
            settings: fake_settings(&out_root),
            jobs: jobs.clone(),
            workers_per_child: 2,
            children: 2,
            fake: true,
            test: vec![
                TestFaults {
                    fail_init: true,
                    ..Default::default()
                },
                TestFaults {
                    fail_init: true,
                    ..Default::default()
                },
            ],
        };
        let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let timed_out = watchdog(
            Duration::from_secs(60),
            Arc::clone(&pids),
            Arc::clone(&done),
        );
        let pids_c = Arc::clone(&pids);
        let job_guard = ChildJobGuard::new();
        let spawn = |ci: usize| {
            let sp = spawn_test_child(&job_guard, ci)?;
            pids_c.lock().unwrap().push(sp.child.id());
            Ok(sp)
        };
        let log = |_m: String| {};
        let event = |_e: &Ev| {};
        let init_settled = |_n: usize| {};
        let driver = HybridDriver {
            params: &params,
            cancel: Arc::new(AtomicBool::new(false)),
            spawn: &spawn,
            log: &log,
            event: &event,
            init_settled: &init_settled,
            watchdog: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
        };
        let outcome = run_hybrid(&driver).expect("E2E-B");
        done.store(true, Ordering::SeqCst);
        assert!(
            !timed_out.load(Ordering::SeqCst),
            "диспетчер вышел ограниченно"
        );
        assert_eq!(outcome.reports.len(), 4);
        assert!(
            outcome
                .reports
                .iter()
                .all(|r| r.as_ref().unwrap().status == FileStatus::Failed),
            "все оставшиеся файлы — Failed"
        );
        assert_eq!(outcome.workers_ready, 0);
        assert!(
            outcome
                .child_exit_codes
                .iter()
                .all(|c| c.is_some() && *c != Some(0)),
            "оба процесса завершились ненулевым кодом: {:?}",
            outcome.child_exit_codes
        );
        let alive: Vec<u32> = pids
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|pid| process_alive(*pid))
            .collect();
        assert!(alive.is_empty(), "нет живых дочерних PID: {alive:?}");
    }

    /// E2E-C: процесс убит ДО Ready. IPC-разрыв обнаружен, выживший процесс
    /// продолжает работу, ожидающие задания не теряются, бесконечного
    /// ожидания Ready нет.
    #[test]
    fn hybrid_e2e_c_process_died_before_ready() {
        let _serial = e2e_lock();
        let d = sandbox("e2eC");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 6);
        let params = HybridParams {
            settings: fake_settings(&out_root),
            jobs: jobs.clone(),
            workers_per_child: 2,
            children: 2,
            fake: true,
            test: vec![
                TestFaults {
                    init_delay_ms: 5_000,
                    ..Default::default()
                },
                TestFaults::default(),
            ],
        };
        let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let pids_k = Arc::clone(&pids);
        let kill_done = Arc::new(AtomicBool::new(false));
        let kill_c = Arc::clone(&kill_done);
        let event = move |ev: &Ev| {
            if matches!(ev, Ev::Started { .. }) && !kill_c.swap(true, Ordering::SeqCst) {
                if let Some(&victim) = pids_k.lock().unwrap().first() {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &victim.to_string(), "/F"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        };
        let done = Arc::new(AtomicBool::new(false));
        let timed_out = watchdog(
            Duration::from_secs(120),
            Arc::clone(&pids),
            Arc::clone(&done),
        );
        let pids_c = Arc::clone(&pids);
        let job_guard = ChildJobGuard::new();
        let spawn = |ci: usize| {
            let sp = spawn_test_child(&job_guard, ci)?;
            pids_c.lock().unwrap().push(sp.child.id());
            Ok(sp)
        };
        let log = |_m: String| {};
        let init_settled = |_n: usize| {};
        let driver = HybridDriver {
            params: &params,
            cancel: Arc::new(AtomicBool::new(false)),
            spawn: &spawn,
            log: &log,
            event: &event,
            init_settled: &init_settled,
            watchdog: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
        };
        let outcome = run_hybrid(&driver).expect("E2E-C");
        done.store(true, Ordering::SeqCst);
        assert!(
            !timed_out.load(Ordering::SeqCst),
            "ожидание Ready не бесконечно"
        );
        assert!(
            kill_done.load(Ordering::SeqCst),
            "сбой внедрён во время init"
        );
        assert_eq!(outcome.reports.len(), 6);
        assert!(
            outcome
                .reports
                .iter()
                .all(|r| r.as_ref().unwrap().status == FileStatus::Completed),
            "выживший процесс доделал все файлы"
        );
        assert_eq!(outcome.workers_ready, 2);
        assert_ne!(outcome.child_exit_codes[0], Some(0));
    }

    /// E2E-D: Ready одного воркера, второй не загрузился. Частично
    /// инициализированный процесс не считается полностью готовым (3 из 4).
    #[test]
    fn hybrid_e2e_d_partial_init() {
        let _serial = e2e_lock();
        let d = sandbox("e2eD");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 6);
        let params = HybridParams {
            settings: fake_settings(&out_root),
            jobs: jobs.clone(),
            workers_per_child: 2,
            children: 2,
            fake: true,
            test: vec![
                TestFaults {
                    fail_workers: vec![1],
                    ..Default::default()
                },
                TestFaults::default(),
            ],
        };
        let settled: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
        let settled_c = Arc::clone(&settled);
        let init_settled = move |n: usize| {
            *settled_c.lock().unwrap() = Some(n);
        };
        let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let timed_out = watchdog(
            Duration::from_secs(120),
            Arc::clone(&pids),
            Arc::clone(&done),
        );
        let pids_c = Arc::clone(&pids);
        let job_guard = ChildJobGuard::new();
        let spawn = |ci: usize| {
            let sp = spawn_test_child(&job_guard, ci)?;
            pids_c.lock().unwrap().push(sp.child.id());
            Ok(sp)
        };
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let logs_c = Arc::clone(&logs);
        let log = move |m: String| logs_c.lock().unwrap().push(m);
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let events_c = Arc::clone(&events);
        let event = move |ev: &Ev| events_c.lock().unwrap().push(format!("{ev:?}"));
        let driver = HybridDriver {
            params: &params,
            cancel: Arc::new(AtomicBool::new(false)),
            spawn: &spawn,
            log: &log,
            event: &event,
            init_settled: &init_settled,
            watchdog: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
        };
        let outcome = run_hybrid(&driver).expect("E2E-D");
        done.store(true, Ordering::SeqCst);
        if *settled.lock().unwrap() != Some(3) {
            eprintln!("E2E-D logs:\n{}", logs.lock().unwrap().join("\n"));
            eprintln!("E2E-D events:\n{}", events.lock().unwrap().join("\n"));
        }
        assert!(!timed_out.load(Ordering::SeqCst));
        assert_eq!(outcome.workers_ready, 3, "3 из 4 воркеров реально готовы");
        assert_eq!(
            *settled.lock().unwrap(),
            Some(3),
            "BatchWorkersReady с фактическим числом"
        );
        assert!(
            outcome
                .reports
                .iter()
                .all(|r| r.as_ref().unwrap().status == FileStatus::Completed),
            "частичный процесс и второй процесс доделали файлы"
        );
    }

    /// E2E-E: порядок финальных сообщений FileFinished → Stopped → Exiting →
    /// EOF; без потерь и дубликатов, Exiting — последнее сообщение.
    #[test]
    fn hybrid_e2e_e_final_event_order() {
        let _serial = e2e_lock();
        let d = sandbox("e2eE");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 1);
        let job_guard = ChildJobGuard::new();
        let mut sp = spawn_test_child(&job_guard, 0).expect("spawn E");
        let init = Cmd::Init {
            settings: fake_settings(&out_root),
            workers: 1,
            fake: true,
            test: TestFaults::default(),
        };
        send_cmd(&sp.writer, &init).expect("init");
        let assign = Cmd::Assign {
            worker: 0,
            pos: 0,
            input: jobs[0].input.clone(),
            relative: jobs[0].relative.clone(),
            out: jobs[0].out.clone(),
            seed: 42,
        };
        send_cmd(&sp.writer, &assign).expect("assign");
        send_cmd(&sp.writer, &Cmd::Shutdown).expect("shutdown");

        let mut evs: Vec<Ev> = Vec::new();
        let mut closed = false;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match sp.events.recv_timeout(Duration::from_millis(100)) {
                Ok(ChildEvent::Msg(ev)) => evs.push(ev),
                Ok(ChildEvent::Closed(_)) => {
                    closed = true;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = sp.child.wait();
        assert!(closed, "EOF после Exiting");
        let fi = evs
            .iter()
            .position(|e| matches!(e, Ev::FileFinished { .. }))
            .expect("FileFinished");
        let st = evs
            .iter()
            .position(|e| matches!(e, Ev::Stopped { .. }))
            .expect("Stopped");
        let ex = evs
            .iter()
            .position(|e| matches!(e, Ev::Exiting))
            .expect("Exiting");
        assert!(fi < st && st < ex, "порядок нарушен: {evs:?}");
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, Ev::FileFinished { .. }))
                .count(),
            1,
            "ровно одно FileFinished"
        );
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, Ev::Stopped { .. }))
                .count(),
            1,
            "ровно одно Stopped"
        );
        assert!(matches!(evs.last(), Some(Ev::Exiting)), "Exiting последнее");
    }

    /// E2E-F: родитель разрывает IPC, пока ребёнок завершается. Нет deadlock:
    /// ребёнок выходит за ограниченное время, writer не остаётся заблокирован.
    #[test]
    fn hybrid_e2e_f_ipc_break_during_finish() {
        let _serial = e2e_lock();
        let d = sandbox("e2eF");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let job_guard = ChildJobGuard::new();
        let mut sp = spawn_test_child(&job_guard, 0).expect("spawn F");
        let init = Cmd::Init {
            settings: fake_settings(&out_root),
            workers: 1,
            fake: true,
            test: TestFaults::default(),
        };
        send_cmd(&sp.writer, &init).expect("init");
        std::thread::sleep(Duration::from_millis(200));
        sp.writer.disconnect().expect("разрыв канала");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut exited = false;
        while Instant::now() < deadline {
            match sp.child.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        if !exited {
            let _ = sp.child.kill();
            let _ = sp.child.wait();
        }
        assert!(
            exited,
            "дочерний процесс завершился за ограниченное время после разрыва IPC"
        );
    }

    /// E2E-G: отказ создания Job Object — fail-closed. 2×2 не запускается,
    /// ошибка понятна; никаких процессов не создаётся.
    #[test]
    fn hybrid_e2e_g_job_creation_failure() {
        let _serial = e2e_lock();
        let guard = ChildJobGuard::with_faults_for_test(true, false);
        let err = guard.ready().expect_err("Job Object не создан");
        assert!(format!("{err:#}").contains("Job Object"));

        let d = sandbox("e2eG");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 2);
        let params = HybridParams {
            settings: fake_settings(&out_root),
            jobs: jobs.clone(),
            workers_per_child: 2,
            children: 2,
            fake: true,
            test: Vec::new(),
        };
        let spawn = |_ci: usize| -> Result<SpawnedChild> {
            anyhow::bail!("Job Object недоступен — 2×2 запрещён")
        };
        let log = |_m: String| {};
        let event = |_e: &Ev| {};
        let init_settled = |_n: usize| {};
        let driver = HybridDriver {
            params: &params,
            cancel: Arc::new(AtomicBool::new(false)),
            spawn: &spawn,
            log: &log,
            event: &event,
            init_settled: &init_settled,
            watchdog: Duration::from_secs(10),
            startup_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
        };
        let err = run_hybrid(&driver).expect_err("2×2 не должен стартовать");
        assert!(format!("{err:#}").contains("Job Object"));
    }

    /// E2E-H: отказ AssignProcessToJobObject — процесс не допускается к
    /// синтезу и сразу завершается; ошибка не скрывается.
    #[test]
    fn hybrid_e2e_h_assign_failure() {
        let _serial = e2e_lock();
        let guard = ChildJobGuard::with_faults_for_test(false, true);
        match spawn_test_child(&guard, 0) {
            Ok(_) => panic!("AssignProcessToJobObject обязан отказать"),
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("Job Object") || msg.contains("AssignProcessToJobObject"),
                    "{msg}"
                );
            }
        }
    }

    /// E2E-I: аварийная гибель изолированного тестового диспетчера — Job
    /// Object обязан завершить собственные дочерние fake-worker PID.
    #[test]
    fn hybrid_e2e_i_dispatcher_crash() {
        let _serial = e2e_lock();
        let d = sandbox("e2eI");
        let pid_file = d.join("pids.txt");
        let exe = std::env::current_exe().unwrap();
        let mut dispatcher = std::process::Command::new(&exe)
            .args([
                "--exact",
                "hybrid::tests::hybrid_e2e_dispatcher_entry",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("SHTORM_G62_DISPATCHER_PID_FILE", &pid_file)
            .spawn()
            .expect("spawn dispatcher");
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut pids: Vec<u32> = Vec::new();
        while Instant::now() < deadline {
            if let Ok(s) = std::fs::read_to_string(&pid_file) {
                let parsed: Vec<u32> = s.lines().filter_map(|l| l.trim().parse().ok()).collect();
                if parsed.len() >= 2 {
                    pids = parsed;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(pids.len(), 2, "диспетчер сообщил PID двух детей");
        // аварийно завершаем ТОЛЬКО собственный тестовый диспетчер
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &dispatcher.id().to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = dispatcher.wait();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut alive: Vec<u32> = Vec::new();
        while Instant::now() < deadline {
            alive = pids
                .iter()
                .copied()
                .filter(|pid| process_alive(*pid))
                .collect();
            if alive.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !alive.is_empty() {
            // страховка теста: свои процессы, не чужие
            for pid in &alive {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        assert!(
            alive.is_empty(),
            "KILL_ON_JOB_CLOSE завершил дочерние PID: {alive:?}"
        );
    }

    /// Child-entry E2E-I: изолированный диспетчер. Создаёт Job Object,
    /// запускает двух fake-воркеров, публикует их PID и ждёт гибели от
    /// родителя. Запускается ТОЛЬКО при заданном PID-файле.
    #[test]
    fn hybrid_e2e_dispatcher_entry() {
        let pid_file = match std::env::var("SHTORM_G62_DISPATCHER_PID_FILE") {
            Ok(p) => p,
            Err(_) => return,
        };
        let guard = ChildJobGuard::new();
        let mut kids = Vec::new();
        for ci in 0..2 {
            let sp = spawn_child_via_test_harness(&guard, ci).expect("spawn child");
            let init = Cmd::Init {
                settings: GuiSettings::default(),
                workers: 1,
                fake: true,
                test: TestFaults::default(),
            };
            let _ = send_cmd(&sp.writer, &init);
            kids.push(sp);
        }
        let pids: Vec<String> = kids.iter().map(|k| k.child.id().to_string()).collect();
        std::fs::write(&pid_file, pids.join("\n")).expect("pid file");
        std::thread::sleep(Duration::from_secs(120));
        for mut k in kids {
            let _ = k.child.kill();
            let _ = k.child.wait();
        }
    }

    // ================= G6.2.2: E2E-M (зависшая отправка диспетчера) =================

    /// Точка входа зависшего ребёнка E2E-M1: подключается к каналу и НЕ
    /// читает ничего (записи диспетчера перевисают в буфере малого размера).
    /// Живёт ограниченное время, чтобы не пережить неудавшийся тест.
    #[test]
    fn hybrid_e2e_m1_stuck_child_entry() {
        let Ok(pipe) = std::env::var("SHTORM_G62_M1_PIPE") else {
            return; // обычный прогон тестов — не child-режим
        };
        let end = match ipc::connect(&pipe, Duration::from_secs(10)) {
            Ok(e) => e,
            Err(_) => std::process::exit(1),
        };
        drop(end); // НЕ читаем: команды диспетчера не потребляются
        std::thread::sleep(Duration::from_secs(90));
        std::process::exit(0);
    }

    /// E2E-M1: дочерний процесс подключился, но перестал читать IPC-команды
    /// (малый буфер канала — init-запись РЕАЛЬНО перевисает, а не проходит в
    /// буфер). Диспетчер обязан: вернуть управление за write_timeout (а не
    /// ждать вечно в send_cmd), завершить СВОЙ зависший PID, не начать
    /// синтез и не зависнуть сам.
    #[test]
    fn hybrid_e2e_m1_hung_child_bounded_init() {
        let _serial = e2e_lock();
        let d = sandbox("e2eM1");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 2);
        // раздуваем init-кадр: text_file не используется worker-процессом
        // для путей — безопасное поле для увеличения размера Init
        let mut settings = fake_settings(&out_root);
        settings.text_file = "m".repeat(3072);
        let params = HybridParams {
            settings,
            jobs,
            workers_per_child: 2,
            children: 2,
            fake: true,
            test: Vec::new(),
        };
        let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let timed_out = watchdog(
            Duration::from_secs(60),
            Arc::clone(&pids),
            Arc::clone(&done),
        );
        let pids_c = Arc::clone(&pids);
        let job_guard = ChildJobGuard::new();
        let spawn = |ci: usize| -> Result<SpawnedChild> {
            if ci == 0 {
                // ЗАВИСАЮЩИЙ ребёнок: буфер канала 512 Б << init-кадра
                let server = PipeServer::create_unique_sized("e2em1", 512)?;
                let exe = std::env::current_exe()?;
                let child = std::process::Command::new(&exe)
                    .args([
                        "--exact",
                        "hybrid::tests::hybrid_e2e_m1_stuck_child_entry",
                        "--test-threads=1",
                        "--nocapture",
                    ])
                    .env("SHTORM_G62_M1_PIPE", server.name())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| anyhow!("spawn stuck child: {e}"))?;
                let pid = child.id();
                job_guard.assign(pid)?;
                pids_c.lock().unwrap().push(pid);
                connect_child(server, child)
            } else {
                let sp = spawn_test_child(&job_guard, ci)?;
                pids_c.lock().unwrap().push(sp.child.id());
                Ok(sp)
            }
        };
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let logs_c = Arc::clone(&logs);
        let log = move |m: String| logs_c.lock().unwrap().push(m);
        let event = |_e: &Ev| {};
        let init_settled = |_n: usize| {};
        let driver = HybridDriver {
            params: &params,
            cancel: Arc::new(AtomicBool::new(false)),
            spawn: &spawn,
            log: &log,
            event: &event,
            init_settled: &init_settled,
            watchdog: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_millis(300),
        };
        let t0 = Instant::now();
        let res = run_hybrid(&driver);
        done.store(true, Ordering::SeqCst);
        assert!(
            !timed_out.load(Ordering::SeqCst),
            "run_hybrid не завис: {}",
            logs.lock().unwrap().join("\n")
        );
        let err = res.expect_err("зависшая init-отправка обязана отказать ограниченно");
        assert!(
            format!("{err:#}").contains("init не доставлен"),
            "причина: {err:#}"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "ограниченное завершение за {:?}",
            t0.elapsed()
        );
        // зависший ребёнок завершён диспетчером (своим PID)
        let victim = pids.lock().unwrap()[0];
        assert!(
            !process_alive(victim),
            "зависший дочерний PID {victim} завершён диспетчером"
        );
    }

    /// E2E-M2: сбой отправки назначения (канал разорван за спиной диспетчера
    /// после готовности всех воркеров). Файл не теряется: процесс
    /// классифицируется неработоспособным и завершается (только свой PID),
    /// его незавершённые файлы получают Failed, выживший процесс доделывает
    /// пакет, GUI-поток возвращается за ограниченное время.
    #[test]
    fn hybrid_e2e_m2_assign_send_failure_recovers() {
        let _serial = e2e_lock();
        let d = sandbox("e2eM2");
        let out_root = d.join("out");
        std::fs::create_dir_all(&out_root).unwrap();
        let jobs = make_jobs(&d, &out_root, 6);
        let params = HybridParams {
            settings: fake_settings(&out_root),
            jobs: jobs.clone(),
            workers_per_child: 2,
            children: 2,
            fake: true,
            test: Vec::new(),
        };
        let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let timed_out = watchdog(
            Duration::from_secs(90),
            Arc::clone(&pids),
            Arc::clone(&done),
        );
        // writer процесса 0 stash'ится спавнером; канал разрывается за спиной
        // диспетчера, когда все 4 воркера готовы И уже есть хотя бы один
        // FileFinished. Второе условие убирает гонку порядка событий: после
        // 4-го Ready диспетчер раздал файлы всем воркерам, а FileFinished
        // доказывает, что исполнение началось — значит у процесса 0 есть
        // файл В РАБОТЕ, и его разрыв даёт детерминированный Failed (без
        // него при быстром выжившем все 6 файлов могли дойти до Completed,
        // и сценарий «сбой отправки» не воспроизводился под нагрузкой).
        let stash: Arc<Mutex<Option<Arc<PipeEnd>>>> = Arc::new(Mutex::new(None));
        let ready_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let finished_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let disconnected = Arc::new(AtomicBool::new(false));
        let pids_c = Arc::clone(&pids);
        let stash_c = Arc::clone(&stash);
        let job_guard = ChildJobGuard::new();
        let spawn = move |ci: usize| -> Result<SpawnedChild> {
            let sp = spawn_test_child(&job_guard, ci)?;
            pids_c.lock().unwrap().push(sp.child.id());
            if ci == 0 {
                *stash_c.lock().unwrap() = Some(Arc::clone(&sp.writer));
            }
            Ok(sp)
        };
        let stash_e = Arc::clone(&stash);
        let ready_c = Arc::clone(&ready_seen);
        let finished_c = Arc::clone(&finished_seen);
        let disc_c = Arc::clone(&disconnected);
        let event = move |ev: &Ev| {
            if matches!(ev, Ev::Ready { .. }) {
                ready_c.fetch_add(1, Ordering::SeqCst);
            }
            if matches!(ev, Ev::FileFinished { .. }) {
                finished_c.fetch_add(1, Ordering::SeqCst);
            }
            if ready_c.load(Ordering::SeqCst) >= 4
                && finished_c.load(Ordering::SeqCst) >= 1
                && !disc_c.swap(true, Ordering::SeqCst)
            {
                if let Some(w) = stash_e.lock().unwrap().as_ref() {
                    let _ = w.disconnect();
                }
            }
        };
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let logs_c = Arc::clone(&logs);
        let log = move |m: String| logs_c.lock().unwrap().push(m);
        let init_settled = |_n: usize| {};
        let driver = HybridDriver {
            params: &params,
            cancel: Arc::new(AtomicBool::new(false)),
            spawn: &spawn,
            log: &log,
            event: &event,
            init_settled: &init_settled,
            watchdog: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_millis(300),
        };
        let t0 = Instant::now();
        let outcome = run_hybrid(&driver).expect("M2: диспетчер восстанавливается");
        done.store(true, Ordering::SeqCst);
        assert!(
            !timed_out.load(Ordering::SeqCst),
            "прогон не завис: {}",
            logs.lock().unwrap().join("\n")
        );
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "ограниченное восстановление за {:?}",
            t0.elapsed()
        );
        assert_eq!(outcome.reports.len(), 6);
        assert!(
            outcome.reports.iter().all(|r| r.is_some()),
            "все файлы терминальны"
        );
        let completed = outcome
            .reports
            .iter()
            .filter(|r| r.as_ref().unwrap().status == FileStatus::Completed)
            .count();
        let failed = outcome
            .reports
            .iter()
            .filter(|r| r.as_ref().unwrap().status == FileStatus::Failed)
            .count();
        assert_eq!(completed + failed, 6);
        assert!(completed >= 1, "выживший процесс доделал свою часть");
        // Разрыв канала мог «опоздать»: если к моменту разрыва у процесса 0
        // не осталось назначенных файлов, он вообще не получает новых
        // назначений (сломанный writer отвергает отправку — фиксируется в
        // логах), а все файлы доделывает выживший. Это валидный исход
        // сценария: файлы не теряются (completed + failed == 6 выше) и
        // диспетчер восстанавливается. Детерминированное покрытие «назначение
        // сломанному процессу → файл Failed» даёт unit-тест
        // dispatch_child_death_fails_its_files_survivor_finishes.
        if failed == 0 {
            assert_eq!(completed, 6, "без Failed все файлы обязаны быть готовы");
        }
        for r in outcome.reports.iter().flatten() {
            if r.status == FileStatus::Failed {
                let reason = r.reason.as_deref().unwrap_or_default();
                assert!(
                    reason.contains("процесс")
                        || reason.contains("воркер")
                        || reason.contains("отправк"),
                    "диагноз сбоя отправки: {reason}"
                );
            }
        }
        // живых дочерних PID не осталось
        let alive: Vec<u32> = pids
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|pid| process_alive(*pid))
            .collect();
        assert!(alive.is_empty(), "осиротевших процессов нет: {alive:?}");
    }
}
