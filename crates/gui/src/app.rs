//! G1/G2: форма GUI (eframe/egui) с рабочей валидацией и запуском синтеза
//! (G2 — фоновый поток поверх app_core::pipeline::synthesize_stream).

use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::batch::FileStatus;
use crate::job::{JobEvent, JobPhase, JobStatus};
use crate::settings::{
    load_settings, save_settings, settings_path, DeviceChoice, GuiSettings, ModelChoice,
    OutputFormatChoice,
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

pub struct ShtormApp {
    pub settings: GuiSettings,
    log: String,
    status: String,
    last_saved: Option<PathBuf>,
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
    /// Пользователь закрыл окно при живом задании: отменяем и закрываем сами,
    /// когда поток завершится (без сиротских ffmpeg).
    close_pending: bool,
    /// Кэш проверки ffmpeg (resolve_ffmpeg запускает процесс — не каждый кадр):
    /// ключ = явный путь из настроек, значение = текст ошибки, если не найден.
    ffmpeg_check_key: Option<String>,
    ffmpeg_check_err: Option<String>,
    ffmpeg_checked_at: std::time::Instant,
    /// G5: последняя применённая тема (переключатель без лишних set_visuals).
    theme_applied: bool,
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
        let (settings, note) = match load_settings(&settings_path()) {
            Ok(s) => (
                s,
                format!("настройки загружены: {}", settings_path().display()),
            ),
            Err(e) => (
                GuiSettings::default(),
                format!("настройки не загружены ({e:#}); значения по умолчанию"),
            ),
        };
        // G5.2 (визуальный контроль): ни один кадр ещё не нарисован — если
        // пометить тему «применённой» здесь, update() никогда не вызовет
        // set_visuals и приложение стартует со светлым дефолтом eframe даже
        // при theme_dark=true. Помечаем НЕ применённой: первый кадр
        // установит visuals до отрисовки (без вспышки светлой темы).
        let theme_applied = !settings.theme_dark;
        Self {
            settings,
            log: String::new(),
            status: note,
            last_saved: None,
            job: None,
            progress: (0, 0),
            file_progress: (0, 0),
            batch_running: false,
            batch_state: None,
            publishing: false,
            close_pending: false,
            ffmpeg_check_key: None,
            ffmpeg_check_err: None,
            ffmpeg_checked_at: std::time::Instant::now(),
            theme_applied,
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
            .name("shtorm-synth".into())
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
}

impl eframe::App for ShtormApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // G5: тема применяется только при изменении настройки, не каждый кадр.
        if self.theme_applied != self.settings.theme_dark {
            ctx.set_visuals(if self.settings.theme_dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            });
            self.theme_applied = self.settings.theme_dark;
        }
        // события фонового задания (прогресс/логи/итог) — каждый кадр
        self.poll_job();

        // Закрытие окна при живом задании: отменяем и удерживаем окно, пока
        // поток не завершится (CancelClose), затем закрываем сами — синк
        // дропается в потоке задания, сиротских ffmpeg не остаётся.
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested {
            if let Some(job) = &self.job {
                job.cancel.store(true, Ordering::SeqCst);
                if !self.close_pending {
                    self.close_pending = true;
                    self.push_log("закрытие: ожидаю отмену задания…");
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }
        if self.close_pending && self.job.is_none() {
            self.close_pending = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // пока задание живо — регулярные перерисовки для дренажа событий
        if self.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("ShtormTTS (нативный GUI)");
                ui.separator();
                ui.label(egui::RichText::new(&self.status).weak());
            });
        });
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.add_space(4.0);
            let errors = self.validate();
            let job_running = self.job.is_some();
            ui.horizontal(|ui| {
                let run = ui.add_enabled(
                    errors.is_empty() && !job_running,
                    egui::Button::new("Запустить синтез"),
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
                        .add_enabled(self.stop_available(), egui::Button::new("Стоп"))
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
                } else if errors.is_empty() {
                    ui.label(
                        egui::RichText::new("✓ параметры валидны")
                            .color(egui::Color32::from_rgb(90, 200, 90)),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(format!("⚠ {} проблем(ы)", errors.len()))
                            .color(egui::Color32::from_rgb(230, 120, 60)),
                    );
                }
                if ui.button("Сохранить настройки").clicked() {
                    self.settings.clamped();
                    match save_settings(&self.settings, &settings_path()) {
                        Ok(()) => {
                            self.last_saved = Some(settings_path());
                            self.status = format!("сохранено: {}", settings_path().display());
                        }
                        Err(e) => self.status = format!("ошибка сохранения: {e:#}"),
                    }
                }
                // G5: переключение темы сохраняется вместе с настройками.
                ui.checkbox(&mut self.settings.theme_dark, "Тёмная тема");
            });
            if !errors.is_empty() {
                for e in &errors {
                    ui.label(
                        egui::RichText::new(format!("• {e}"))
                            .color(egui::Color32::from_rgb(230, 120, 60)),
                    );
                }
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
                        for f in &bs.active {
                            let phase = match f.phase {
                                JobPhase::Generating => "генерация",
                                JobPhase::Finalizing | JobPhase::Publishing => "сохранение",
                            };
                            ui.label(format!(
                                "  [{}/{}] {} — чанки {}/{} ({})",
                                f.index, bs.total, f.name, f.done, f.total, phase
                            ));
                        }
                    }
                }
            } else if self.job.is_some() || self.progress.1 > 0 {
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
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            let s = &mut self.settings;
            let resp = egui::CollapsingHeader::new("Источник и голос")
                .open(Some(s.source_open))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Режим:");
                        ui.radio_value(&mut s.batch, false, "Один файл");
                        ui.radio_value(&mut s.batch, true, "Каталог (пакетно)");
                    });
                    ui.horizontal(|ui| {
                        ui.label(if s.batch {
                            "Исходный каталог:"
                        } else {
                            "Текстовый файл:"
                        });
                        ui.text_edit_singleline(&mut s.text_file);
                        if ui.button("Обзор…").clicked() {
                            let picked = if s.batch {
                                rfd::FileDialog::new()
                                    .set_title("Выберите каталог с .txt/.md/.log")
                                    .pick_folder()
                            } else {
                                rfd::FileDialog::new()
                                    .set_title("Выберите текстовый файл")
                                    .add_filter("Текст", &["txt", "md", "log"])
                                    .pick_file()
                            };
                            if let Some(p) = picked {
                                s.text_file = p.display().to_string();
                            }
                        }
                    });
                    if s.batch {
                        ui.weak(
                            "Рекурсивно .txt/.md/.log без символических ссылок; выходная папка исключается.",
                        );
                    }
                    ui.horizontal(|ui| {
                        ui.label("WAV-голос:");
                        ui.text_edit_singleline(&mut s.voice_wav);
                        if ui.button("Обзор…").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_title("Выберите WAV-голос (1–30 с чистой речи)")
                                .add_filter("WAV", &["wav"])
                                .pick_file()
                            {
                                s.voice_wav = p.display().to_string();
                            }
                        }
                    });
                    ui.weak(
                        "Голосовой референс: поддерживается WAV (.safetensors — в будущих этапах).",
                    );
                });
            header_clicked(&resp, &mut s.source_open);
            ui.add_space(4.0);

            let resp = egui::CollapsingHeader::new("Модель и устройство")
                .open(Some(s.model_open))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Модель:");
                        egui::ComboBox::from_id_salt("model")
                            .selected_text(match s.model {
                                ModelChoice::Fast => "Shtorm-v2 fast",
                                ModelChoice::Slow => "Shtorm-v2 slow",
                                ModelChoice::Custom => "Свой каталог ONNX",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut s.model,
                                    ModelChoice::Fast,
                                    "Shtorm-v2 fast",
                                );
                                ui.selectable_value(
                                    &mut s.model,
                                    ModelChoice::Slow,
                                    "Shtorm-v2 slow",
                                );
                                ui.selectable_value(
                                    &mut s.model,
                                    ModelChoice::Custom,
                                    "Свой каталог ONNX",
                                );
                            });
                    });
                    if s.model == ModelChoice::Custom {
                        ui.horizontal(|ui| {
                            ui.label("Каталог модели:");
                            ui.text_edit_singleline(&mut s.model_dir);
                            if ui.button("Обзор…").clicked() {
                                if let Some(p) = rfd::FileDialog::new()
                                    .set_title("Каталог модели (flow_lm_main.onnx и др.)")
                                    .pick_folder()
                                {
                                    s.model_dir = p.display().to_string();
                                }
                            }
                        });
                    } else {
                        ui.horizontal(|ui| {
                            ui.label("Каталог ONNX (база):");
                            ui.text_edit_singleline(&mut s.onnx_base);
                            if ui.button("Обзор…").clicked() {
                                if let Some(p) = rfd::FileDialog::new()
                                    .set_title("Каталог с ru-v2-fast / ru-v2-slow")
                                    .pick_folder()
                                {
                                    s.onnx_base = p.display().to_string();
                                }
                            }
                        });
                        ui.weak(format!(
                            "модель: {}",
                            crate::job::resolve_model_dir(s)
                                .map(|d| d.display().to_string())
                                .unwrap_or_else(|e| e.to_string())
                        ));
                    }
                    ui.horizontal(|ui| {
                        ui.label("Устройство:");
                        egui::ComboBox::from_id_salt("device")
                            .selected_text(match s.device {
                                DeviceChoice::Cpu => "CPU",
                                DeviceChoice::Cuda => "CUDA",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut s.device, DeviceChoice::Cpu, "CPU");
                                ui.selectable_value(&mut s.device, DeviceChoice::Cuda, "CUDA");
                            });
                        if s.device == DeviceChoice::Cuda {
                            ui.add(
                                egui::DragValue::new(&mut s.device_id)
                                    .range(0..=8)
                                    .prefix("GPU "),
                            );
                        }
                    });
                    ui.horizontal(|ui| {
                        let dll = if s.device == DeviceChoice::Cuda {
                            &mut s.ort_gpu_dll
                        } else {
                            &mut s.ort_cpu_dll
                        };
                        ui.label("onnxruntime.dll:");
                        ui.text_edit_singleline(dll);
                        ui.weak("(пусто = авто из deps проекта)");
                    });
                    ui.weak(format!(
                        "Воркеров: {} (авто: CPU 2 / CUDA 1)",
                        s.effective_workers()
                    ));
                });
            header_clicked(&resp, &mut s.model_open);
            ui.add_space(4.0);

            let resp = egui::CollapsingHeader::new("Параметры генерации")
                .open(Some(s.gen_open))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Температура:");
                        if ui
                            .add(egui::Slider::new(&mut s.temperature, 0.05..=1.5).step_by(0.05))
                            .changed()
                        {
                            // значение уже в диапазоне слайдера
                        }
                        ui.label(format!("{:.2}", s.temperature));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Шаги декодирования:");
                        ui.add(egui::Slider::new(&mut s.decode_steps, 1..=8));
                    });
                    // G4.1: компактная группа EOS — порог и оба хвоста.
                    // Значения teratts-android: порог −4, хвосты 5/3 — без
                    // Python-пересчёта 6/4.
                    ui.group(|ui| {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label("EOS:").on_hover_text(
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
                                    s.eos_threshold =
                                        crate::settings::EosThresholdSetting::Fixed(-4.0);
                                    s.eos_tail_short = shtorm_engine::EosTail::Fixed(5);
                                    s.eos_tail_long = shtorm_engine::EosTail::Fixed(3);
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("Порог:");
                                // G5: «Авто» в списке больше нет — выбранное
                                // значение всегда число. Устаревший Auto из
                                // старого конфига показываем как −4.0 и при
                                // любом выборе заменяем числом.
                                let selected = format!("{}", s.eos_threshold.value());
                                egui::ComboBox::from_id_salt("eos_threshold")
                                    .selected_text(selected)
                                    .show_ui(ui, |ui| {
                                        for v in crate::settings::EOS_THRESHOLD_CHOICES {
                                            ui.selectable_value(
                                                &mut s.eos_threshold,
                                                crate::settings::EosThresholdSetting::Fixed(*v),
                                                format!("{v}"),
                                            );
                                        }
                                    })
                                    .response
                                    .on_hover_text(
                                        "Логит EOS выше порога — конец речи. Значение \
                                         передаётся движку как есть; −4.0 — встроенное \
                                         значение teratts-android (не читается из runtime.json).",
                                    );
                            });
                            ui.horizontal(|ui| {
                                ui.label("Кадров хвоста EOS:");
                                ui.label("≤4 слов:").on_hover_text(
                                    "Хвост для коротких чанков (1–4 слова по подготовленному \
                                     тексту до добавления служебных пробелов).",
                                );
                                egui::ComboBox::from_id_salt("eos_tail_short")
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
                                ui.label("≥5 слов:").on_hover_text(
                                    "Хвост для длинных чанков (5 и более слов текущего чанка).",
                                );
                                egui::ComboBox::from_id_salt("eos_tail_long")
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
                    ui.horizontal(|ui| {
                        ui.label("Seed:");
                        ui.add(egui::DragValue::new(&mut s.seed).range(-1..=i64::MAX));
                        ui.weak("(-1 — случайный)");
                    });
                    ui.checkbox(&mut s.anti_repeat, "Защита от повторов (двухпопытная)");
                    ui.checkbox(&mut s.denoise, "Очистка звука (DPDFNet)");
                    ui.horizontal(|ui| {
                        ui.label("Яркость (дБ):");
                        ui.add(egui::Slider::new(&mut s.brightness_db, 0.0..=12.0));
                    });
                });
            header_clicked(&resp, &mut s.gen_open);
            ui.add_space(4.0);

            let resp = egui::CollapsingHeader::new("Паузы и воркеры")
                .open(Some(s.pauses_open))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Масштаб пауз:");
                        ui.add(egui::Slider::new(&mut s.pause_scale, 0.0..=2.0).step_by(0.05));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Пауза абзаца (с):");
                        ui.add(
                            egui::Slider::new(&mut s.paragraph_pause_s, 0.0..=3.0).step_by(0.05),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Макс. токенов:");
                        ui.add(egui::Slider::new(&mut s.max_tokens, 10..=300).step_by(5.0));
                    });
                    let mut manual = s.workers.is_some();
                    if ui.checkbox(&mut manual, "Воркеров вручную").changed() {
                        s.workers = if manual {
                            Some(s.effective_workers().max(1))
                        } else {
                            None
                        };
                    }
                    if let Some(w) = &mut s.workers {
                        ui.add(egui::Slider::new(w, 1..=12));
                    }
                    ui.horizontal(|ui| {
                        ui.label("Потоков ORT (intra-op):");
                        ui.add(egui::DragValue::new(&mut s.threads).range(1..=16));
                        ui.weak("(паритет с CLI --threads, по умолчанию 4)");
                    });
                });
            header_clicked(&resp, &mut s.pauses_open);
            ui.add_space(4.0);

            let resp = egui::CollapsingHeader::new("Вывод")
                .open(Some(s.out_open))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Формат:");
                        egui::ComboBox::from_id_salt("format")
                            .selected_text(s.format.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut s.format, OutputFormatChoice::Wav, "WAV");
                                ui.selectable_value(&mut s.format, OutputFormatChoice::Mp3, "MP3");
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
                        ui.checkbox(&mut s.chunk_log, "Лог чанков");
                    });
                    if s.format.needs_ffmpeg() {
                        ui.horizontal(|ui| {
                            ui.label("ffmpeg:");
                            ui.text_edit_singleline(&mut s.ffmpeg_path);
                            if ui.button("Обзор…").clicked() {
                                if let Some(p) = rfd::FileDialog::new()
                                    .set_title("ffmpeg.exe")
                                    .add_filter("ffmpeg", &["exe"])
                                    .pick_file()
                                {
                                    s.ffmpeg_path = p.display().to_string();
                                }
                            }
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label("Папка результата:");
                        ui.text_edit_singleline(&mut s.out_dir);
                        if ui.button("Обзор…").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_title("Папка результата")
                                .pick_folder()
                            {
                                s.out_dir = p.display().to_string();
                            }
                        }
                    });
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
                });
            header_clicked(&resp, &mut s.out_open);
            ui.add_space(6.0);
            ui.separator();
            ui.label("Журнал:");
            egui::ScrollArea::vertical()
                .max_height(ui.available_height() - 28.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.log)
                            .desired_rows(6)
                            .interactive(false),
                    );
                });
            ui.horizontal(|ui| {
                // G5: журнал удобно скопировать в буфер обмена целиком.
                if ui.button("Копировать журнал").clicked() {
                    if self.log.is_empty() {
                        self.status = "журнал пуст — копировать нечего".into();
                    } else {
                        ui.ctx().copy_text(self.log.clone());
                        self.status = "журнал скопирован в буфер обмена".into();
                    }
                }
                if ui.button("Очистить журнал").clicked() {
                    // очищается только отображаемая копия; файлы журналов
                    // (диагностика/лог чанков) на диске не удаляются
                    self.log.clear();
                    self.status = "журнал очищен (файлы на диске не тронуты)".into();
                }
                if let Some(p) = &self.last_saved {
                    ui.weak(format!("последнее сохранение: {}", p.display()));
                }
            });
        });
    }
}

#[cfg(test)]
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
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
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
}
