//! G1/G2: форма GUI (egui) с рабочей валидацией и запуском синтеза
//! (G2 — фоновый поток поверх app_core::pipeline::synthesize_stream).
//!
//! G6.2.3: автосохранение настроек при штатном закрытии окна — явная машина
//! состояний [`CloseFlow`] (закрытие при живом задании по-прежнему ждёт
//! отмены и завершения потока, публикация дорабатывает).
//!
//! G6.8: egui обновлён 0.31 → 0.34 (требование egui_software_backend 0.0.3).
//! Кадр UI выделен в [`ShtormApp::update_ui`] и используется ОБОИМИ
//! рантаймами: eframe/glow (EXE PocketTTS-GUI-Rust) и программным
//! sw-рантаймом (EXE PocketTTS-GUI-Rust-cpu, без OpenGL).

use egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::batch::FileStatus;
use crate::job::{JobEvent, JobPhase, JobStatus};
use crate::settings::{
    load_settings, save_settings, settings_path, DeviceChoice, GuiSettings, ModelChoice,
    OutputFormatChoice, LEGACY_SETTINGS_FILE,
};

/// Клик по заголовку свёртки переворачивает ПОЛЕ настроек: состояние хранится
/// в настройках (единственный источник истины, заголовок открывается через
/// `.open(Some(field))`). Иначе фактическое состояние живёт только в
/// egui-памяти — и в конфиг сохранялись бы стартовые значения (дефект
/// G1-фикс №3).
pub(crate) fn header_clicked<R>(resp: &egui::CollapsingResponse<R>, open: &mut bool) {
    if resp.header_response.clicked() {
        *open = !*open;
    }
}

/// Подпись значения хвоста EOS в ComboBox: None — штатная политика движка.
fn eos_tail_label(t: shtorm_engine::EosTail) -> String {
    match t {
        shtorm_engine::EosTail::Default => "None".into(),
        shtorm_engine::EosTail::Fixed(n) => n.to_string(),
    }
}

/// Точная семантика хвоста EOS — по фактической проверке счётчика движка
/// (тест eos_gate_counts_frames_including_detection_frame): кадр
/// срабатывания EOS входит в счёт.
const EOS_TAIL_TOOLTIP: &str = "N передаётся движку напрямую, без пересчёта. \
    Фактическая семантика счётчика teratts-android (проверена тестом): \
    при N генерируется N кадров с момента срабатывания EOS ВКЛЮЧИТЕЛЬНО — \
    то есть N−1 кадр после кадра срабатывания; при N=0 не генерируется \
    ни одного (кадр срабатывания отбрасывается). None — штатная политика \
    движка: 5 для чанков ≤4 слов и 3 для остальных.";

/// G6.1: активный файл пакета в модели UI. События нескольких параллельных
/// файлов приходят вперемешку — состояние каждого tracked по индексу K.
#[derive(Debug, Clone)]
struct ActiveFileUi {
    /// K файла (1-based, соответствует JobEvent::File*).
    index: usize,
    /// Имя входного файла (короткое, для строки прогресса).
    name: String,
    done: usize,
    total: usize,
    phase: JobPhase,
}

/// G6.1: состояние пакетного задания. Итоговое состояние пакета определяется
/// РЕАЛЬНЫМИ результатами всех файлов (счётчики), а не последним событием.
#[derive(Debug, Default)]
struct BatchUiState {
    total: usize,
    /// Файлов обрабатывается одновременно (постоянных воркеров пула).
    /// G6.1.1: 0 — воркеры ещё не готовы (preflight/загрузка пула).
    workers: usize,
    /// Завершено файлов (любым статусом) — K из N.
    finished: usize,
    completed: usize,
    failed: usize,
    skipped: usize,
    cancelled: usize,
    /// Индексы файлов (K, 1-based), для которых FileDone уже учтён:
    /// защита от двойного счёта одного файла (G6.1.1, Test E).
    done_indices: std::collections::HashSet<usize>,
    /// Активные файлы (не более `workers`).
    active: Vec<ActiveFileUi>,
}

/// G6.2.3: результат чтения собственного конфига при старте — определяет
/// политику автосохранения при закрытии окна.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsLoadState {
    /// Конфиг прочитан — автосохранение при закрытии разрешено.
    Loaded,
    /// Конфига нет (первый запуск) — автосохранение разрешено: создаёт
    /// файл, перезаписывать нечего.
    FirstRun,
    /// Конфиг существует, но не читается/не разбирается (повреждён, BOM,
    /// занят) — автосохранение ЗАПРЕЩЕНО: закрытие окна не должно незаметно
    /// затирать непрочитанный пользовательский файл настройками по
    /// умолчанию. Причина сообщается в статусе при старте; ручное
    /// сохранение кнопкой остаётся осознанным решением пользователя.
    Unreadable,
}

/// G6.2.3: явные состояния закрытия окна (замена одиночного `close_pending`).
/// Каждое состояние однозначно: повторная инициализация закрытия и цикл
/// `Close → CancelClose → Save → Close → …` исключены — в [`CloseFlow::Closing`]
/// на `close_requested` (индуцированный собственным `ViewportCommand::Close`)
/// НЕ отвечаем CancelClose, процесс завершается.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseFlow {
    /// Обычная работа — закрытие не запрашивалось.
    Idle,
    /// Крестик при живом задании: отмена отправлена, окно удерживается
    /// (CancelClose) до завершения фонового потока — семантика G2/G5.1
    /// (публикация дорабатывает, сиротских процессов нет).
    WaitingForJob,
    /// Сохранить настройки и закрыться. Переходное состояние: попытка
    /// выполняется в кадре перехода (задание уже завершено или его не было).
    Saving,
    /// Автосохранение не удалось: окно удерживается, пользователь явно
    /// выбирает «Повторить» или «Выйти без сохранения» (существующий файл
    /// не разрушается — атомарная замена в save_settings).
    SaveFailed,
    /// Решение принято (сохранено / пропущено по политике / отказ
    /// пользователя) — фактическое закрытие: `ViewportCommand::Close` уже
    /// отправлен, повторно не отправляется.
    Closing,
}

pub struct ShtormApp {
    pub settings: GuiSettings,
    log: String,
    status: String,
    last_saved: Option<PathBuf>,
    /// Путь собственного конфига (продакшн — `settings_path()` рядом с exe;
    /// отдельное поле — инъекция изолированного пути в тестах).
    conf_path: PathBuf,
    /// G6.2.3: политика автосохранения по результату чтения конфига.
    settings_load: SettingsLoadState,
    /// G6.2.3: снимок сериализованных настроек на момент старта — при
    /// закрытии файл пишется только при реальных изменениях (сессия без
    /// правок не создаёт временных файлов).
    settings_snapshot: String,
    /// G6.2.3: состояние закрытия окна (автосохранение при закрытии).
    close_flow: CloseFlow,
    /// `ViewportCommand::Close` уже отправлен (ровно один раз).
    close_sent: bool,
    /// Текст последней ошибки автосохранения (диалог при закрытии).
    autosave_error: Option<String>,
    /// Активное фоновое задание (None — можно запустить новое; два задания
    /// одновременно запрещены).
    job: Option<JobHandle>,
    /// Прогресс (чанков готово / всего) последнего активного задания.
    progress: (usize, usize),
    /// Пакетный режим: номер текущего файла / всего файлов.
    file_progress: (usize, usize),
    /// Текущее задание — пакетное (для отображения прогресса по файлам).
    batch_running: bool,
    /// G6.1: состояние пакетного задания (per-file модель параллельного пула).
    batch_state: Option<BatchUiState>,
    /// G5.1: задание перешло фазу Finalizing/Publishing — публикация уже
    /// необратима, «Стоп» отключается (он не может предотвратить замену).
    /// В пакетном режиме семантика per-file (см. stop_available).
    publishing: bool,
    /// Кэш проверки ffmpeg (resolve_ffmpeg запускает процесс — не каждый кадр):
    /// ключ = явный путь из настроек, значение = текст ошибки, если не найден.
    ffmpeg_check_key: Option<String>,
    ffmpeg_check_err: Option<String>,
    ffmpeg_checked_at: std::time::Instant,
    /// G5: последняя применённая тема (переключатель без лишних set_visuals).
    theme_applied: bool,
    /// G6.4: типографика и отступы применены (один раз за процесс).
    style_applied: bool,
    /// G6.4: полный текст статуса загрузки настроек (tooltip верхней панели;
    /// сама панель показывает короткую метку).
    load_note: String,
    /// G6.4: высота прокручиваемой области журнала в логических точках
    /// (регулируется перетаскиванием верхней кромки, в конфиг не пишется).
    log_height: f32,
    /// G6.4: журнал развёрнут пользователем в этой сессии.
    log_open: bool,
    /// G6.4: последняя наблюдённая конфигурация монитора (размер в точках,
    /// DPI×100) — однократная подстройка при смене монитора/разрешения.
    monitor_key: Option<(i32, i32, i32)>,
}

/// Живое задание: флаг отмены, канал событий из потока, поток.
struct JobHandle {
    cancel: Arc<AtomicBool>,
    rx: std::sync::mpsc::Receiver<JobEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for JobHandle {
    /// Выход из приложения с живым заданием: отмена + ожидание потока —
    /// синк дропается в потоке задания (ffmpeg дожимается/убивается своим
    /// abort-ом), сиротских процессов не остаётся.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Default for ShtormApp {
    fn default() -> Self {
        Self::new()
    }
}

impl ShtormApp {
    pub fn new() -> Self {
        Self::with_conf_path(settings_path())
    }

    /// Конструктор с явным путём конфига (продакшн — рядом с exe; тесты —
    /// изолированная песочница, т.к. `settings_path()` привязана к
    /// `current_exe` тестового харнесса).
    ///
    /// G6.5 (переименование приложения): если нового конфига
    /// `PocketTTS-GUI-Rust.json` нет, но рядом лежит прежний
    /// `shtorm_gui_settings.json`, настройки читаются из него и атомарно
    /// переносятся в новый файл. Прежний файл не удаляется и не изменяется.
    /// Повреждённый прежний конфиг — защитная политика `Unreadable` без
    /// перезаписи; повреждённый НОВЫЙ конфиг не подменяется старыми
    /// настройками.
    fn with_conf_path(conf_path: PathBuf) -> Self {
        // Прежнее имя конфига ищем в том же каталоге (продакшн — каталог exe,
        // «рядом со старым EXE или новым EXE» — это один и тот же каталог).
        let legacy_path = conf_path.with_file_name(LEGACY_SETTINGS_FILE);
        let (settings, settings_load, note, snapshot) = match load_settings(&conf_path) {
            Ok(s) => {
                let snapshot = serde_json::to_string_pretty(&s).unwrap_or_default();
                (
                    s,
                    SettingsLoadState::Loaded,
                    format!("настройки загружены: {}", conf_path.display()),
                    snapshot,
                )
            }
            // Нового файла нет — до FirstRun проверяем прежний конфиг (G6.5).
            Err(_) if !conf_path.exists() => match load_settings(&legacy_path) {
                Ok(s) => {
                    let snapshot = serde_json::to_string_pretty(&s).unwrap_or_default();
                    match save_settings(&s, &conf_path) {
                        Ok(()) => (
                            s,
                            SettingsLoadState::Loaded,
                            format!(
                                "настройки перенесены из прежнего конфига {} в новый {}; \
                                 прежний файл оставлен без изменений",
                                legacy_path.display(),
                                conf_path.display()
                            ),
                            snapshot,
                        ),
                        // Перенос не выполнен: настройки работают, ни один конфиг
                        // не затёрт (атомарная запись не трогает цель при сбое);
                        // пустой снимок заставит автосохранение при закрытии
                        // (или ручное сохранение) повторить создание нового файла.
                        Err(e) => (
                            s,
                            SettingsLoadState::Loaded,
                            format!(
                                "настройки прочитаны из прежнего конфига {}, но перенос \
                                 в {} не выполнен ({e:#}); попытка будет повторена \
                                 при сохранении/закрытии",
                                legacy_path.display(),
                                conf_path.display()
                            ),
                            String::new(),
                        ),
                    }
                }
                // Прежнего файла тоже нет — первый запуск.
                Err(_) if !legacy_path.exists() => (
                    GuiSettings::default(),
                    SettingsLoadState::FirstRun,
                    format!(
                        "настройки не найдены — значения по умолчанию (сохранятся при закрытии): {}",
                        conf_path.display()
                    ),
                    String::new(),
                ),
                // Прежний конфиг существует, но не читается: та же защитная
                // политика Unreadable — автоматически ничего не перезаписывать.
                Err(e) => (
                    GuiSettings::default(),
                    SettingsLoadState::Unreadable,
                    format!(
                        "прежний конфиг не прочитан ({e:#}); файл не будет перезаписан \
                         автоматически: {}",
                        legacy_path.display()
                    ),
                    String::new(),
                ),
            },
            // Новый конфиг существует, но не читается: настройки из прежнего
            // файла НЕ подставляем молча (G6.5) — политика прежняя.
            Err(e) => (
                GuiSettings::default(),
                SettingsLoadState::Unreadable,
                format!(
                    "настройки не загружены ({e:#}); конфиг не будет перезаписан автоматически"
                ),
                String::new(),
            ),
        };
        // G5.2 (визуальный контроль): ни один кадр ещё не нарисован — если
        // пометить тему «применённой» здесь, update() никогда не вызовет
        // set_visuals и приложение стартует со светлым дефолтом eframe даже
        // при theme_dark=true. Помечаем НЕ применённой: первый кадр
        // установит visuals до отрисовки (без вспышки светлой темы).
        let theme_applied = !settings.theme_dark;
        // G6.2.3: снимок для сравнения «были ли изменения» при закрытии
        // (G6.5: считается в ветках загрузки выше — миграция задаёт его явно).
        let settings_snapshot = snapshot;
        Self {
            settings,
            log: String::new(),
            // G6.4: статус загрузки конфига — отдельно от рабочего статуса
            // (верхняя панель показывает его постоянно, нижняя — ход задания).
            status: "готов".into(),
            load_note: note,
            last_saved: None,
            conf_path,
            settings_load,
            settings_snapshot,
            close_flow: CloseFlow::Idle,
            close_sent: false,
            autosave_error: None,
            job: None,
            progress: (0, 0),
            file_progress: (0, 0),
            batch_running: false,
            batch_state: None,
            publishing: false,
            ffmpeg_check_key: None,
            ffmpeg_check_err: None,
            ffmpeg_checked_at: std::time::Instant::now(),
            theme_applied,
            style_applied: false,
            log_height: 120.0,
            log_open: true,
            monitor_key: None,
        }
    }

    /// Запуск фонового задания (кнопка «Запустить синтез»): текст читается в
    /// рабочем потоке, UI не блокируется. Повторный запуск при живом задании
    /// исключён (кнопка отключена + проверка здесь).
    fn start_job(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            self.status = "задание уже выполняется".into();
            return;
        }
        let mut s = self.settings.clone();
        s.clamped();
        let batch = s.batch;
        let out_path = if batch {
            PathBuf::new()
        } else {
            crate::job::derive_out_path(&s)
        };
        let (tx, rx) = std::sync::mpsc::channel::<JobEvent>();
        // G6.1: события приходят из НЕСКОЛЬКИХ потоков воркеров пула —
        // Sender обёрнут в Mutex (Sync), emit — разделяемый Arc
        let tx = Arc::new(std::sync::Mutex::new(tx));
        let ctx2 = ctx.clone();
        let emit: Arc<dyn Fn(JobEvent) + Send + Sync> = Arc::new(move |ev: JobEvent| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(ev);
            }
            ctx2.request_repaint();
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel2 = Arc::clone(&cancel);
        // относительный путь источника — от каталога exe, не от CWD
        let text_file = crate::resources::resolve_user_path(&s.text_file);
        let thread = std::thread::Builder::new()
            .name("pockettts-synth".into())
            .spawn(move || {
                let result = if batch {
                    let source = text_file.clone();
                    crate::job::execute_batch(&s, &source, &cancel2, &emit)
                } else {
                    let text = match std::fs::read_to_string(&text_file) {
                        Ok(t) => t,
                        Err(e) => {
                            let r = crate::job::JobResult::failed(format!(
                                "прочитать текстовый файл {}: {e}",
                                text_file.display()
                            ));
                            emit(JobEvent::Log(r.summary.clone()));
                            emit(JobEvent::Finished(Box::new(r)));
                            return;
                        }
                    };
                    let cfg = crate::job::JobConfig {
                        settings: s,
                        text,
                        out_path,
                    };
                    let emit_j = Arc::clone(&emit);
                    crate::job::execute_job(&cfg, &cancel2, &move |ev| emit_j(ev))
                };
                emit(JobEvent::Finished(Box::new(result)));
            })
            .expect("spawn synth thread");
        self.job = Some(JobHandle {
            cancel,
            rx,
            thread: Some(thread),
        });
        self.progress = (0, 0);
        self.file_progress = (0, 0);
        self.batch_running = batch;
        self.batch_state = None;
        self.publishing = false;
        self.status = "задание запущено…".into();
    }

    /// Применение одного события задания к состоянию UI (выделено для
    /// мок-тестов состояний без запуска синтеза).
    fn apply_event(&mut self, ev: JobEvent) {
        match ev {
            JobEvent::Progress { done, total } => {
                self.progress = (done, total);
                self.status = format!("синтез: {done}/{total} чанков");
            }
            JobEvent::Phase(ph) => {
                // G5.1: после точки решения о публикации «Стоп» больше не
                // может предотвратить замену файла — отключаем кнопку и
                // честно показываем сохранение результата.
                match ph {
                    JobPhase::Generating => {
                        self.publishing = false;
                        self.status = "генерация…".into();
                    }
                    JobPhase::Finalizing => {
                        self.publishing = true;
                        self.status = "Сохранение результата: завершение кодирования…".into();
                    }
                    JobPhase::Publishing => {
                        self.publishing = true;
                        self.status = "Сохранение результата: публикация…".into();
                    }
                }
            }
            JobEvent::BatchStarted { total, workers } => {
                self.batch_state = Some(BatchUiState {
                    total,
                    workers,
                    ..Default::default()
                });
                self.refresh_batch_status();
            }
            JobEvent::BatchWorkersReady { workers } => {
                // G6.1.1: только обновление числа воркеров в УЖЕ созданном
                // BatchStarted состоянии — счётчики ранних FileDone preflight
                // не сбрасываются. События до BatchStarted — протокольная
                // аномалия: диагностируем, а не молча создаём состояние.
                match self.batch_state.as_mut() {
                    Some(bs) => bs.workers = workers,
                    None => self.push_log(
                        "ВНИМАНИЕ (протокол): BatchWorkersReady до BatchStarted — \
                         проигнорировано",
                    ),
                }
                self.refresh_batch_status();
            }
            JobEvent::FileStarted {
                index,
                total,
                input,
                output: _,
            } => {
                if let Some(bs) = self.batch_state.as_mut() {
                    bs.total = total;
                    if !bs.active.iter().any(|f| f.index == index) {
                        bs.active.push(ActiveFileUi {
                            index,
                            name: input
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default(),
                            done: 0,
                            total: 0,
                            phase: JobPhase::Generating,
                        });
                    }
                }
                self.refresh_batch_status();
            }
            JobEvent::FileProgress {
                index,
                total,
                done,
                chunks_total,
            } => {
                // per-file прогресс: события разных файлов вперемешку
                self.file_progress = (index, total);
                self.progress = (done, chunks_total);
                if let Some(bs) = self.batch_state.as_mut() {
                    bs.total = total;
                    if let Some(f) = bs.active.iter_mut().find(|f| f.index == index) {
                        f.done = done;
                        f.total = chunks_total;
                    }
                }
                self.refresh_batch_status();
            }
            JobEvent::FilePhase { index, phase } => {
                // фаза КОНКРЕТНОГО файла: публикация этого файла уже
                // необратима, но другие активные файлы ещё отменяемы
                if let Some(bs) = self.batch_state.as_mut() {
                    if let Some(f) = bs.active.iter_mut().find(|f| f.index == index) {
                        f.phase = phase;
                    }
                }
                self.refresh_batch_status();
            }
            JobEvent::FileDone {
                index,
                total,
                report,
            } => {
                // K/N и счётчики обновляются на КАЖДОМ завершённом файле,
                // включая Failed/Skipped; итог пакета считается по реальным
                // результатам всех файлов, а не последнему событию.
                // G6.1.1: повторный FileDone того же индекса — протокольная
                // аномалия: не считаем дважды и честно диагностируем.
                let duplicate = match self.batch_state.as_mut() {
                    Some(bs) => {
                        bs.total = total;
                        if bs.done_indices.contains(&index) {
                            true
                        } else {
                            bs.done_indices.insert(index);
                            bs.active.retain(|f| f.index != index);
                            bs.finished += 1;
                            match report.status {
                                FileStatus::Completed => bs.completed += 1,
                                FileStatus::Failed => bs.failed += 1,
                                FileStatus::Skipped => bs.skipped += 1,
                                FileStatus::Cancelled => bs.cancelled += 1,
                            }
                            false
                        }
                    }
                    None => false,
                };
                if duplicate {
                    self.push_log(&format!(
                        "ВНИМАНИЕ (протокол): повторное завершение файла {index}/{total} \
                         не учтено дважды"
                    ));
                }
                self.publishing = false;
                self.file_progress = (index, total);
                self.progress = (report.chunks_done, report.chunks_total);
                self.push_log(&report.line());
                self.refresh_batch_status();
            }
            JobEvent::Log(line) => self.push_log(&line),
            JobEvent::Finished(res) => {
                let warn = matches!(res.status, JobStatus::Partial | JobStatus::NoWork);
                self.publishing = false;
                self.status = format!(
                    "{}{} — {}",
                    if warn { "⚠ " } else { "" },
                    res.status.label(),
                    res.summary
                );
                self.push_log(&res.summary);
            }
        }
    }

    /// Строка статуса пакетного режима из счётчиков per-file модели.
    fn refresh_batch_status(&mut self) {
        let Some(bs) = &self.batch_state else { return };
        if !self.batch_running {
            return;
        }
        let mut s = format!(
            "пакет: {}/{} файлов (готово {} | ошибок {} | пропущено {})",
            bs.finished, bs.total, bs.completed, bs.failed, bs.skipped
        );
        if bs.cancelled > 0 {
            s.push_str(&format!(" | отменено {}", bs.cancelled));
        }
        // G6.1.1: workers=0 — пул ещё не загружен (preflight/загрузка Pocket);
        // честное «готовятся» вместо ложного «работают N»
        if bs.workers > 0 {
            s.push_str(&format!("; одновременно {}", bs.workers));
        } else {
            s.push_str("; воркеры готовятся…");
        }
        self.status = s;
    }

    /// G6.1: доступность «Стоп». В пакете кнопка доступна, пока хотя бы один
    /// активный файл находится в генерации (файлы, перешедшие точку
    /// публикации, дорабатывают сами); между файлами — доступна. Одиночный
    /// режим — прежняя семантика G5.1 (фазы одного файла).
    fn stop_available(&self) -> bool {
        if self.batch_running {
            if let Some(bs) = &self.batch_state {
                return bs.active.is_empty()
                    || bs.active.iter().any(|f| f.phase == JobPhase::Generating);
            }
        }
        !self.publishing
    }

    /// Дренаж событий задания + завершение потока (вызывается каждый кадр).
    fn poll_job(&mut self) {
        let Some(job) = &mut self.job else { return };
        let mut events = Vec::new();
        while let Ok(ev) = job.rx.try_recv() {
            events.push(ev);
        }
        let thread_done = job.thread.as_ref().is_some_and(|h| h.is_finished());
        for ev in events {
            self.apply_event(ev);
        }
        // поток закончил работу (Finished получен/канал пуст) — прибираем
        if thread_done {
            let mut job = self.job.take().expect("job есть");
            if let Some(h) = job.thread.take() {
                let _ = h.join();
            }
        }
    }

    // ===== G6.2.3: закрытие окна и автосохранение настроек =====

    /// Машина состояний закрытия (вызывается из update() каждый кадр ДО
    /// панелей; отдельный метод — headless-тесты состояний без окна).
    ///
    /// Порядок при живом задании сохранён от G2: отмена → удержание окна
    /// (CancelClose) → завершение потока → автосохранение → Close. Отмена
    /// не блокирует UI-поток (нет ожидания CUDA/IPC — только флаг и дренаж
    /// событий в poll_job).
    // G6.8: pub(crate) — headless-тесты программного рантайма (sw.rs)
    // проверяют контракт CloseFlow поверх обоих рантаймов.
    pub(crate) fn handle_close_flow(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested {
            match self.close_flow {
                CloseFlow::Idle => {
                    if let Some(job) = &self.job {
                        job.cancel.store(true, Ordering::SeqCst);
                        self.push_log("закрытие: ожидаю отмену задания…");
                        self.close_flow = CloseFlow::WaitingForJob;
                    } else {
                        self.close_flow = CloseFlow::Saving;
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
                // Повторный крестик, пока окно удерживается (задание
                // завершается / диалог ошибки сохранения открыт), не должен
                // обгонять уже идущее закрытие — продолжаем отменять.
                CloseFlow::WaitingForJob | CloseFlow::Saving | CloseFlow::SaveFailed => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
                // Closing: собственный ViewportCommand::Close уже отправлен;
                // индуцированный им close_requested — НЕ отменяем, иначе
                // закрытие зациклится (процесс завершается после кадра).
                CloseFlow::Closing => {}
            }
        }
        // фоновое задание завершилось (после отмены) — к сохранению и закрытию
        if self.close_flow == CloseFlow::WaitingForJob && self.job.is_none() {
            self.close_flow = CloseFlow::Saving;
        }
        // попытка автосохранения — в кадре перехода; задание не живое,
        // запись маленького JSON локальна и атомарна (UI не подвисает)
        if self.close_flow == CloseFlow::Saving && self.job.is_none() {
            self.attempt_autosave();
        }
        // решение принято — закрываем окно сами (ровно один Close)
        if self.close_flow == CloseFlow::Closing && !self.close_sent {
            self.close_sent = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Ручное сохранение кнопкой «Сохранить настройки» (и тестами — общий
    /// метод, чтобы логика состояния не дублировалась). При успехе:
    /// обновляется снимок для пропуска лишней записи при закрытии, и
    /// НЕЧИТАЕМЫЙ конфиг переводится в Loaded — файл на диске теперь
    /// корректен по явному действию пользователя, автосохранение при
    /// закрытии снова работает (дефект G6.2.3-fix: state оставался
    /// Unreadable и последние правки терялись). При ошибке состояние
    /// загрузки НЕ меняется: защита повреждённого JSON сохраняется,
    /// существующий файл не тронут (атомарная запись в save_settings).
    /// Возвращает текст ошибки для вызывающего кода/тестов.
    fn manual_save(&mut self) -> Result<(), String> {
        self.settings.clamped();
        match save_settings(&self.settings, &self.conf_path) {
            Ok(()) => {
                self.last_saved = Some(self.conf_path.clone());
                self.settings_snapshot =
                    serde_json::to_string_pretty(&self.settings).unwrap_or_default();
                self.settings_load = SettingsLoadState::Loaded;
                self.status = format!("сохранено: {}", self.conf_path.display());
                Ok(())
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.status = format!("ошибка сохранения: {msg}");
                Err(msg)
            }
        }
    }

    /// Попытка автосохранения при закрытии (G6.2.3). Политики:
    /// - конфиг не читался при старте (повреждён) — НЕ перезаписывать молча:
    ///   пользовательский файл остаётся нетронутым, причина — в журнале
    ///   (и сообщалась в статусе при старте);
    /// - изменений с момента старта нет — файл не трогаем (без tmp-файлов);
    /// - ошибка записи — окно остаётся живым с явным выбором, существующий
    ///   файл цел (атомарная замена: сбой до MoveFileExW не трогает его).
    fn attempt_autosave(&mut self) {
        if self.settings_load == SettingsLoadState::Unreadable {
            self.push_log(&format!(
                "закрытие: настройки при запуске не были прочитаны — автосохранение \
                 пропущено, существующий конфиг не изменён ({})",
                self.conf_path.display()
            ));
            self.close_flow = CloseFlow::Closing;
            return;
        }
        self.settings.clamped();
        let current = serde_json::to_string_pretty(&self.settings).unwrap_or_default();
        if current == self.settings_snapshot {
            self.close_flow = CloseFlow::Closing;
            return;
        }
        match save_settings(&self.settings, &self.conf_path) {
            Ok(()) => {
                self.settings_snapshot = current;
                self.close_flow = CloseFlow::Closing;
            }
            Err(e) => {
                self.autosave_error = Some(format!("{e:#}"));
                self.push_log(&format!("ошибка автосохранения настроек: {e:#}"));
                self.close_flow = CloseFlow::SaveFailed;
            }
        }
    }

    /// Диалог ошибки автосохранения: окно не закрывается молча — явный выбор
    /// «Повторить сохранение» или «Выйти без сохранения» (рендер после
    /// панелей, поверх них).
    fn show_autosave_dialog(&mut self, ctx: &egui::Context) {
        if self.close_flow != CloseFlow::SaveFailed {
            return;
        }
        let mut flow = self.close_flow;
        let err = self
            .autosave_error
            .clone()
            .unwrap_or_else(|| "неизвестная ошибка".into());
        egui::Window::new("Настройки не сохранены")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Не удалось автоматически сохранить настройки:");
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&err).small());
                ui.add_space(4.0);
                ui.label("Файл настроек не изменён.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Повторить сохранение").clicked() {
                        flow = CloseFlow::Saving;
                    }
                    if ui.button("Выйти без сохранения").clicked() {
                        flow = CloseFlow::Closing;
                    }
                });
            });
        self.close_flow = flow;
    }

    /// Строка в журнал (ограничение размера: последние ~400 строк).
    fn push_log(&mut self, line: &str) {
        self.log.push_str(line);
        self.log.push('\n');
        if self.log.lines().count() > 400 {
            if let Some(pos) = self
                .log
                .match_indices('\n')
                .nth(self.log.lines().count() - 400)
                .map(|(i, _)| i)
            {
                self.log.drain(..pos + 1);
            }
        }
    }

    /// Валидация путей/параметров. ffmpeg проверяется общим `resolve_ffmpeg`
    /// (явный путь → автопоиск), результат кэшируется, чтобы не запускать
    /// процесс каждый кадр; при ошибке — перепроверка раз в 2 секунды.
    fn validate(&mut self) -> Vec<String> {
        let needs_ffmpeg = self.settings.format.needs_ffmpeg();
        let mut ffmpeg_err = None;
        if needs_ffmpeg {
            let key = self.settings.ffmpeg_path.trim().to_string();
            let now = std::time::Instant::now();
            let recheck = self.ffmpeg_check_err.is_some()
                && now.duration_since(self.ffmpeg_checked_at) >= std::time::Duration::from_secs(2);
            if self.ffmpeg_check_key.as_deref() != Some(key.as_str()) || recheck {
                let explicit = if key.is_empty() {
                    None
                } else {
                    // относительный путь ffmpeg — от каталога exe
                    Some(crate::resources::resolve_user_path(&key))
                };
                self.ffmpeg_check_err = app_core::sink::resolve_ffmpeg(explicit.as_deref())
                    .err()
                    .map(|e| format!("{e:#}"));
                self.ffmpeg_check_key = Some(key);
                self.ffmpeg_checked_at = now;
            }
            ffmpeg_err = self.ffmpeg_check_err.clone();
        }

        let mut errs = Vec::new();
        let s = &self.settings;
        let text_path = crate::resources::resolve_user_path(&s.text_file);
        let voice_path = crate::resources::resolve_user_path(&s.voice_wav);
        if s.text_file.trim().is_empty() {
            errs.push(if s.batch {
                "не выбран исходный каталог".into()
            } else {
                "не выбран текстовый файл".into()
            });
        } else if s.batch {
            if !text_path.is_dir() {
                errs.push(format!(
                    "исходный каталог не найден: {}",
                    text_path.display()
                ));
            }
        } else if !text_path.is_file() {
            errs.push(format!("текстовый файл не найден: {}", text_path.display()));
        }
        if s.voice_wav.trim().is_empty() {
            errs.push("не выбран WAV-голос".into());
        } else if !s.voice_wav.to_lowercase().ends_with(".wav") {
            errs.push("голосовой референс: поддерживается только WAV".into());
        } else if !voice_path.is_file() {
            errs.push(format!("WAV-голос не найден: {}", voice_path.display()));
        }
        // модель — единым резолвером запуска (fast/slow через onnx_base);
        // ошибки резолва не глотаются: без модели проверка пути результата
        // по каталогу модели бессмысленна, а задание всё равно не стартует
        let model_dir = match crate::job::resolve_model_dir(s) {
            Ok(d) => Some(d),
            Err(e) => {
                errs.push(e.to_string());
                None
            }
        };
        if s.out_dir.trim().is_empty() {
            errs.push("не указана папка результата".into());
        }
        // G5: существующий результат — не ошибка валидации. При выключенной
        // перезаписи задание само сообщит «Пропущено: файл уже существует»;
        // при включённой — заменит файл только после успешного синтеза.
        // Пути вывода проверяются на reparse point и защищённые файлы здесь,
        // чтобы пользователь увидел проблему до запуска.
        if !s.batch && !s.out_dir.trim().is_empty() {
            if let Some(model_dir) = &model_dir {
                let out = crate::job::derive_out_path(s);
                if let Err(e) = crate::job::ensure_output_path_safe(s, &out, model_dir) {
                    errs.push(format!("{e:#}"));
                }
            }
        }
        if needs_ffmpeg {
            if let Some(e) = ffmpeg_err {
                errs.push(e);
            }
            // Пресет обязан соответствовать формату: несовместимый выбор —
            // ошибка, а не тихая подстановка другого пресета.
            if let Err(e) = app_core::sink::encoder_args(
                crate::job::output_format(s.format),
                s.preset_for_format(),
            ) {
                errs.push(format!("пресет качества: {e:#}"));
            }
        }
        errs
    }

    /// G6.4: содержимое центральной прокручиваемой области: режим обработки
    /// и все секции настроек. Ширину задаёт вызывающий (форма центрируется,
    /// на узком окне колонки автоматически становятся одной).
    fn form_ui(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.settings;

        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Режим обработки:").strong());
            ui.radio_value(&mut s.batch, false, "Один файл");
            ui.radio_value(&mut s.batch, true, "Каталог (пакетно)");
            if s.batch {
                secondary_label(ui, "рекурсивно .txt/.md/.log");
            }
        });
        ui.add_space(4.0);

        // ---- Источник и голос ----
        let batch = s.batch;
        let resp = egui::CollapsingHeader::new(egui::RichText::new("Источник и голос").heading().strong())
            .open(Some(s.source_open))
            .show(ui, |ui| {
                path_row(
                    ui,
                    if batch {
                        "Исходный каталог:"
                    } else {
                        "Текстовый файл:"
                    },
                    &mut s.text_file,
                    || {
                        if batch {
                            rfd::FileDialog::new()
                                .set_title("Выберите каталог с .txt/.md/.log")
                                .pick_folder()
                        } else {
                            rfd::FileDialog::new()
                                .set_title("Выберите текстовый файл")
                                .add_filter("Текст", &["txt", "md", "log"])
                                .pick_file()
                        }
                    },
                );
                if batch {
                    hint(
                        ui,
                        "Рекурсивно .txt/.md/.log без символических ссылок; выходная папка исключается.",
                    );
                }
                path_row(ui, "WAV-голос:", &mut s.voice_wav, || {
                    rfd::FileDialog::new()
                        .set_title("Выберите WAV-голос (1–30 с чистой речи)")
                        .add_filter("WAV", &["wav"])
                        .pick_file()
                });
                hint(
                    ui,
                    "Голосовой референс: поддерживается WAV.",
                );
            });
        header_clicked(&resp, &mut s.source_open);
        ui.add_space(8.0);

        // ---- Модель и устройство ----
        let resp = egui::CollapsingHeader::new(
            egui::RichText::new("Модель и устройство")
                .heading()
                .strong(),
        )
        .open(Some(s.model_open))
        .show(ui, |ui| {
            let is_cuda = s.device == DeviceChoice::Cuda;
            let int8 = s.model.precision() == shtorm_engine::ModelPrecision::Int8;
            let model_note = crate::job::resolve_model_dir(s)
                .map(|d| d.display().to_string())
                .unwrap_or_else(|e| e.to_string());
            // Строгий вертикальный порядок Python GUI: модель → каталог →
            // разрешённый путь → устройство → DLL → подсказка → воркеры.
            // Две независимые колонки в этом блоке запрещены ТЗ G6.4.
            // G6.3: один список — вариант сразу задаёт и Fast/Slow,
            // и точность графов; отдельного поля «точность» нет.
            combo_row(ui, "Модель:", "model", s.model.label(), |ui| {
                for choice in [
                    ModelChoice::Fast,
                    ModelChoice::FastInt8,
                    ModelChoice::Slow,
                    ModelChoice::SlowInt8,
                    ModelChoice::Custom,
                ] {
                    ui.selectable_value(&mut s.model, choice, choice.label());
                }
            });
            if s.model == ModelChoice::Custom {
                path_row(ui, "Каталог модели:", &mut s.model_dir, || {
                    rfd::FileDialog::new()
                        .set_title("Каталог модели (flow_lm_main.onnx и др.)")
                        .pick_folder()
                });
            } else {
                path_row(
                    ui,
                    "Каталог ONNX (база):",
                    &mut s.onnx_base,
                    || {
                        rfd::FileDialog::new()
                            .set_title("Каталог с ru-v2-fast / ru-v2-slow")
                            .pick_folder()
                    },
                );
            }
            hint(ui, format!("модель: {model_note}"));
            ui.horizontal(|ui| {
                label_cell(ui, "Устройство:");
                let selected = match s.device {
                    DeviceChoice::Cpu => "CPU",
                    DeviceChoice::Cuda => "CUDA",
                };
                egui::ComboBox::from_id_salt("device")
                    .width(110.0)
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.device, DeviceChoice::Cpu, "CPU");
                        ui.selectable_value(&mut s.device, DeviceChoice::Cuda, "CUDA");
                    });
                if is_cuda {
                    ui.add(
                        egui::DragValue::new(&mut s.device_id)
                            .range(0..=8)
                            .prefix("GPU "),
                    );
                }
            });
            let dll = if is_cuda {
                &mut s.ort_gpu_dll
            } else {
                &mut s.ort_cpu_dll
            };
            path_row(ui, "onnxruntime.dll:", dll, || {
                rfd::FileDialog::new()
                    .set_title("onnxruntime.dll")
                    .add_filter("DLL", &["dll"])
                    .pick_file()
            });
            hint(ui, "(пусто = авто из deps проекта)");
            if int8 {
                hint(
                    ui,
                    "INT8: 3 квантованных графа (flow_lm_main, flow_lm_flow, \
                     mimi_decoder) + 2 FP32 (mimi_encoder, text_conditioner).",
                );
                if is_cuda {
                    // G6.3: квантованные узлы CUDA EP не поддерживает — они
                    // идут на CPU с покадровыми копиями; предупреждение без
                    // блокировки и смены выбора.
                    hint_colored(
                        ui,
                        "CUDA + INT8: работает, но замеренно ~2.7× медленнее \
                         FP32 (квантованные узлы выполняются на CPU); для INT8 \
                         рекомендован CPU. Выбор не меняется автоматически.",
                        caution_color(ui),
                    );
                }
            }
        });
        header_clicked(&resp, &mut s.model_open);
        ui.add_space(8.0);

        // ---- Параметры генерации ----
        let resp = egui::CollapsingHeader::new(
            egui::RichText::new("Параметры генерации")
                .heading()
                .strong(),
        )
        .open(Some(s.gen_open))
        .show(ui, |ui| {
            two_columns(
                ui,
                |ui| {
                    slider_row(ui, "Температура:", |ui| {
                        ui.add(
                            egui::Slider::new(&mut s.temperature, 0.05..=1.5)
                                .step_by(0.05)
                                .fixed_decimals(2),
                        );
                    });
                    slider_row(ui, "Шаги декодирования:", |ui| {
                        ui.add(egui::Slider::new(&mut s.decode_steps, 1..=8));
                    });
                    slider_row(ui, "Макс. токенов:", |ui| {
                        ui.add(egui::Slider::new(&mut s.max_tokens, 10..=300).step_by(5.0));
                    });
                    ui.horizontal(|ui| {
                        label_cell(ui, "Seed:");
                        ui.add(egui::DragValue::new(&mut s.seed).range(-1..=i64::MAX));
                        secondary_label(ui, "(-1 — случайный)");
                    });
                },
                |ui| {
                    checkbox_row(ui, &mut s.anti_repeat, "Защита от повторов (двухпопытная)");
                    checkbox_row(ui, &mut s.denoise, "Очистка звука (DPDFNet)");
                    slider_row(ui, "Яркость (дБ):", |ui| {
                        ui.add(
                            egui::Slider::new(&mut s.brightness_db, 0.0..=12.0).fixed_decimals(1),
                        );
                    });
                },
            );
            ui.add_space(4.0);
            // G4.1: компактная группа EOS — порог и оба хвоста.
            // Значения teratts-android: порог −4, хвосты 5/3 — без
            // Python-пересчёта 6/4.
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("EOS (конец речи)").strong())
                        .on_hover_text(
                            "Параметры конца речи (EOS) движка teratts-android. \
                                     Дефолты −4/5/3 воспроизводят оригинальную Android-версию.",
                        );
                    if ui
                        .button("Значения teratts-android")
                        .on_hover_text(
                            "Установить порог −4 и хвосты 5/3 — встроенные значения \
                                     оригинального teratts-android (без пересчёта).",
                        )
                        .clicked()
                    {
                        s.eos_threshold = crate::settings::EosThresholdSetting::Fixed(-4.0);
                        s.eos_tail_short = shtorm_engine::EosTail::Fixed(5);
                        s.eos_tail_long = shtorm_engine::EosTail::Fixed(3);
                    }
                });
                // G5: «Авто» в списке больше нет — выбранное значение
                // всегда число. Устаревший Auto из старого конфига
                // показываем как −4.0 и при любом выборе заменяем числом.
                let threshold_label = format!("{}", s.eos_threshold.value());
                combo_row(ui, "Порог:", "eos_threshold", &threshold_label, |ui| {
                    for v in crate::settings::EOS_THRESHOLD_CHOICES {
                        ui.selectable_value(
                            &mut s.eos_threshold,
                            crate::settings::EosThresholdSetting::Fixed(*v),
                            format!("{v}"),
                        );
                    }
                })
                .on_hover_text(
                    "Логит EOS выше порога — конец речи. Значение \
                             передаётся движку как есть; −4.0 — встроенное \
                             значение teratts-android (не читается из runtime.json).",
                );
                ui.horizontal(|ui| {
                    label_cell(ui, "Кадров хвоста EOS:");
                    ui.label("≤4 слов:").on_hover_text(
                        "Хвост для коротких чанков (1–4 слова по подготовленному \
                                 тексту до добавления служебных пробелов).",
                    );
                    egui::ComboBox::from_id_salt("eos_tail_short")
                        .width(64.0)
                        .selected_text(eos_tail_label(s.eos_tail_short))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut s.eos_tail_short,
                                shtorm_engine::EosTail::Default,
                                "None",
                            );
                            for n in crate::settings::EOS_TAIL_CHOICES {
                                ui.selectable_value(
                                    &mut s.eos_tail_short,
                                    shtorm_engine::EosTail::Fixed(*n),
                                    n.to_string(),
                                );
                            }
                        })
                        .response
                        .on_hover_text(EOS_TAIL_TOOLTIP);
                    ui.label("≥5 слов:")
                        .on_hover_text("Хвост для длинных чанков (5 и более слов текущего чанка).");
                    egui::ComboBox::from_id_salt("eos_tail_long")
                        .width(64.0)
                        .selected_text(eos_tail_label(s.eos_tail_long))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut s.eos_tail_long,
                                shtorm_engine::EosTail::Default,
                                "None",
                            );
                            for n in crate::settings::EOS_TAIL_CHOICES {
                                ui.selectable_value(
                                    &mut s.eos_tail_long,
                                    shtorm_engine::EosTail::Fixed(*n),
                                    n.to_string(),
                                );
                            }
                        })
                        .response
                        .on_hover_text(EOS_TAIL_TOOLTIP);
                });
            });
        });
        header_clicked(&resp, &mut s.gen_open);
        ui.add_space(8.0);

        // ---- Паузы и воркеры ----
        let resp =
            egui::CollapsingHeader::new(egui::RichText::new("Паузы и воркеры").heading().strong())
                .open(Some(s.pauses_open))
                .show(ui, |ui| {
                    two_columns(
                        ui,
                        |ui| {
                            slider_row(ui, "Масштаб пауз:", |ui| {
                                ui.add(
                                    egui::Slider::new(&mut s.pause_scale, 0.0..=2.0)
                                        .step_by(0.05)
                                        .fixed_decimals(2),
                                );
                            });
                        },
                        |ui| {
                            slider_row(ui, "Пауза абзаца (с):", |ui| {
                                ui.add(
                                    egui::Slider::new(&mut s.paragraph_pause_s, 0.0..=3.0)
                                        .step_by(0.05)
                                        .fixed_decimals(2),
                                );
                            });
                        },
                    );
                    // G6.7: ОДИН универсальный блок параллельной обработки для
                    // обоих режимов (одиночный TXT и каталог) и обоих устройств
                    // (CPU/CUDA): P/W/C/T, пресеты «Оптимальный»/«Производительный»,
                    // ручное состояние «Свой». Старые «Воркеров вручную» и
                    // дубль «Потоков ORT» одиночного режима удалены (ТЗ G6.7 §7).
                    parallel_processing_block(ui, s);
                });
        header_clicked(&resp, &mut s.pauses_open);
        ui.add_space(8.0);

        // ---- Вывод и формат ----
        let resp =
            egui::CollapsingHeader::new(egui::RichText::new("Вывод и формат").heading().strong())
                .open(Some(s.out_open))
                .show(ui, |ui| {
                    let needs_ffmpeg = s.format.needs_ffmpeg();
                    two_columns(
                        ui,
                        |ui| {
                            ui.horizontal(|ui| {
                                label_cell(ui, "Формат:");
                                egui::ComboBox::from_id_salt("format")
                                    .width(90.0)
                                    .selected_text(s.format.label())
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut s.format,
                                            OutputFormatChoice::Wav,
                                            "WAV",
                                        );
                                        ui.selectable_value(
                                            &mut s.format,
                                            OutputFormatChoice::Mp3,
                                            "MP3",
                                        );
                                        ui.selectable_value(
                                            &mut s.format,
                                            OutputFormatChoice::Opus,
                                            "Opus",
                                        );
                                    });
                                // Пресеты раздельные: каждый формат показывает и
                                // меняет только свой — несовместимый выбор невозможен.
                                match s.format {
                                    OutputFormatChoice::Mp3 => {
                                        egui::ComboBox::from_id_salt("mp3_preset")
                                            .width(170.0)
                                            .selected_text(&s.mp3_preset)
                                            .show_ui(ui, |ui| {
                                                for p in crate::settings::MP3_PRESETS {
                                                    ui.selectable_value(
                                                        &mut s.mp3_preset,
                                                        (*p).to_string(),
                                                        *p,
                                                    );
                                                }
                                            });
                                    }
                                    OutputFormatChoice::Opus => {
                                        egui::ComboBox::from_id_salt("opus_preset")
                                            .width(140.0)
                                            .selected_text(&s.opus_preset)
                                            .show_ui(ui, |ui| {
                                                for p in crate::settings::OPUS_PRESETS {
                                                    ui.selectable_value(
                                                        &mut s.opus_preset,
                                                        (*p).to_string(),
                                                        *p,
                                                    );
                                                }
                                            });
                                    }
                                    OutputFormatChoice::Wav => {}
                                }
                            });
                        },
                        |ui| {
                            ui.checkbox(&mut s.chunk_log, "Лог чанков");
                            // G5: замена готового аудиофайла — только явное согласие.
                            // Старый файл не удаляется до успешного синтеза; замена
                            // выполняется атомарно после финализации sink.
                            ui.checkbox(
                                &mut s.overwrite_existing,
                                "Перезаписывать существующие файлы",
                            )
                            .on_hover_text(
                                "Выключено (по умолчанию): существующий результат \
                             пропускается без изменений. Включено: новый аудиофайл \
                             пишется в уникальный .partial и заменяет готовый только \
                             после успешного завершения синтеза; при ошибке/отмене \
                             старый файл остаётся. Защита входных файлов, моделей и \
                             настроек не ослабляется.",
                            );
                        },
                    );
                    if needs_ffmpeg {
                        path_row(ui, "ffmpeg:", &mut s.ffmpeg_path, || {
                            rfd::FileDialog::new()
                                .set_title("ffmpeg.exe")
                                .add_filter("ffmpeg", &["exe"])
                                .pick_file()
                        });
                    }
                    path_row(
                        ui,
                        "Папка результата:",
                        &mut s.out_dir,
                        || {
                            rfd::FileDialog::new()
                                .set_title("Папка результата")
                                .pick_folder()
                        },
                    );
                });
        header_clicked(&resp, &mut s.out_open);
        ui.add_space(8.0);
    }

    /// G6.4: однократная реакция на смену монитора/разрешения/DPI: минимум
    /// окна и (при необходимости) его размер подрезаются под новый монитор,
    /// чтобы элементы оставались доступными. Ключ монитора исключает
    /// повторные команды и resize-циклы; ручной ресайз не затрагивается.
    fn adapt_to_monitor(&mut self, ctx: &egui::Context) {
        let (monitor, ppp) = ctx.input(|i| {
            let vp = i.viewport();
            (vp.monitor_size, vp.native_pixels_per_point)
        });
        let Some(monitor) = monitor else { return };
        let key = (
            monitor.x.round() as i32,
            monitor.y.round() as i32,
            (ppp.unwrap_or(1.0) * 100.0).round() as i32,
        );
        if self.monitor_key == Some(key) {
            return;
        }
        self.monitor_key = Some(key);
        // Приблизительная доступная область нового монитора: только поля по
        // краям. Точная рабочая область (без панели задач) вычисляется на
        // старте (window.rs); здесь важно лишь не оставить окно БОЛЬШЕ
        // монитора при смене разрешения/отключении второго монитора.
        let avail = egui::vec2((monitor.x - 32.0).max(240.0), (monitor.y - 32.0).max(180.0));
        let min = egui::vec2(
            crate::window::MIN_WIDTH.min(avail.x),
            crate::window::MIN_HEIGHT.min(avail.y),
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(min));
        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
            let size = rect.size();
            if size.x > avail.x || size.y > avail.y {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    size.x.min(avail.x),
                    size.y.min(avail.y),
                )));
            }
        }
    }
}

// ===== G6.4: адаптивная компоновка формы =====

/// Максимальная ширина формы: на широком мониторе поля не растягиваются на
/// всю ширину, а форма центрируется.
const FORM_MAX_WIDTH: f32 = 1160.0;
const LABEL_WIDTH: f32 = 165.0;
/// Высота строки параметра — под основной текст 15 пт с полями.
const ROW_HEIGHT: f32 = 26.0;
const SLIDER_MIN_WIDTH: f32 = 80.0;
const SLIDER_MAX_WIDTH: f32 = 360.0;
/// Запас справа под значение ползунка («0.30», «300», «8.0»), чтобы оно
/// не обрезалось краем колонки.
const SLIDER_VALUE_RESERVE: f32 = 64.0;
const COMBO_MAX_WIDTH: f32 = 300.0;
const LOG_MIN_HEIGHT: f32 = 48.0;
const LOG_HEADER_HEIGHT: f32 = 34.0;
const LOG_HANDLE_HEIGHT: f32 = 6.0;

// Типографика e03342f (G6.4-fix) в ЛОГИЧЕСКИХ пунктах: egui × DPI.
const BODY_SIZE: f32 = 15.0;
const SMALL_SIZE: f32 = 14.0;
const HEADING_SIZE: f32 = 19.0;

/// Минимальная ширина одной колонки параметров: подпись + ползунок
/// минимальной ширины + значение + два горизонтальных отступа. Порог
/// 2→1 колонки выводится из неё, а не из конкретного разрешения монитора.
const COLUMN_MIN_WIDTH: f32 = LABEL_WIDTH + SLIDER_MIN_WIDTH + SLIDER_VALUE_RESERVE + 2.0 * 8.0;
/// Дополнительный зазор/запас для двух колонок.
const COLUMN_GAP: f32 = 12.0;

fn two_columns_threshold() -> f32 {
    COLUMN_MIN_WIDTH * 2.0 + COLUMN_GAP
}

/// Типографика и отступы — один раз при старте. Размеры логические:
/// при 125/150/200 % Windows масштабирование делает egui, а не мы.
/// (egui 0.34: `Context::style`/`set_style` переименованы в
/// `global_style`/`set_global_style`.)
fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.text_styles = [
        (
            egui::TextStyle::Heading,
            egui::FontId::proportional(HEADING_SIZE),
        ),
        (egui::TextStyle::Body, egui::FontId::proportional(BODY_SIZE)),
        (
            egui::TextStyle::Button,
            egui::FontId::proportional(BODY_SIZE),
        ),
        (
            egui::TextStyle::Small,
            egui::FontId::proportional(SMALL_SIZE),
        ),
        (
            egui::TextStyle::Monospace,
            egui::FontId::monospace(SMALL_SIZE),
        ),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size = egui::vec2(40.0, ROW_HEIGHT);
    style.spacing.icon_width = 16.0;
    style.spacing.icon_width_inner = 9.0;
    style.spacing.icon_spacing = 5.0;
    style.spacing.indent = 20.0;
    ctx.set_global_style(style);
}

/// Второстепенный текст: 13 пт (как основной) и контраст в обеих темах
/// (вместо `.small().weak()`, который на тёмной теме был слишком тусклым).
fn secondary_text_color(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_gray(178)
    } else {
        egui::Color32::from_gray(88)
    }
}

fn secondary_label(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.add(egui::Label::new(
        egui::RichText::new(text.into())
            .small()
            .color(secondary_text_color(ui)),
    ));
}

fn ok_color(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_rgb(100, 220, 110)
    } else {
        egui::Color32::from_rgb(20, 110, 30)
    }
}

fn warn_color(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_rgb(245, 150, 80)
    } else {
        egui::Color32::from_rgb(165, 65, 0)
    }
}

fn caution_color(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_rgb(245, 185, 90)
    } else {
        egui::Color32::from_rgb(145, 85, 0)
    }
}

/// Две колонки только при фактической ширине, достаточной для двух
/// колонок минимального содержимого (не по разрешению монитора).
fn uses_two_columns(available_width: f32) -> bool {
    available_width >= two_columns_threshold()
}

/// Подпись параметра фиксированной ширины — значения в строках выровнены
/// (как в Python-версии), длинные подписи не ломают сетку. Текст прижат
/// к левому краю ячейки: `add_sized` центрирует содержимое и давал
/// «плавающие» подписи.
fn label_cell(ui: &mut egui::Ui, text: &str) {
    // Ширина подписи измеряется реальным шрифтом: 150 пт — базовая, но
    // длинная локализованная подпись не обрезается, если её можно
    // разместить (не более 45 % доступной ширины).
    let font = egui::TextStyle::Body.resolve(ui.style());
    let measured = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, egui::Color32::WHITE)
        .size()
        .x
        + 8.0;
    let cap = (ui.available_width() * 0.45).max(120.0);
    let width = measured.clamp(LABEL_WIDTH.min(cap), cap);
    ui.allocate_ui_with_layout(
        egui::vec2(width, ROW_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.add(egui::Label::new(text).truncate())
                .on_hover_text(text);
        },
    );
}

fn hint(ui: &mut egui::Ui, text: impl Into<String>) {
    // Мягкие точки переноса: длинный путь переносится, а не выходит за
    // правую границу колонки/окна.
    let text = soft_wrap_paths(&text.into());
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .small()
                .color(secondary_text_color(ui)),
        )
        .wrap(),
    );
}

fn hint_colored(ui: &mut egui::Ui, text: impl Into<String>, color: egui::Color32) {
    let text = soft_wrap_paths(&text.into());
    ui.add(egui::Label::new(egui::RichText::new(text).small().color(color)).wrap());
}

/// Мягкие точки переноса для длинных путей: невидимый U+200B после
/// разделителей и каждых 24 символов. Текст визуально не меняется, но
/// egui может переносить длинный путь, а не обрезать его краем панели.
fn soft_wrap_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 24);
    let mut run = 0usize;
    for ch in text.chars() {
        out.push(ch);
        if matches!(ch, '\\' | '/' | ':' | ' ') {
            run = 0;
        } else {
            run += 1;
            if run >= 24 {
                out.push('\u{200B}');
                run = 0;
            }
        }
    }
    out
}

/// Строка «подпись + ползунок». Ширина ползунка ограничена: на широком
/// мониторе он не растягивается на всю колонку, значение видно в самом
/// ползунке рядом с бегунком.
fn slider_row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        label_cell(ui, label);
        let w =
            (ui.available_width() - SLIDER_VALUE_RESERVE).clamp(SLIDER_MIN_WIDTH, SLIDER_MAX_WIDTH);
        ui.spacing_mut().slider_width = w;
        add(ui);
    });
}

/// Строка «подпись + ComboBox» ограниченной ширины.
fn combo_row(
    ui: &mut egui::Ui,
    label: &str,
    id: &str,
    selected: &str,
    add: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let mut response = None;
    ui.horizontal(|ui| {
        label_cell(ui, label);
        let w = ui.available_width().clamp(80.0, COMBO_MAX_WIDTH);
        response = Some(
            egui::ComboBox::from_id_salt(id)
                .width(w)
                .selected_text(selected)
                .show_ui(ui, add)
                .response,
        );
    });
    response.expect("combo row")
}

/// Фиксированная ширина кнопки «Обзор…»: поле получает ровно остаток
/// строки, поэтому длинный путь не выталкивает кнопку за пределы окна.
const BROWSE_BUTTON_WIDTH: f32 = 92.0;

/// G6.6: универсальный блок «Параллельная обработка» для каталога.
/// Одинаковые поля/порядок/пресеты для CPU и CUDA; устройства отличаются
/// только диапазонами (parallel_limits) и оценкой ресурсов. Кнопки пресетов
/// задают ПОЛНУЮ конфигурацию (адаптация под CPU/RAM текущей машины);
/// ручная правка любого поля переводит профиль в «Свой». Конфигурации CPU и
/// CUDA хранятся независимо (parallel_cpu / parallel_cuda).
fn parallel_processing_block(ui: &mut egui::Ui, s: &mut GuiSettings) {
    use crate::settings::{parallel_limits, ParallelProfile, ParallelSettings};

    let device = s.device;
    let (pmax, wmax) = parallel_limits(device);
    let mut cfg = s.parallel_config();
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    let precision = s.model.precision();

    // правка поля: профиль → «Свой», значения зажимаются, F ≤ P×W
    let edited = |cfg: &mut ParallelSettings| {
        cfg.clamp_for_device(device);
        cfg.profile = ParallelProfile::Custom;
    };

    ui.add_space(4.0);
    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Параллельная обработка").strong())
                .on_hover_text(
                    "Процесс (P) — рабочий процесс с Pocket; при P=1 пул живёт в \
                 процессе приложения, при P>1 — в дочерних процессах. Воркер (W) = \
                 постоянный Pocket (набор ONNX-сессий), создаётся один раз на задание. \
                 Одновременные чанки (C) ограничивают параллелизм чанков текущего \
                 файла (файлов в обработке всегда максимум 1). ORT-потоки (T) — \
                 intra-op потоки одной ONNX-сессии.",
                );
        });
        ui.horizontal(|ui| {
            ui.label("Профиль:");
            let optimal = ui
                .button("Оптимальный")
                .on_hover_text("Баланс скорости, RAM/VRAM и времени первого результата");
            let productive = ui
                .button("Производительный")
                .on_hover_text("Приоритет общей скорости пакетной обработки (больше ресурсов)");
            if optimal.clicked() {
                cfg = ParallelSettings::preset_optimal(device, logical, precision);
            } else if productive.clicked() {
                cfg = ParallelSettings::preset_productive(device, logical, precision);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                secondary_label(ui, format!("Режим: {}", cfg.profile.label()));
            });
        });
        // правки полей выполняются в копиях (две колонки — два замыкания);
        // слияние и перевод профиля в «Свой» — после two_columns
        let (mut processes, mut wpp) = (cfg.processes, cfg.workers_per_process);
        let (mut files, mut threads_t) = (cfg.max_concurrent_chunks, cfg.ort_threads);
        let mut left_changed = false;
        let mut right_changed = false;
        let pockets_for_files = cfg.total_pockets();
        two_columns(
            ui,
            |ui| {
                combo_row(
                    ui,
                    "Процессов:",
                    "par_processes",
                    &processes.to_string(),
                    |ui| {
                        for p in 1..=pmax {
                            left_changed |= ui
                                .selectable_value(&mut processes, p, p.to_string())
                                .clicked();
                        }
                    },
                );
                combo_row(
                    ui,
                    "Воркеров на процесс:",
                    "par_workers",
                    &wpp.to_string(),
                    |ui| {
                        for w in 1..=wmax {
                            left_changed |=
                                ui.selectable_value(&mut wpp, w, w.to_string()).clicked();
                        }
                    },
                );
            },
            |ui| {
                combo_row(
                    ui,
                    "Одновременных чанков:",
                    "par_chunks",
                    &files.to_string(),
                    |ui| {
                        for f in 1..=pockets_for_files {
                            right_changed |=
                                ui.selectable_value(&mut files, f, f.to_string()).clicked();
                        }
                    },
                )
                .on_hover_text(
                    "Верхний предел чанков текущего файла, синтезируемых одновременно \
                     (min(C, P×W, оставшихся чанков)). C не создаёт дополнительные \
                     Pocket — только ограничивает параллелизм. Файлов в обработке \
                     всегда не более одного.",
                );
                combo_row(
                    ui,
                    "Потоков ORT:",
                    "par_threads",
                    &threads_t.to_string(),
                    |ui| {
                        for t in 1..=16u32 {
                            right_changed |= ui
                                .selectable_value(&mut threads_t, t, t.to_string())
                                .clicked();
                        }
                    },
                )
                .on_hover_text(
                    "Intra-op потоки ONNX Runtime на одну ONNX-сессию (Pocket). \
                     Изменение потоков ORT может изменить побитовый результат синтеза.",
                );
            },
        );
        if left_changed || right_changed {
            cfg.processes = processes;
            cfg.workers_per_process = wpp;
            cfg.max_concurrent_chunks = files;
            cfg.ort_threads = threads_t;
            edited(&mut cfg);
        }
        // компактная конфигурация + оценка ресурсов по ПЛАНИРУЕМЫМ к
        // загрузке Pocket (P=1 → min(W,C); P>1 → P×W) — G6.7.1. VRAM —
        // единая формула префлайта (settings::vram_need_mb, МБ); перевод
        // MB → GB выполняется ровно один раз, здесь
        ui.horizontal(|ui| {
            secondary_label(ui, cfg.summary_line());
            let pockets = cfg.planned_pockets();
            match device {
                DeviceChoice::Cuda => {
                    let need_mb = crate::settings::vram_need_mb(pockets, cfg.processes > 1);
                    secondary_label(
                        ui,
                        format!("· ~{:.1} ГБ VRAM (оценка)", need_mb as f64 / 1000.0),
                    );
                }
                DeviceChoice::Cpu => {
                    let per = match precision {
                        shtorm_engine::ModelPrecision::Fp32 => 1.1,
                        shtorm_engine::ModelPrecision::Int8 => 0.7,
                    };
                    secondary_label(
                        ui,
                        format!("· ~{:.1} ГБ RAM (приблизительно)", pockets as f64 * per),
                    );
                }
            }
        })
        .response
        .on_hover_text(
            "P — процессы, W — Pocket на процесс, C — одновременно синтезируемых \
             чанков текущего файла, T — потоки ORT на сессию. Каталог: файлы строго \
             последовательны, чанки файла параллельны. Одиночный TXT — тот же пул.",
        );
        if cfg.ort_threads != 4 {
            hint(
                ui,
                "Изменение потоков ORT может изменить побитовый результат синтеза",
            );
        }
    });
    // запись обратно в независимую конфигурацию устройства
    s.set_parallel_config(device, cfg);
}

/// Ширины (поле, кнопка) для строки пути: сумма + отступ не превышает
/// доступную ширину. Вынесено для юнит-теста адаптивности.
fn path_widths(available: f32, spacing: f32) -> (f32, f32) {
    let field = (available - BROWSE_BUTTON_WIDTH - spacing).max(60.0);
    (field, BROWSE_BUTTON_WIDTH)
}

/// Строка пути: поле забирает доступную ширину, «Обзор…» всегда остаётся
/// внутри строки. Раскладка LTR с явными ширинами (right_to_left в
/// вложенных UI не гарантировал резерв под кнопку).
fn path_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    pick: impl FnOnce() -> Option<PathBuf>,
) {
    ui.horizontal(|ui| {
        label_cell(ui, label);
        let avail = ui.available_width();
        let (field_w, button_w) = path_widths(avail, ui.spacing().item_spacing.x);
        // desired_width(INFINITY) здесь НЕЛЬЗЯ: TextEdit растягивался до
        // ширины всей строки, выталкивая «Обзор…» за правый край. Ровно
        // выделенная add_sized ширина — единственный источник размера.
        ui.add_sized([field_w, ROW_HEIGHT], egui::TextEdit::singleline(value));
        if ui
            .add_sized([button_w, ROW_HEIGHT], egui::Button::new("Обзор…"))
            .clicked()
        {
            if let Some(p) = pick() {
                *value = p.display().to_string();
            }
        }
    });
}

/// Строка чекбокса с явной раскладкой: иконка в фиксированной ячейке,
/// подпись — отдельный виджет. Внутри `ui.columns` (justified-колонка)
/// стандартный `ui.checkbox` растягивался на всю ширину колонки, и подпись
/// рисовалась поверх иконки (визуальная приёмка G6.4). Здесь иконка и текст
/// никогда не пересекаются, клик по подписи тоже переключает значение.
fn checkbox_row(ui: &mut egui::Ui, checked: &mut bool, label: &str) {
    let _ = checkbox_row_resp(ui, checked, label);
}

/// Та же строка, но с rect'ами иконки и подписи — для регрессионного теста
/// «иконка не наезжает на подпись».
fn checkbox_row_resp(
    ui: &mut egui::Ui,
    checked: &mut bool,
    label: &str,
) -> (egui::Response, egui::Response) {
    ui.horizontal(|ui| {
        let icon_cell = ui.spacing().icon_width + ui.spacing().icon_spacing;
        ui.spacing_mut().interact_size.x = icon_cell;
        let icon = ui.checkbox(checked, "");
        let text = ui
            .add(
                egui::Label::new(egui::RichText::new(label).size(BODY_SIZE))
                    .selectable(false)
                    .truncate()
                    .sense(egui::Sense::click()),
            )
            .on_hover_text(label);
        if text.clicked() {
            *checked = !*checked;
        }
        (icon, text)
    })
    .inner
}

/// Две адаптивные колонки: при недостатке ширины правая часть переходит под
/// левую (одна колонка, та же логическая группировка).
fn two_columns(
    ui: &mut egui::Ui,
    left: impl FnOnce(&mut egui::Ui),
    right: impl FnOnce(&mut egui::Ui),
) {
    if uses_two_columns(ui.available_width()) {
        ui.columns(2, |cols| {
            left(&mut cols[0]);
            right(&mut cols[1]);
        });
    } else {
        left(ui);
        ui.add_space(8.0);
        right(ui);
    }
}

impl ShtormApp {
    /// G6.8: один кадр UI для обоих рантаймов (eframe/glow и программный
    /// sw). Выделен из `eframe::App::update` (миграция egui 0.34: рантаймы
    /// передают корневой `Ui`, контекст берётся из него); принимает только
    /// egui-контекст — платформенно-зависимой части (eframe::Frame) в кадре
    /// нет, поэтому логика не дублируется.
    // egui 0.34 объявил deprecated панели/`screen_rect` на контексте
    // (замена — `show_inside`/`content_rect`); миграция компоновки G6.4 не
    // входит в этап G6.8 — те же API работают, поведение не изменилось.
    #[allow(deprecated)]
    pub(crate) fn update_ui(&mut self, ctx: &egui::Context) {
        // G5: тема применяется только при изменении настройки, не каждый кадр.
        if self.theme_applied != self.settings.theme_dark {
            ctx.set_visuals(if self.settings.theme_dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            });
            self.theme_applied = self.settings.theme_dark;
        }
        // G6.4: единая типографика и отступы — один раз, после применения
        // темы (configure_style клонирует текущий Style с её visuals).
        if !self.style_applied {
            configure_style(ctx);
            self.style_applied = true;
        }
        // события фонового задания (прогресс/логи/итог) — каждый кадр
        self.poll_job();

        // G6.2.3: закрытие окна — машина состояний (закрытие при живом
        // задании: отмена + удержание окна до завершения потока — синк
        // дропается в потоке задания, сиротских ffmpeg не остаётся; затем
        // автосохранение настроек и фактическое закрытие).
        self.handle_close_flow(ctx);
        // пока задание живо или закрытие в прогрессе — регулярные
        // перерисовки (дренаж событий и переходы состояний закрытия)
        if self.job.is_some() || self.close_flow != CloseFlow::Idle {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        // G6.4: при смене монитора/разрешения/DPI окно остаётся доступным.
        // Однократно (ключ монитора), не мешает ручному ресайзу.
        self.adapt_to_monitor(ctx);

        // Валидация — один раз за кадр (ffmpeg-проверка кэшируется внутри).
        let errors = self.validate();
        let job_running = self.job.is_some();

        // ---- верхняя область: название + краткий статус загрузки настроек ----
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("PocketTTS-GUI-Rust v1.0")
                        .heading()
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (badge, color) = match self.settings_load {
                        SettingsLoadState::Loaded => ("настройки загружены", None),
                        SettingsLoadState::FirstRun => ("настройки по умолчанию", None),
                        SettingsLoadState::Unreadable => {
                            ("⚠ настройки не прочитаны", Some(warn_color(ui)))
                        }
                    };
                    let text = match color {
                        Some(c) => egui::RichText::new(badge).small().color(c),
                        None => egui::RichText::new(badge)
                            .small()
                            .color(secondary_text_color(ui)),
                    };
                    ui.add(egui::Label::new(text).truncate())
                        .on_hover_text(&self.load_note);
                });
            });
            ui.add_space(1.0);
        });

        // ---- нижняя закреплённая область: журнал (самая нижняя полоса) ----
        let screen_h = ctx.screen_rect().height();
        // На очень низком окне журнал автоматически сворачивается: форма
        // важнее. Пользовательский выбор log_open сохраняется в сессии.
        let log_max = if screen_h < 460.0 {
            0.0
        } else {
            (screen_h * 0.35).clamp(90.0, 420.0)
        };
        let log_visible = self.log_open && log_max >= 40.0;
        let content_h = if log_visible {
            self.log_height.clamp(LOG_MIN_HEIGHT, log_max)
        } else {
            0.0
        };
        let log_panel_h = LOG_HEADER_HEIGHT
            + if log_visible {
                LOG_HANDLE_HEIGHT + content_h
            } else {
                0.0
            }
            + 6.0;

        egui::TopBottomPanel::bottom("log")
            .exact_height(log_panel_h)
            .show(ctx, |ui| {
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                if log_visible {
                    // Регулируемая высота: тянем верхнюю кромку журнала.
                    let handle = ui.allocate_response(
                        egui::vec2(ui.available_width(), LOG_HANDLE_HEIGHT),
                        egui::Sense::drag(),
                    );
                    if handle.hovered() {
                        ctx.set_cursor_icon(egui::CursorIcon::ResizeVertical);
                    }
                    if handle.dragged() {
                        self.log_height = (self.log_height - handle.drag_delta().y)
                            .clamp(LOG_MIN_HEIGHT, log_max);
                    }
                    ui.painter().hline(
                        handle.rect.x_range(),
                        handle.rect.center().y,
                        ui.visuals().widgets.noninteractive.bg_stroke,
                    );
                }
                ui.horizontal(|ui| {
                    let arrow = if log_visible { "▼" } else { "▲" };
                    if ui
                        .small_button(arrow)
                        .on_hover_text(if log_visible {
                            "Свернуть журнал"
                        } else {
                            "Развернуть журнал"
                        })
                        .clicked()
                    {
                        self.log_open = !self.log_open;
                    }
                    ui.label(egui::RichText::new("Журнал выполнения").strong());
                    if self.log_open && !log_visible {
                        secondary_label(ui, "скрыт: недостаточно высоты окна");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Очистить").clicked() {
                            // очищается только отображаемая копия; файлы журналов
                            // (диагностика/лог чанков) на диске не удаляются
                            self.log.clear();
                            self.status = "журнал очищен (файлы на диске не тронуты)".into();
                        }
                        if ui.button("Копировать").clicked() {
                            if self.log.is_empty() {
                                self.status = "журнал пуст — копировать нечего".into();
                            } else {
                                ui.ctx().copy_text(self.log.clone());
                                self.status = "журнал скопирован в буфер обмена".into();
                            }
                        }
                        if ui.available_width() > 220.0 {
                            if let Some(p) = &self.last_saved {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("сохранено: {}", p.display()))
                                            .small()
                                            .color(secondary_text_color(ui)),
                                    )
                                    .truncate(),
                                );
                            }
                        }
                    });
                });
                if log_visible {
                    // Собственная прокрутка журнала; колесо над журналом не
                    // конфликтует с прокруткой формы (это разные панели).
                    // stick_to_bottom: авто-прокрутка вниз только пока
                    // пользователь сам находится внизу; чтение старых
                    // сообщений не сбрасывается.
                    egui::ScrollArea::vertical()
                        .id_salt("log_scroll")
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if self.log.is_empty() {
                                secondary_label(ui, "пока пусто");
                            } else {
                                ui.add(
                                    egui::TextEdit::multiline(&mut self.log)
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(4)
                                        .font(egui::TextStyle::Monospace)
                                        .interactive(false),
                                );
                            }
                        });
                }
            });

        // ---- нижняя закреплённая область: действия, статус, прогресс ----
        egui::TopBottomPanel::bottom("actions").show(ctx, |ui| {
            ui.add_space(2.0);
            // horizontal_wrapped: на узком окне кнопки переносятся на вторую
            // строку, а не уезжают за край.
            ui.horizontal_wrapped(|ui| {
                let run = ui.add_enabled(
                    errors.is_empty() && !job_running,
                    egui::Button::new(egui::RichText::new("Запустить синтез").strong())
                        .min_size(egui::vec2(160.0, 26.0)),
                );
                if run.clicked() {
                    self.start_job(ctx);
                }
                if job_running {
                    // G5.1/G6.1: «Стоп» отключается, только когда отменять
                    // нечего — в пакете все активные файлы уже в фазах
                    // Finalizing/Publishing (их публикация необратима),
                    // в одиночном режиме — после точки решения.
                    let stop = ui
                        .add_enabled(
                            self.stop_available(),
                            egui::Button::new("Стоп")
                                .min_size(egui::vec2(72.0, 26.0))
                                .fill(egui::Color32::from_rgb(150, 45, 45)),
                        )
                        .on_disabled_hover_text(
                            "идёт сохранение результата — отмена уже не может предотвратить \
                             замену файла; кнопка вернётся после завершения",
                        );
                    if stop.clicked() {
                        if let Some(job) = &self.job {
                            job.cancel.store(true, Ordering::SeqCst);
                            self.status = "отмена…".into();
                        }
                    }
                }
                if ui
                    .add(egui::Button::new("Сохранить настройки").min_size(egui::vec2(0.0, 26.0)))
                    .clicked()
                {
                    // общий метод (используется и тестами) — логика состояния
                    // загрузки не дублируется
                    let _ = self.manual_save();
                }
                // G5: переключение темы сохраняется вместе с настройками.
                ui.checkbox(&mut self.settings.theme_dark, "Тёмная тема");
                ui.add_space(10.0);
                if errors.is_empty() {
                    ui.label(
                        egui::RichText::new("✓ параметры валидны")
                            .small()
                            .color(ok_color(ui)),
                    );
                } else {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("⚠ {} проблем(ы)", errors.len()))
                                .small()
                                .color(warn_color(ui)),
                        )
                        .truncate(),
                    )
                    .on_hover_text(errors.join("\n"));
                }
            });

            // Рабочий статус (ход задания / сохранения) — отдельно от статуса
            // загрузки конфига в верхней панели.
            if !self.status.is_empty() {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&self.status)
                            .small()
                            .color(secondary_text_color(ui)),
                    )
                    .truncate(),
                )
                .on_hover_text(&self.status);
            }

            // Пакетный режим (G6.1): общий прогресс по файлам K/N + активные
            // файлы пула (каждый со своим прогрессом чанков и фазой);
            // одиночный режим — один бар по чанкам.
            if self.batch_running {
                if let Some(bs) = &self.batch_state {
                    if bs.total > 0 {
                        let frac = bs.finished as f32 / bs.total as f32;
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .text(format!(
                                    "файлы {}/{} (готово {} | ошибок {} | пропущено {})",
                                    bs.finished, bs.total, bs.completed, bs.failed, bs.skipped
                                ))
                                .show_percentage(),
                        );
                        if !bs.active.is_empty() {
                            let h = (screen_h * 0.06).clamp(24.0, 48.0);
                            egui::ScrollArea::vertical()
                                .id_salt("batch_active")
                                .max_height(h)
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    for f in &bs.active {
                                        let phase = match f.phase {
                                            JobPhase::Generating => "генерация",
                                            JobPhase::Finalizing | JobPhase::Publishing => {
                                                "сохранение"
                                            }
                                        };
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(format!(
                                                    "[{}/{}] {} — чанки {}/{} ({})",
                                                    f.index,
                                                    bs.total,
                                                    f.name,
                                                    f.done,
                                                    f.total,
                                                    phase
                                                ))
                                                .small(),
                                            )
                                            .truncate(),
                                        );
                                    }
                                });
                        }
                    }
                }
            } else if job_running || self.progress.1 > 0 {
                let (done, total) = self.progress;
                let frac = if total > 0 {
                    done as f32 / total as f32
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .text(format!("{done}/{total} чанков"))
                        .show_percentage(),
                );
            }

            // Сообщения валидации — в ограниченной по высоте области: большое
            // их количество не вытесняет прокручиваемую форму.
            if !errors.is_empty() {
                let h = (screen_h * 0.08).clamp(26.0, 56.0);
                egui::ScrollArea::vertical()
                    .id_salt("validation_errors")
                    .max_height(h)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        // Жёсткая ширина переноса: длинный путь в тексте
                        // ошибки не расширяет внутренний UI ScrollArea и не
                        // обрезается правым краем панели.
                        let wrap_w = (ui.ctx().screen_rect().width() - 32.0).max(200.0);
                        ui.set_max_width(wrap_w);
                        for e in &errors {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!("• {}", soft_wrap_paths(e)))
                                        .small()
                                        .color(warn_color(ui)),
                                )
                                .wrap(),
                            );
                        }
                    });
            }
        });

        // ---- центральная прокручиваемая область: вся форма настроек ----
        // Форма живёт в ЖЁСТКО ограниченном слоте content_w: без этого
        // вложенные горизонтальные строки расширяли содержимое ScrollArea
        // кадр от кадра, форма становилась шире панели, а кнопки «Обзор…»
        // уезжали за правый край (обнаружено в G6.4 визуальной приёмкой).
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
            let avail = ui.available_size();
            // 18 точек — зазор под вертикальную полосу прокрутки и края.
            let content_w = (avail.x - 18.0).clamp(160.0, FORM_MAX_WIDTH);
            let pad = ((avail.x - content_w) / 2.0).max(0.0);
            ui.horizontal_top(|ui| {
                ui.add_space(pad);
                ui.allocate_ui_with_layout(
                    egui::vec2(content_w, avail.y),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(content_w);
                        egui::ScrollArea::vertical()
                            .id_salt("form_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                // полоса прокрутки внутри слота — ещё 12 точек
                                ui.set_max_width(content_w - 12.0);
                                self.form_ui(ui);
                            });
                    },
                );
            });
        });

        // G6.2.3: диалог ошибки автосохранения — поверх панелей
        self.show_autosave_dialog(ctx);
    }

    /// G6.2.3: страховка на случай выхода МИМО штатного потока закрытия
    /// (close_flow остался Idle) — лучшая попытка автосохранения. Показать
    /// ошибку пользователю уже некому: политика/снимок соблюдаются те же,
    /// отказ фиксируется в stderr (при перенаправленных потоках — в журнале
    /// скрипта). Основной путь — handle_close_flow, на Drop не полагаемся.
    /// G6.8: метод общий для обоих рантаймов (eframe on_exit и sw-цикл).
    pub(crate) fn on_exit_save(&mut self) {
        if self.close_flow != CloseFlow::Idle {
            return; // штатный поток закрытия уже сохранил/осознанно пропустил
        }
        if self.settings_load == SettingsLoadState::Unreadable {
            return;
        }
        self.settings.clamped();
        let current = serde_json::to_string_pretty(&self.settings).unwrap_or_default();
        if current != self.settings_snapshot {
            if let Err(e) = save_settings(&self.settings, &self.conf_path) {
                eprintln!("автосохранение при выходе не выполнено: {e:#}");
            }
        }
    }
}

/// G6.8: рантайм eframe/glow — только в EXE PocketTTS-GUI-Rust (feature
/// `glow`); программный EXE вообще не линкует eframe/OpenGL.
#[cfg(feature = "glow")]
impl eframe::App for ShtormApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.update_ui(ui.ctx());
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.on_exit_save();
    }
}

#[cfg(test)]
// egui 0.34: в headless-тестах панели на контексте (CentralPanel::show и
// т.п.) объявлены deprecated, но остаются штатным способом сборки кадра
// без рантайма — миграция компоновки не входит в G6.8.
#[allow(deprecated)]
mod tests {
    use super::*;

    /// G5.2 (баг визуального контроля): при старте тема обязана примениться
    /// на первом кадре — theme_applied начинается «не применённой»,
    /// иначе update() не вызовет set_visuals и окно откроется светлым
    /// даже при theme_dark=true.
    #[test]
    fn first_frame_must_apply_theme() {
        let app = ShtormApp::new();
        assert_ne!(
            app.theme_applied, app.settings.theme_dark,
            "первый кадр обязан применить тему из настроек"
        );
    }

    /// Управляемая свёртка: поле настроек — единственный источник истины.
    /// 1) открытое поле → тело показано; закрытое поле → тело скрыто.
    /// 2) клик по заголовку (симуляция PointerButton по rect) переворачивает
    ///    поле — состояние переживает кадры и попадает в конфиг.
    #[test]
    fn header_state_is_driven_by_settings_field() {
        let ctx = egui::Context::default();
        // без анимации: openness мгновенно 0/1 — тело строго соответствует
        // принудительному .open() даже в разреженных headless-пассах
        ctx.style_mut(|s| s.animation_time = 0.0);
        let mut open = true;
        let mut header_rect: Option<egui::Rect> = None;
        for frame in 0..3 {
            let mut raw = egui::RawInput::default();
            if frame == 1 {
                // «пользователь» кликает центр заголовка первого кадра
                let pos = header_rect.expect("rect заголовка").center();
                raw.events.push(egui::Event::PointerMoved(pos));
                raw.events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                });
                raw.events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            let _ = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui.ctx(), |ui| {
                    let was_open = open;
                    let resp = egui::CollapsingHeader::new("Тест-секция")
                        .open(Some(open))
                        .show(ui, |ui| {
                            ui.label("тело секции");
                        });
                    header_rect = Some(resp.header_response.rect);
                    header_clicked(&resp, &mut open);
                    // в этом кадре тело показано <=> поле было открыто
                    assert_eq!(resp.body_returned.is_some(), was_open);
                });
            });
            match frame {
                0 => assert!(open, "стартовое состояние — открыто"),
                1 => assert!(!open, "клик по заголовку закрыл секцию"),
                _ => assert!(!open, "состояние сохраняется между кадрами"),
            }
        }
    }

    /// Мок-состояния UI задания: прогресс обновляется, Completed/Cancelled/
    /// Failed попадают в статус и журнал (без запуска синтеза).
    #[test]
    fn job_events_update_ui_state() {
        let mut app = ShtormApp::new();
        app.apply_event(crate::job::JobEvent::Progress { done: 3, total: 7 });
        assert_eq!(app.progress, (3, 7));
        assert!(app.status.contains("3/7"), "{}", app.status);

        app.apply_event(crate::job::JobEvent::Finished(Box::new(
            crate::job::JobResult {
                status: crate::job::JobStatus::Cancelled,
                summary: "ОТМЕНЕНО на 3/7 чанках; итоговый файл не создан; черновик: C:/tmp/x.partial.wav".into(),
                final_path: None,
                partial_path: Some(PathBuf::from("C:/tmp/x.partial.wav")),
                diag_path: Some(PathBuf::from("C:/tmp/x.log.txt")),
                chunk_log_path: None,
                rtf: Some(0.5),
                progress: (3, 7),
                replaced_existing: false,
                batch: None,
            },
        )));
        assert!(app.status.starts_with("Отменено"), "{}", app.status);
        assert!(app.log.contains("ОТМЕНЕНО") && app.log.contains("x.partial.wav"));

        app.apply_event(crate::job::JobEvent::Finished(Box::new(
            crate::job::JobResult::failed("нет модели".into()),
        )));
        assert!(app.status.starts_with("Ошибка"), "{}", app.status);
        assert!(app.log.contains("нет модели"));

        // Completed: итоговый путь и RTF видны в журнале
        app.apply_event(crate::job::JobEvent::Finished(Box::new(
            crate::job::JobResult {
                status: crate::job::JobStatus::Completed,
                summary: "ГОТОВО: C:/out/book.wav (RTF 0.123)".into(),
                final_path: Some(PathBuf::from("C:/out/book.wav")),
                partial_path: None,
                diag_path: None,
                chunk_log_path: None,
                rtf: Some(0.123),
                progress: (7, 7),
                replaced_existing: false,
                batch: None,
            },
        )));
        assert!(app.status.starts_with("Завершено"), "{}", app.status);
        assert!(app.log.contains("book.wav"));

        // журнал ограничен (нет неограниченного роста)
        for i in 0..1000 {
            app.push_log(&format!("строка {i}"));
        }
        assert!(
            app.log.lines().count() <= 401,
            "{} строк",
            app.log.lines().count()
        );
    }

    /// Пакетные события: K/N файлов, счётчики и отдельно чанки активных
    /// файлов (G6.1: состояние per-file, не «последний пришедший»).
    #[test]
    fn batch_events_update_ui_state() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        app.apply_event(crate::job::JobEvent::BatchStarted {
            total: 5,
            workers: 1,
        });
        app.apply_event(crate::job::JobEvent::FileStarted {
            index: 2,
            total: 5,
            input: PathBuf::from("C:/src/sub/a.txt"),
            output: PathBuf::from("C:/out/sub/a.wav"),
        });
        app.apply_event(crate::job::JobEvent::FileProgress {
            index: 2,
            total: 5,
            done: 1,
            chunks_total: 4,
        });
        assert_eq!(app.file_progress, (2, 5));
        assert_eq!(app.progress, (1, 4));
        let bs = app.batch_state.as_ref().expect("состояние пакета");
        assert_eq!(bs.finished, 0);
        assert_eq!(bs.active.len(), 1);
        assert_eq!(bs.active[0].done, 1, "прогресс чанков файла 2");
        assert_eq!(bs.active[0].total, 4);

        let mut report = crate::batch::FileReport::new(
            &crate::batch::BatchFile {
                input: PathBuf::from("C:/src/sub/a.txt"),
                relative: PathBuf::from("sub/a.txt"),
            },
            Some(PathBuf::from("C:/out/sub/a.wav")),
        );
        report.status = crate::batch::FileStatus::Completed;
        app.apply_event(crate::job::JobEvent::FileDone {
            index: 2,
            total: 5,
            report: Box::new(report),
        });
        let bs = app.batch_state.as_ref().expect("состояние пакета");
        assert_eq!(bs.finished, 1, "K/N двигается на Completed");
        assert!(bs.active.is_empty(), "файл покинул активные");
        assert_eq!(bs.completed, 1);
        assert!(app.status.contains("1/5"), "{}", app.status);
        assert!(app.log.contains("sub/a.txt") && app.log.contains("sub/a.wav"));
    }

    /// G6.1: параллельные файлы — события приходят ВПЕРЕМЕШКУ, состояние
    /// каждого файла независимо; завершение в другом порядке не путает
    /// счётчики; итог считается по всем файлам.
    #[test]
    fn parallel_batch_interleaved_events_keep_per_file_state() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        app.apply_event(crate::job::JobEvent::BatchStarted {
            total: 3,
            workers: 2,
        });
        // стартовали файлы 1 и 2 (параллельно)
        for k in [1, 2] {
            app.apply_event(crate::job::JobEvent::FileStarted {
                index: k,
                total: 3,
                input: PathBuf::from(format!("C:/src/f{k}.txt")),
                output: PathBuf::from(format!("C:/out/f{k}.wav")),
            });
        }
        // прогресс вперемешку: файл 2 опережает файл 1
        for (k, done) in [(2, 1), (2, 2), (1, 1)] {
            app.apply_event(crate::job::JobEvent::FileProgress {
                index: k,
                total: 3,
                done,
                chunks_total: 5,
            });
        }
        {
            let bs = app.batch_state.as_ref().unwrap();
            let f1 = bs.active.iter().find(|f| f.index == 1).unwrap();
            let f2 = bs.active.iter().find(|f| f.index == 2).unwrap();
            assert_eq!(
                (f1.done, f1.total),
                (1, 5),
                "файл 1 не перезаписан файлом 2"
            );
            assert_eq!(
                (f2.done, f2.total),
                (2, 5),
                "файл 2 не перезаписан файлом 1"
            );
        }
        // фазы вперемешку: файл 2 ушёл в публикацию, файл 1 генерируется
        app.apply_event(crate::job::JobEvent::FilePhase {
            index: 2,
            phase: JobPhase::Publishing,
        });
        assert!(app.stop_available(), "файл 1 ещё отменяем — Стоп доступен");
        app.apply_event(crate::job::JobEvent::FilePhase {
            index: 1,
            phase: JobPhase::Finalizing,
        });
        assert!(
            !app.stop_available(),
            "все активные файлы в фазах сохранения — Стоп отключён"
        );
        // завершение в обратном порядке: файл 2 готов, файл 1 отменён
        let done2 = {
            let mut r = crate::batch::FileReport::new(
                &crate::batch::BatchFile {
                    input: PathBuf::from("C:/src/f2.txt"),
                    relative: PathBuf::from("f2.txt"),
                },
                Some(PathBuf::from("C:/out/f2.wav")),
            );
            r.status = crate::batch::FileStatus::Completed;
            r
        };
        app.apply_event(crate::job::JobEvent::FileDone {
            index: 2,
            total: 3,
            report: Box::new(done2),
        });
        // файл 1 всё ещё в Finalizing → отмена ничего не может — Стоп недоступен
        assert!(!app.stop_available());
        let done1 = {
            let mut r = crate::batch::FileReport::new(
                &crate::batch::BatchFile {
                    input: PathBuf::from("C:/src/f1.txt"),
                    relative: PathBuf::from("f1.txt"),
                },
                Some(PathBuf::from("C:/out/f1.wav")),
            );
            r.status = crate::batch::FileStatus::Cancelled;
            r
        };
        app.apply_event(crate::job::JobEvent::FileDone {
            index: 1,
            total: 3,
            report: Box::new(done1),
        });
        let bs = app.batch_state.as_ref().unwrap();
        assert_eq!(bs.finished, 2);
        assert_eq!((bs.completed, bs.cancelled), (1, 1));
        assert!(bs.active.is_empty());
        assert!(
            app.stop_available(),
            "активных файлов нет — Стоп (между файлами)"
        );
    }

    // ===== G6.1.1: регрессионные тесты пакетного состояния (Test A–F) =====

    fn file_report(idx: usize, status: FileStatus, reason: &str) -> crate::batch::FileReport {
        let mut r = crate::batch::FileReport::new(
            &crate::batch::BatchFile {
                input: PathBuf::from(format!("C:/src/f{idx}.txt")),
                relative: PathBuf::from(format!("f{idx}.txt")),
            },
            Some(PathBuf::from(format!("C:/out/f{idx}.wav"))),
        );
        r.status = status;
        r.reason = Some(reason.into());
        r
    }

    fn apply_file_done(app: &mut ShtormApp, index: usize, total: usize, status: FileStatus) {
        app.apply_event(crate::job::JobEvent::FileDone {
            index,
            total,
            report: Box::new(file_report(index, status, "мок")),
        });
    }

    /// Test A — ранние Skipped preflight учитываются до загрузки пула;
    /// BatchWorkersReady обновляет только число воркеров, БЕЗ сброса
    /// счётчиков и без повторного создания состояния.
    #[test]
    fn batch_early_skipped_survive_workers_ready() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        // порядок событий — в точности как в задании G6.1.1 §4 (Test A)
        app.apply_event(crate::job::JobEvent::BatchStarted {
            total: 5,
            workers: 0,
        });
        apply_file_done(&mut app, 1, 5, FileStatus::Skipped);
        apply_file_done(&mut app, 3, 5, FileStatus::Skipped);
        {
            let bs = app.batch_state.as_ref().expect("состояние пакета");
            assert_eq!((bs.finished, bs.skipped), (2, 2), "ранние Skipped учтены");
            assert_eq!(bs.workers, 0, "воркеры ещё не готовы");
        }
        app.apply_event(crate::job::JobEvent::BatchWorkersReady { workers: 2 });
        {
            let bs = app.batch_state.as_ref().expect("состояние пакета");
            assert_eq!(bs.workers, 2, "число воркеров обновлено");
            assert_eq!(
                (bs.finished, bs.skipped, bs.completed, bs.failed),
                (2, 2, 0, 0),
                "обновление воркеров не сбросило счётчики preflight"
            );
        }
        for k in [2, 4] {
            app.apply_event(crate::job::JobEvent::FileStarted {
                index: k,
                total: 5,
                input: PathBuf::from(format!("C:/src/f{k}.txt")),
                output: PathBuf::from(format!("C:/out/f{k}.wav")),
            });
        }
        apply_file_done(&mut app, 4, 5, FileStatus::Completed);
        apply_file_done(&mut app, 2, 5, FileStatus::Completed);
        apply_file_done(&mut app, 5, 5, FileStatus::Failed);
        let bs = app.batch_state.as_ref().expect("состояние пакета");
        assert_eq!(bs.total, 5);
        assert_eq!(bs.finished, 5);
        assert_eq!(bs.completed, 2);
        assert_eq!(bs.skipped, 2);
        assert_eq!(bs.failed, 1);
        assert!(bs.active.is_empty());
        assert_eq!(bs.workers, 2);
        assert!(
            app.status.contains("5/5") && app.status.contains("одновременно 2"),
            "{}",
            app.status
        );
    }

    /// Test B — весь пакет пропущен: все FileDone учтены, пул не загружался
    /// (workers остаётся 0), итог честно NoWork, а не «воркеры работают».
    #[test]
    fn batch_all_skipped_never_shows_running_workers() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        app.apply_event(crate::job::JobEvent::BatchStarted {
            total: 3,
            workers: 0,
        });
        for k in 1..=3 {
            apply_file_done(&mut app, k, 3, FileStatus::Skipped);
        }
        let bs = app.batch_state.as_ref().expect("состояние пакета");
        assert_eq!((bs.finished, bs.skipped), (3, 3), "все FileDone учтены");
        assert_eq!(
            bs.workers, 0,
            "Pocket не загружался — BatchWorkersReady не было"
        );
        assert!(
            app.status.contains("пропущено 3"),
            "GUI показывает число пропущенных: {}",
            app.status
        );
        assert!(
            app.status.contains("воркеры готовятся"),
            "ложного статуса работающих воркеров нет: {}",
            app.status
        );
        // итог NoWork по существующей семантике (не вводит в заблуждение)
        app.apply_event(crate::job::JobEvent::Finished(Box::new(
            crate::job::JobResult {
                status: JobStatus::NoWork,
                summary: "ПАКЕТ: готово 0 | ошибок 0 | пропущено 3 | всего 3 | НЕЧЕГО ДЕЛАТЬ"
                    .into(),
                final_path: None,
                partial_path: None,
                diag_path: None,
                chunk_log_path: None,
                rtf: None,
                progress: (3, 3),
                replaced_existing: false,
                batch: None,
            },
        )));
        assert!(app.status.starts_with("⚠ Нет работы"), "{}", app.status);
    }

    /// Test C — сбой загрузки пула: часть файлов пропущена на preflight,
    /// остальные Failed («пул не запущен»); счётчики UI полностью совпадают
    /// с BatchReport (сверка обоих наборов счётчиков).
    #[test]
    fn batch_pool_load_failure_counters_match_report() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        app.apply_event(crate::job::JobEvent::BatchStarted {
            total: 4,
            workers: 0,
        });
        apply_file_done(&mut app, 1, 4, FileStatus::Skipped);
        apply_file_done(&mut app, 2, 4, FileStatus::Skipped);
        // пул не запустился — оставшиеся кандидаты честно Failed
        app.apply_event(crate::job::JobEvent::Log(
            "ВНИМАНИЕ: пул не запущен: воркер 0: CUDA OOM".into(),
        ));
        apply_file_done(&mut app, 3, 4, FileStatus::Failed);
        apply_file_done(&mut app, 4, 4, FileStatus::Failed);
        app.apply_event(crate::job::JobEvent::Finished(Box::new(
            crate::job::JobResult::failed(
                "ПАКЕТ: готово 0 | ошибок 2 | пропущено 2 | всего 4 | ОШИБКА".into(),
            ),
        )));
        let bs = app.batch_state.as_ref().expect("состояние пакета");
        assert_eq!(bs.total, 4);
        assert_eq!(bs.finished, 4);
        assert_eq!((bs.completed, bs.failed, bs.skipped), (0, 2, 2));
        assert_eq!(
            bs.workers, 0,
            "пул не загрузился — UI не показывает работающих воркеров"
        );
        assert!(app.log.contains("пул не запущен"));
        assert!(app.status.starts_with("Ошибка"), "{}", app.status);
    }

    /// Test D — отмена до/во время preflight: новые файлы не стартуют,
    /// уже завершённые результаты сохранены, счётчики не сбрасываются,
    /// итог — Cancelled.
    #[test]
    fn batch_cancel_during_preflight_keeps_counters() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        app.apply_event(crate::job::JobEvent::BatchStarted {
            total: 3,
            workers: 0,
        });
        // preflight под отменой помечает всё Skipped «отменено до запуска»
        for k in 1..=3 {
            app.apply_event(crate::job::JobEvent::FileDone {
                index: k,
                total: 3,
                report: Box::new(crate::batch::FileReport::skipped(
                    &crate::batch::BatchFile {
                        input: PathBuf::from(format!("C:/src/f{k}.txt")),
                        relative: PathBuf::from(format!("f{k}.txt")),
                    },
                    Some(PathBuf::from(format!("C:/out/f{k}.wav"))),
                    "отменено до запуска",
                )),
            });
        }
        let bs = app.batch_state.as_ref().expect("состояние пакета");
        assert_eq!((bs.finished, bs.skipped), (3, 3));
        assert!(bs.active.is_empty(), "после отмены новые файлы не стартуют");
        assert_eq!(
            bs.cancelled, 0,
            "Skipped-отмена не считается Cancelled-файлом"
        );
        app.apply_event(crate::job::JobEvent::Finished(Box::new(
            crate::job::JobResult {
                status: JobStatus::Cancelled,
                summary: "ПАКЕТ: готово 0 | ошибок 0 | пропущено 3 | всего 3 | ОТМЕНЕНО".into(),
                final_path: None,
                partial_path: None,
                diag_path: None,
                chunk_log_path: None,
                rtf: None,
                progress: (3, 3),
                replaced_existing: false,
                batch: None,
            },
        )));
        assert!(app.status.starts_with("Отменено"), "{}", app.status);
        // счётчики уцелели после итога
        let bs = app.batch_state.as_ref().expect("состояние пакета");
        assert_eq!(bs.finished, 3, "итог не сбросил счётчики");
    }

    /// Test E — порядок событий: FileDone до загрузки Pocket не теряются,
    /// повторный BatchWorkersReady НЕ повторяет инициализацию (состояние и
    /// счётчики переживают его), а повторный FileDone того же индекса не
    /// учитывается дважды — с явной диагностикой в журнале (не молча).
    #[test]
    fn batch_workers_ready_is_not_reinit_and_duplicate_filedone_diagnosed() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        app.apply_event(crate::job::JobEvent::BatchStarted {
            total: 2,
            workers: 0,
        });
        apply_file_done(&mut app, 1, 2, FileStatus::Skipped);
        // повторное обновление воркеров (например, деградация 2→1) — только
        // поле workers, состояние то же самое
        app.apply_event(crate::job::JobEvent::BatchWorkersReady { workers: 2 });
        app.apply_event(crate::job::JobEvent::BatchWorkersReady { workers: 1 });
        {
            let bs = app.batch_state.as_ref().expect("состояние пакета");
            assert_eq!(bs.workers, 1);
            assert_eq!(
                (bs.finished, bs.skipped),
                (1, 1),
                "счётчики пережили повторное BatchWorkersReady"
            );
            assert_eq!(bs.total, 2);
        }
        // протокольная аномалия: тот же индекс завершён дважды — двойного
        // учёта нет, в журнале явная диагностика
        app.log.clear();
        apply_file_done(&mut app, 1, 2, FileStatus::Completed);
        {
            let bs = app.batch_state.as_ref().expect("состояние пакета");
            assert_eq!(bs.finished, 1, "дубль не учтён в finished");
            assert_eq!(bs.completed, 0, "дубль не учтён в completed");
            assert_eq!(bs.skipped, 1, "исходный статус файла 1 не изменён");
        }
        assert!(
            app.log.contains("не учтено дважды"),
            "диагностика дубля в журнале: {}",
            app.log
        );
        // BatchWorkersReady ДО BatchStarted — диагностируется, состояние не
        // создаётся молча
        let mut app2 = ShtormApp::new();
        app2.batch_running = true;
        app2.log.clear();
        app2.apply_event(crate::job::JobEvent::BatchWorkersReady { workers: 2 });
        assert!(
            app2.batch_state.is_none(),
            "состояние не создано вне BatchStarted"
        );
        assert!(app2.log.contains("протокол"), "{}", app2.log);
    }

    /// Test F (UI-часть) — последовательный путь: один BatchStarted
    /// (workers=0) → BatchWorkersReady(1) ДО preflight FileDone; одиночный
    /// синтез вообще не создаёт пакетного состояния.
    #[test]
    fn batch_sequential_single_started_then_single_file_mode() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        app.apply_event(crate::job::JobEvent::BatchStarted {
            total: 2,
            workers: 0,
        });
        app.apply_event(crate::job::JobEvent::BatchWorkersReady { workers: 1 });
        apply_file_done(&mut app, 1, 2, FileStatus::Skipped); // preflight
        app.apply_event(crate::job::JobEvent::FileStarted {
            index: 2,
            total: 2,
            input: PathBuf::from("C:/src/f2.txt"),
            output: PathBuf::from("C:/out/f2.wav"),
        });
        apply_file_done(&mut app, 2, 2, FileStatus::Completed);
        let bs = app.batch_state.as_ref().expect("состояние пакета");
        assert_eq!((bs.finished, bs.skipped, bs.completed), (2, 1, 1));
        assert_eq!(bs.workers, 1, "последовательный путь — один «воркер»");
        assert!(app.status.contains("одновременно 1"), "{}", app.status);

        // одиночный синтез (не пакет): пакетное состояние не затрагивается
        let mut solo = ShtormApp::new();
        solo.apply_event(crate::job::JobEvent::Progress { done: 1, total: 3 });
        solo.apply_event(crate::job::JobEvent::Phase(JobPhase::Finalizing));
        solo.apply_event(crate::job::JobEvent::Finished(Box::new(
            crate::job::JobResult::failed("тест-одиночный".into()),
        )));
        assert!(
            solo.batch_state.is_none(),
            "одиночный режим без пакетного состояния"
        );
    }

    /// G3.1: K/N обновляется на Failed/Skipped, а частичный успех и пустой
    /// пакет явно предупреждают пользователя.
    #[test]
    fn batch_failed_skipped_and_partial_states() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        app.apply_event(crate::job::JobEvent::BatchStarted {
            total: 5,
            workers: 1,
        });
        let file = crate::batch::BatchFile {
            input: PathBuf::from("C:/src/b.txt"),
            relative: PathBuf::from("b.txt"),
        };
        app.apply_event(crate::job::JobEvent::FileStarted {
            index: 2,
            total: 5,
            input: file.input.clone(),
            output: PathBuf::from("C:/out/b.wav"),
        });
        let mut failed = crate::batch::FileReport::failed(&file, None, "мок-сбой");
        failed.chunks_done = 1;
        failed.chunks_total = 3;
        app.apply_event(crate::job::JobEvent::FileDone {
            index: 2,
            total: 5,
            report: Box::new(failed),
        });
        assert_eq!(app.file_progress, (2, 5), "K/N обновился на Failed");
        assert_eq!(app.progress, (1, 3));
        let bs = app.batch_state.as_ref().unwrap();
        assert_eq!(bs.failed, 1);
        assert!(app.status.contains("ошибок 1"), "{}", app.status);

        let skipped = crate::batch::FileReport::skipped(&file, None, "конфликт");
        app.apply_event(crate::job::JobEvent::FileDone {
            index: 3,
            total: 5,
            report: Box::new(skipped),
        });
        assert_eq!(app.file_progress, (3, 5), "K/N обновился на Skipped");

        app.apply_event(crate::job::JobEvent::Finished(Box::new(
            crate::job::JobResult {
                status: JobStatus::Partial,
                summary: "ПАКЕТ: готово 1 | ошибок 1 | ЧАСТИЧНЫЙ УСПЕХ".into(),
                final_path: None,
                partial_path: None,
                diag_path: None,
                chunk_log_path: None,
                rtf: None,
                progress: (5, 5),
                replaced_existing: false,
                batch: None,
            },
        )));
        assert!(app.status.starts_with("⚠ Частично"), "{}", app.status);
        assert!(app.log.contains("ЧАСТИЧНЫЙ УСПЕХ"));

        app.apply_event(crate::job::JobEvent::Finished(Box::new(
            crate::job::JobResult {
                status: JobStatus::NoWork,
                summary: "ПАКЕТ: НЕЧЕГО ДЕЛАТЬ".into(),
                final_path: None,
                partial_path: None,
                diag_path: None,
                chunk_log_path: None,
                rtf: None,
                progress: (0, 0),
                replaced_existing: false,
                batch: None,
            },
        )));
        assert!(app.status.starts_with("⚠ Нет работы"), "{}", app.status);
    }

    /// G5.1: фазы задания управляют доступностью «Стоп». Generating —
    /// кнопка доступна; Finalizing/Publishing — публикация необратима,
    /// кнопка отключена и статус показывает сохранение результата;
    /// Finished/FileDone возвращают кнопку.
    #[test]
    fn job_phases_drive_stop_availability_and_status() {
        let mut app = ShtormApp::new();
        app.apply_event(crate::job::JobEvent::Phase(JobPhase::Generating));
        assert!(!app.publishing, "во время генерации «Стоп» доступен");
        assert!(app.status.contains("генерация"), "{}", app.status);

        app.apply_event(crate::job::JobEvent::Phase(JobPhase::Finalizing));
        assert!(app.publishing, "после точки решения «Стоп» отключается");
        assert!(
            app.status.contains("Сохранение результата"),
            "{}",
            app.status
        );
        app.apply_event(crate::job::JobEvent::Phase(JobPhase::Publishing));
        assert!(app.publishing);
        assert!(app.status.contains("Сохранение результата"));

        // пакетный режим: после готового файла кнопка возвращается
        let report = crate::batch::FileReport::new(
            &crate::batch::BatchFile {
                input: PathBuf::from("C:/src/a.txt"),
                relative: PathBuf::from("a.txt"),
            },
            Some(PathBuf::from("C:/out/a.wav")),
        );
        app.apply_event(crate::job::JobEvent::FileDone {
            index: 1,
            total: 2,
            report: Box::new(report),
        });
        assert!(
            !app.publishing,
            "между файлами пакета «Стоп» снова доступен"
        );

        // итог задания тоже возвращает кнопку
        app.apply_event(crate::job::JobEvent::Phase(JobPhase::Publishing));
        assert!(app.publishing);
        app.apply_event(crate::job::JobEvent::Finished(Box::new(
            crate::job::JobResult::failed("тест".into()),
        )));
        assert!(!app.publishing, "после итога «Стоп» не нужен");
    }

    // ===== G6.2.3: автосохранение при закрытии (headless-машина состояний) =====

    /// Изолированная песочница с путём конфига (settings_path() в тестах
    /// указывала бы на каталог тестового харнесса).
    fn close_sandbox(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "pockettts_close_{}_{}_{}",
            std::process::id(),
            tag,
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join(crate::settings::SETTINGS_FILE)
    }

    /// Один headless-кадр машины закрытия: close_requested подаётся через
    /// RawInput (ViewportEvent::Close в ROOT), возвращаются viewport-команды.
    fn close_frame(app: &mut ShtormApp, close_requested: bool) -> Vec<egui::ViewportCommand> {
        let ctx = egui::Context::default();
        let mut info = egui::ViewportInfo::default();
        if close_requested {
            info.events.push(egui::ViewportEvent::Close);
        }
        let mut raw = egui::RawInput::default();
        raw.viewports.insert(egui::ViewportId::ROOT, info);
        let full = ctx.run_ui(raw, |ui| app.handle_close_flow(ui.ctx()));
        full.viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|o| o.commands.clone())
            .unwrap_or_default()
    }

    fn has_cancel_close(cmds: &[egui::ViewportCommand]) -> bool {
        cmds.iter()
            .any(|c| matches!(c, egui::ViewportCommand::CancelClose))
    }

    fn has_close(cmds: &[egui::ViewportCommand]) -> bool {
        cmds.iter()
            .any(|c| matches!(c, egui::ViewportCommand::Close))
    }

    /// A1/A2/A3 (ядро): закрытие без задания сохраняет изменённые настройки
    /// (модель/устройство/пакетный CUDA/тема/свёртки/неизвестные ключи),
    /// повторная загрузка возвращает их.
    #[test]
    fn close_autosaves_changed_settings_and_reload_restores() {
        let conf = close_sandbox("changed");
        let mut app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, crate::app::SettingsLoadState::FirstRun);
        app.settings.temperature = 0.65;
        app.settings.model = ModelChoice::Fast;
        app.settings.device = DeviceChoice::Cuda;
        app.settings.parallel_cuda = crate::settings::ParallelSettings {
            profile: crate::settings::ParallelProfile::Productive,
            processes: 2,
            workers_per_process: 2,
            max_concurrent_chunks: 4,
            ort_threads: 4,
        };
        app.settings.theme_dark = false;
        app.settings.pauses_open = false;
        // A6: неизвестный ключ не теряется автосохранением
        app.settings
            .extra
            .insert("future_key".into(), serde_json::json!({"x": 1}));

        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds), "кадр закрытия отменяет OS-close");
        assert_eq!(app.close_flow, crate::app::CloseFlow::Closing);
        assert!(has_close(&cmds), "фактическое закрытие отправлено");

        let reloaded = load_settings(&conf).unwrap();
        assert_eq!(reloaded.temperature, 0.65);
        assert_eq!(reloaded.model, ModelChoice::Fast);
        assert_eq!(reloaded.device, DeviceChoice::Cuda);
        assert_eq!(
            reloaded.parallel_cuda, app.settings.parallel_cuda,
            "A2: производительная конфигурация CUDA сохраняется"
        );
        assert!(!reloaded.theme_dark, "A3: тема сохраняется");
        assert!(!reloaded.pauses_open, "A3: свёртки сохраняются");
        assert_eq!(
            reloaded.extra.get("future_key"),
            Some(&serde_json::json!({"x": 1})),
            "A6: неизвестный ключ пережил автосохранение"
        );
    }

    /// Сессия без изменений не пишет файл: содержимое байт-в-байт то же,
    /// временных файлов не появляется.
    #[test]
    fn close_without_changes_leaves_file_untouched() {
        let conf = close_sandbox("untouched");
        let s = GuiSettings {
            temperature: 0.4,
            ..Default::default()
        };
        save_settings(&s, &conf).unwrap();
        let before = std::fs::read(&conf).unwrap();
        let mut app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, crate::app::SettingsLoadState::Loaded);
        // пользователь ничего не менял
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && has_close(&cmds));
        assert_eq!(app.close_flow, crate::app::CloseFlow::Closing);
        assert_eq!(
            std::fs::read(&conf).unwrap(),
            before,
            "файл не переписывался"
        );
        let tmps: Vec<_> = std::fs::read_dir(conf.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp."))
            .collect();
        assert!(tmps.is_empty(), "лишних tmp-файлов нет");
    }

    /// A7: повреждённый конфиг не перезаписывается молча — закрытие без
    /// сохранения, байты сохранены, причина — в журнале.
    #[test]
    fn close_never_overwrites_unreadable_config() {
        let conf = close_sandbox("corrupt");
        std::fs::write(&conf, b"{ this is not valid json !!!").unwrap();
        let before = std::fs::read(&conf).unwrap();
        let mut app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, crate::app::SettingsLoadState::Unreadable);
        app.settings.temperature = 0.9; // пользователь менял дефолты
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && has_close(&cmds));
        assert_eq!(app.close_flow, crate::app::CloseFlow::Closing);
        assert_eq!(std::fs::read(&conf).unwrap(), before, "конфиг цел");
        assert!(
            app.log.contains("автосохранение"),
            "решение отражено в журнале: {}",
            app.log
        );
    }

    // ===== G6.5: миграция настроек из прежнего имени конфига =====

    /// Песочница миграции: (новый конфиг, прежний конфиг) в одном каталоге.
    fn migration_sandbox(tag: &str) -> (PathBuf, PathBuf) {
        let conf = close_sandbox(tag);
        let legacy = conf.with_file_name(crate::settings::LEGACY_SETTINGS_FILE);
        (conf, legacy)
    }

    /// G6.5: нового конфига нет, прежний валиден — старт переносит настройки
    /// в новый JSON (атомарная запись), прежний файл байт-в-байт не меняется
    /// и не удаляется; значения модели FP32/INT8/устройства/unknown-ключи
    /// сохраняются; закрытие без правок не переписывает новый файл повторно;
    /// повторный старт читает уже новый файл (миграция не повторяется).
    #[test]
    fn migration_reads_legacy_creates_new_keeps_legacy_intact() {
        let (conf, legacy) = migration_sandbox("mig_ok");
        let legacy_json = r#"{
            "model": "SlowInt8",
            "device": "Cuda",
            "temperature": 0.45,
            "theme_dark": false,
            "future_key": {"nested": [1, 2]}
        }"#;
        std::fs::write(&legacy, legacy_json).unwrap();
        let legacy_before = std::fs::read(&legacy).unwrap();

        let mut app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, crate::app::SettingsLoadState::Loaded);
        assert_eq!(app.settings.model, ModelChoice::SlowInt8);
        assert_eq!(app.settings.device, DeviceChoice::Cuda);
        assert!(app.settings.extra.contains_key("future_key"));
        assert!(app.load_note.contains("перенесены"), "{}", app.load_note);

        assert!(conf.is_file(), "новый конфиг создан сразу при старте");
        let migrated = load_settings(&conf).unwrap();
        assert_eq!(migrated.model, ModelChoice::SlowInt8);
        assert_eq!(migrated.temperature, 0.45);
        assert_eq!(
            migrated.extra.get("future_key"),
            app.settings.extra.get("future_key"),
            "unknown-ключ пережил миграцию"
        );
        assert_eq!(
            std::fs::read(&legacy).unwrap(),
            legacy_before,
            "прежний файл не тронут"
        );

        // закрытие без правок не переписывает новый файл
        let new_before = std::fs::read(&conf).unwrap();
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && has_close(&cmds));
        assert_eq!(std::fs::read(&conf).unwrap(), new_before);
        assert_eq!(std::fs::read(&legacy).unwrap(), legacy_before);

        // повторный старт — из нового файла, без повторной миграции
        let app2 = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app2.settings_load, crate::app::SettingsLoadState::Loaded);
        assert_eq!(app2.settings.model, ModelChoice::SlowInt8);
        assert!(!app2.load_note.contains("перенесены"), "{}", app2.load_note);
    }

    /// G6.5: прежний конфиг повреждён — та же защитная политика Unreadable:
    /// автосохранение при закрытии пропускается, ни прежний, ни новый файл
    /// не создаются/не меняются.
    #[test]
    fn migration_legacy_corrupt_is_unreadable_and_never_overwritten() {
        let (conf, legacy) = migration_sandbox("mig_corrupt_legacy");
        std::fs::write(&legacy, b"{ broken !!!").unwrap();
        let legacy_before = std::fs::read(&legacy).unwrap();
        let mut app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, crate::app::SettingsLoadState::Unreadable);
        assert!(app.load_note.contains("прежний"), "{}", app.load_note);
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && has_close(&cmds));
        assert!(
            !conf.exists(),
            "новый файл не создаётся из повреждённых данных"
        );
        assert_eq!(std::fs::read(&legacy).unwrap(), legacy_before);
    }

    /// G6.5: новый конфиг существует, но повреждён — молчаливой подмены
    /// настройками из прежнего файла НЕТ (Unreadable), оба файла целы.
    #[test]
    fn corrupt_new_config_is_not_replaced_by_legacy() {
        let (conf, legacy) = migration_sandbox("mig_corrupt_new");
        std::fs::write(&conf, b"{ broken new !!!").unwrap();
        std::fs::write(&legacy, r#"{"temperature": 0.31, "model": "FastInt8"}"#).unwrap();
        let (new_before, legacy_before) = (
            std::fs::read(&conf).unwrap(),
            std::fs::read(&legacy).unwrap(),
        );
        let mut app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, crate::app::SettingsLoadState::Unreadable);
        assert_eq!(
            app.settings.model,
            GuiSettings::default().model,
            "дефолт, а не значения прежнего файла"
        );
        assert_ne!(app.settings.model, ModelChoice::FastInt8);
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && has_close(&cmds));
        assert_eq!(std::fs::read(&conf).unwrap(), new_before);
        assert_eq!(std::fs::read(&legacy).unwrap(), legacy_before);
    }

    /// G6.5: ни нового, ни прежнего конфига — прежний FirstRun (создание
    /// нового файла при закрытии разрешено, как до переименования).
    #[test]
    fn no_new_and_no_legacy_config_is_first_run() {
        let (conf, legacy) = migration_sandbox("mig_firstrun");
        let app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, crate::app::SettingsLoadState::FirstRun);
        assert!(!conf.exists() && !legacy.exists());
    }

    /// A5: ошибка записи — окно остаётся живым (без Close), существующий
    /// файл цел; явный выбор «Выйти без сохранения» закрывает.
    #[cfg(windows)]
    #[test]
    fn close_save_failure_keeps_window_and_file_then_explicit_exit() {
        let conf = close_sandbox("writefail");
        let s = GuiSettings {
            temperature: 0.4,
            ..Default::default()
        };
        save_settings(&s, &conf).unwrap();
        let before = std::fs::read(&conf).unwrap();
        // read-only у ЦЕЛИ: атомарная замена MoveFileExW REPLACE не может
        // подменить файл с атрибутом «только чтение»
        let mut perms = std::fs::metadata(&conf).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&conf, perms).unwrap();

        let mut app = ShtormApp::with_conf_path(conf.clone());
        app.settings.temperature = 0.8;
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds), "окно удержано");
        assert!(!has_close(&cmds), "фактического закрытия НЕ было");
        assert_eq!(app.close_flow, crate::app::CloseFlow::SaveFailed);
        assert!(app.autosave_error.is_some(), "ошибка зафиксирована");
        assert_eq!(std::fs::read(&conf).unwrap(), before, "старый JSON цел");

        // повторный крестик в SaveFailed не закрывает молча
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && !has_close(&cmds));

        // осознанное решение: выйти без сохранения
        app.close_flow = crate::app::CloseFlow::Closing;
        let cmds = close_frame(&mut app, false);
        assert!(has_close(&cmds) && !has_cancel_close(&cmds));
        assert_eq!(std::fs::read(&conf).unwrap(), before);

        // восстановить атрибут, чтобы артефакт песочницы не путал следующие
        // запуски этого теста (FILE_ATTRIBUTE_NORMAL вместо READONLY)
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{
                SetFileAttributesW, FILE_ATTRIBUTE_NORMAL,
            };
            let wide: Vec<u16> = conf
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: wide — NUL-терминированный путь; вызов меняет атрибуты
            // только собственного файла тестовой песочницы.
            unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_NORMAL) };
        }
    }

    /// A4: крестик при живом задании — отмена, удержание окна до завершения
    /// потока, затем автосохранение и закрытие. UI-поток ничего не ждёт
    /// блокирующе (только флаг cancel + дренаж poll_job).
    #[test]
    fn close_during_job_cancels_then_saves_then_closes() {
        let conf = close_sandbox("duringjob");
        let mut app = ShtormApp::with_conf_path(conf.clone());
        app.settings.temperature = 0.55;
        // мок-задание: фоновый поток жив, пока не увидит cancel
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel2 = Arc::clone(&cancel);
        let (tx, rx) = std::sync::mpsc::channel::<JobEvent>();
        let thread = std::thread::Builder::new()
            .name("mock-synth".into())
            .spawn(move || {
                while !cancel2.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                let _ = tx.send(JobEvent::Finished(Box::new(crate::job::JobResult::failed(
                    "отменено тестом".into(),
                ))));
            })
            .unwrap();
        app.job = Some(JobHandle {
            cancel,
            rx,
            thread: Some(thread),
        });

        // крестик: отмена + удержание, сохранения ещё нет
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && !has_close(&cmds));
        assert_eq!(app.close_flow, crate::app::CloseFlow::WaitingForJob);
        assert!(
            !conf.exists(),
            "сохранение до завершения задания не выполнялось"
        );

        // кадры без close: дренаж событий прибирает завершённое задание,
        // затем машина сама доходит до сохранения и Close
        let mut saw_close = false;
        for _ in 0..2000 {
            app.poll_job();
            let cmds = close_frame(&mut app, false);
            if has_close(&cmds) {
                saw_close = true;
            }
            if app.job.is_none() && app.close_flow == crate::app::CloseFlow::Closing {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.job.is_none(), "задание прибрано");
        assert_eq!(app.close_flow, crate::app::CloseFlow::Closing);
        assert!(saw_close || app.close_sent, "Close отправлен");
        let reloaded = load_settings(&conf).unwrap();
        assert_eq!(
            reloaded.temperature, 0.55,
            "A4: настройки сохранены после отмены"
        );
    }

    /// Защита от цикла: после фактического закрытия индуцированный
    /// close_requested НЕ отменяется (иначе Close↔CancelClose зацикливался
    /// бы), повторный Close не отправляется.
    #[test]
    fn close_after_closing_command_never_cancels_again() {
        let conf = close_sandbox("noloop");
        let mut app = ShtormApp::with_conf_path(conf);
        app.settings.temperature = 0.7;
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && has_close(&cmds));
        assert_eq!(app.close_flow, crate::app::CloseFlow::Closing);
        // кадры «после Close» (eframe индуцирует close_requested сам)
        for _ in 0..3 {
            let cmds = close_frame(&mut app, true);
            assert!(
                !has_cancel_close(&cmds),
                "индуцированный close не отменяется — выхода нет"
            );
            assert!(!has_close(&cmds), "Close повторно не отправляется");
        }
    }

    /// Страховка on_exit: выход мимо штатного потока (Idle) сохраняет
    /// изменения; непрочитанный конфиг по-прежнему не трогается.
    #[test]
    fn on_exit_fallback_saves_changes_but_respects_policy() {
        let conf = close_sandbox("onexit");
        let mut app = ShtormApp::with_conf_path(conf.clone());
        app.settings.temperature = 0.75;
        app.on_exit_save();
        assert_eq!(load_settings(&conf).unwrap().temperature, 0.75);

        // уже закрытое штатным потоком состояние не сохраняет повторно
        let mut app2 = ShtormApp::with_conf_path(conf.clone());
        app2.settings.temperature = 0.31;
        app2.close_flow = crate::app::CloseFlow::Closing;
        app2.on_exit_save();
        assert_eq!(
            load_settings(&conf).unwrap().temperature,
            0.75,
            "повторной записи не было"
        );
        assert_eq!(app2.settings.temperature, 0.31, "внутреннее состояние цело");

        // повреждённый конфиг on_exit тоже не перезаписывает
        let conf2 = close_sandbox("onexit_corrupt");
        std::fs::write(&conf2, b"not json").unwrap();
        let mut app3 = ShtormApp::with_conf_path(conf2.clone());
        app3.settings.temperature = 0.9;
        app3.on_exit_save();
        assert_eq!(std::fs::read(&conf2).unwrap(), b"not json");
    }

    // ===== G6.2.3-fix: ручное сохранение и состояние Unreadable =====

    /// Дефект G6.2.3: Unreadable → ручное сохранение (успех) → правки →
    /// закрытие → повторная загрузка: последние изменения обязаны
    /// сохраниться (раньше settings_load оставался Unreadable, и
    /// автосохранение на закрытии молча пропускалось).
    #[test]
    fn manual_save_unlocks_autosave_after_unreadable_config() {
        let conf = close_sandbox("fix_unlock");
        std::fs::write(&conf, b"{ broken json").unwrap();
        let mut app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, SettingsLoadState::Unreadable);
        // пользователь вручную восстанавливает конфиг (осознанное решение)
        app.settings.temperature = 0.4;
        app.manual_save()
            .expect("ручное сохранение перезаписывает битый файл");
        assert_eq!(app.settings_load, SettingsLoadState::Loaded);
        // ...затем меняет ещё несколько настроек и закрывает окно
        app.settings.temperature = 0.61;
        app.settings.theme_dark = false;
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && has_close(&cmds));
        assert_eq!(app.close_flow, CloseFlow::Closing);
        let reloaded = load_settings(&conf).unwrap();
        assert_eq!(reloaded.temperature, 0.61, "последние правки сохранены");
        assert!(!reloaded.theme_dark);
    }

    /// Unreadable → ручное сохранение (ОШИБКА, read-only цель) → закрытие:
    /// состояние остаётся Unreadable (защита не снята), повреждённый файл
    /// не перезаписан ни ручной попыткой, ни автосохранением.
    #[cfg(windows)]
    #[test]
    fn manual_save_failure_keeps_unreadable_and_broken_file() {
        let conf = close_sandbox("fix_fail");
        let broken = b"{ broken json";
        std::fs::write(&conf, broken).unwrap();
        // read-only у ЦЕЛИ: атомарная замена MoveFileExW не может подменить
        // файл с атрибутом «только чтение»
        let mut perms = std::fs::metadata(&conf).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&conf, perms).unwrap();

        let mut app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, SettingsLoadState::Unreadable);
        app.settings.temperature = 0.7;
        assert!(app.manual_save().is_err(), "read-only цель — отказ записи");
        assert_eq!(
            app.settings_load,
            SettingsLoadState::Unreadable,
            "ошибка ручного сохранения НЕ снимает защиту повреждённого конфига"
        );
        // закрытие: политика Unreadable — пропуск автосохранения, файл цел
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && has_close(&cmds));
        assert_eq!(std::fs::read(&conf).unwrap(), broken, "файл байт-в-байт");

        // восстановить атрибут артефакта песочницы
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{
                SetFileAttributesW, FILE_ATTRIBUTE_NORMAL,
            };
            let wide: Vec<u16> = conf
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: wide — NUL-терминированный путь песочницы этого теста
            unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_NORMAL) };
        }
    }

    /// Unreadable → закрытие БЕЗ ручного сохранения: прежнее защитное
    /// поведение — автосохранение пропущено, повреждённый файл не тронут,
    /// лишних временных файлов не появилось (A7-семантика сохранена).
    #[test]
    fn unreadable_close_without_manual_save_still_protects_file() {
        let conf = close_sandbox("fix_protect");
        let broken = b"{ broken json";
        std::fs::write(&conf, broken).unwrap();
        let mut app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, SettingsLoadState::Unreadable);
        app.settings.temperature = 0.8; // правки пользователя
        let cmds = close_frame(&mut app, true);
        assert!(has_cancel_close(&cmds) && has_close(&cmds));
        assert_eq!(app.close_flow, CloseFlow::Closing);
        assert_eq!(std::fs::read(&conf).unwrap(), broken);
        let tmps: Vec<_> = std::fs::read_dir(conf.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp."))
            .collect();
        assert!(tmps.is_empty(), "временных файлов нет");
    }

    /// Loaded → ручное сохранение → закрытие без новых правок: повторной
    /// записи нет (снимок обновлён ручным сохранением, файл байт-в-байт).
    #[test]
    fn manual_save_updates_snapshot_close_writes_nothing() {
        let conf = close_sandbox("fix_snapshot");
        save_settings(&GuiSettings::default(), &conf).unwrap();
        let mut app = ShtormApp::with_conf_path(conf.clone());
        assert_eq!(app.settings_load, SettingsLoadState::Loaded);
        app.settings.temperature = 0.5;
        app.manual_save()
            .expect("ручное сохранение валидного конфига");
        let after_manual = std::fs::read(&conf).unwrap();
        // пользователь больше ничего не меняет и закрывает окно
        let cmds = close_frame(&mut app, true);
        assert!(has_close(&cmds));
        assert_eq!(
            std::fs::read(&conf).unwrap(),
            after_manual,
            "повторной записи при закрытии нет"
        );
    }

    /// G6.4: поле + «Обзор…» + отступ всегда помещаются в доступную ширину
    /// (на узком окне поле ужимается, кнопка не уезжает за край).
    #[test]
    fn path_row_reserves_space_for_browse_button() {
        for available in [280.0, 360.0, 500.0, 820.0, 1160.0] {
            let spacing = 8.0;
            let (field, button) = path_widths(available, spacing);
            assert!(field >= 60.0, "поле слишком узкое: {field}");
            assert_eq!(button, BROWSE_BUTTON_WIDTH);
            assert!(
                field + button + spacing <= available + 0.01,
                "строка {available}: поле {field} + кнопка {button} не помещаются"
            );
        }
        // ниже минимума строка вырождается в минимальное поле — но окно
        // меньше 560 точек недоступно (min_inner_size), см. window.rs
        let (field, button) = path_widths(120.0, 8.0);
        assert_eq!((field, button), (60.0, BROWSE_BUTTON_WIDTH));
    }

    /// G6.4: единый читаемый масштаб типографики и отступов.
    #[test]
    fn typography_and_spacing_are_readable() {
        let ctx = egui::Context::default();
        configure_style(&ctx);
        let st = ctx.style();
        let size = |style: egui::TextStyle| st.text_styles[&style].size;
        // Типографика e03342f (G6.4-fix): Body/Button 15, Small/Mono 14,
        // Heading 19 — читаемые размеры, согласованные заказчиком.
        assert!((15.0..=16.0).contains(&size(egui::TextStyle::Body)));
        assert!((15.0..=16.0).contains(&size(egui::TextStyle::Button)));
        assert!((14.0..=15.0).contains(&size(egui::TextStyle::Small)));
        assert!(
            size(egui::TextStyle::Heading) >= size(egui::TextStyle::Body) + 3.0,
            "заголовки должны быть заметно крупнее основного текста"
        );
        assert!((26.0..=28.0).contains(&st.spacing.interact_size.y));
        assert!(st.spacing.icon_width >= 16.0);
        assert!(st.spacing.item_spacing.y >= 8.0);
        assert!(st.spacing.indent >= 18.0);
    }

    /// G6.4 (визуальная приёмка): иконка чекбокса и подпись — разные
    /// виджеты, их прямоугольники не пересекаются даже внутри колонки.
    #[test]
    fn checkbox_row_icon_and_label_do_not_overlap() {
        let ctx = egui::Context::default();
        configure_style(&ctx);
        let mut rects = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui.ctx(), |ui| {
                ui.vertical(|ui| {
                    ui.set_width(500.0);
                    let mut checked = true;
                    let (icon, text) =
                        checkbox_row_resp(ui, &mut checked, "Защита от повторов (двухпопытная)");
                    rects = Some((icon.rect, text.rect));
                });
            });
        });
        let (icon, text) = rects.expect("строка чекбокса отрисована");
        assert!(
            icon.right() <= text.left() + 0.5,
            "иконка {icon:?} пересекает подпись {text:?}"
        );
        assert!(text.width() > 20.0, "подпись не отрисована: {text:?}");
    }

    /// G6.4: порог 2→1 колонки зависит только от фактической ширины панели.
    #[test]
    fn two_column_threshold_follows_panel_width() {
        let threshold = two_columns_threshold();
        assert!(!uses_two_columns(threshold - 1.0));
        assert!(uses_two_columns(threshold));
        assert!(uses_two_columns(FORM_MAX_WIDTH));
        // Порог выведен из минимальных размеров содержимого, а не из
        // конкретного разрешения монитора.
        assert!(
            (560.0..=760.0).contains(&threshold),
            "threshold={threshold}"
        );
    }

    /// G6.4: мягкие переносы не меняют видимый текст (ZWSP невидим), но
    /// дают возможность перенести длинный неразрывный токен пути.
    #[test]
    fn soft_wrap_paths_keeps_text_and_adds_break_points() {
        let token = "a".repeat(60);
        let wrapped = soft_wrap_paths(&token);
        assert!(wrapped.contains('\u{200B}'), "нет точек переноса");
        let restored: String = wrapped.chars().filter(|c| *c != '\u{200B}').collect();
        assert_eq!(restored, token, "видимый текст изменился");
        assert!(
            !soft_wrap_paths("короткий путь").contains('\u{200B}'),
            "короткий текст не должен разрываться"
        );
    }
}
