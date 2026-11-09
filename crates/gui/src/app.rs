//! G1/G2: форма GUI (eframe/egui) с рабочей валидацией и запуском синтеза
//! (G2 — фоновый поток поверх app_core::pipeline::synthesize_stream).

use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::job::{JobEvent, JobStatus};
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
    /// Пользователь закрыл окно при живом задании: отменяем и закрываем сами,
    /// когда поток завершится (без сиротских ffmpeg).
    close_pending: bool,
    /// Кэш проверки ffmpeg (resolve_ffmpeg запускает процесс — не каждый кадр):
    /// ключ = явный путь из настроек, значение = текст ошибки, если не найден.
    ffmpeg_check_key: Option<String>,
    ffmpeg_check_err: Option<String>,
    ffmpeg_checked_at: std::time::Instant,
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
        Self {
            settings,
            log: String::new(),
            status: note,
            last_saved: None,
            job: None,
            progress: (0, 0),
            file_progress: (0, 0),
            batch_running: false,
            close_pending: false,
            ffmpeg_check_key: None,
            ffmpeg_check_err: None,
            ffmpeg_checked_at: std::time::Instant::now(),
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
        let ctx2 = ctx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel2 = Arc::clone(&cancel);
        let text_file = s.text_file.trim().to_string();
        let thread = std::thread::Builder::new()
            .name("shtorm-synth".into())
            .spawn(move || {
                let result = if batch {
                    let source = PathBuf::from(&text_file);
                    crate::job::execute_batch(&s, &source, &cancel2, &|ev| {
                        let _ = tx.send(ev);
                        ctx2.request_repaint();
                    })
                } else {
                    let text = match std::fs::read_to_string(&text_file) {
                        Ok(t) => t,
                        Err(e) => {
                            let r = crate::job::JobResult::failed(format!(
                                "прочитать текстовый файл {text_file}: {e}"
                            ));
                            let _ = tx.send(JobEvent::Log(r.summary.clone()));
                            let _ = tx.send(JobEvent::Finished(Box::new(r)));
                            ctx2.request_repaint();
                            return;
                        }
                    };
                    let cfg = crate::job::JobConfig {
                        settings: s,
                        text,
                        out_path,
                    };
                    crate::job::execute_job(&cfg, &cancel2, &|ev| {
                        let _ = tx.send(ev);
                        ctx2.request_repaint();
                    })
                };
                let _ = tx.send(JobEvent::Finished(Box::new(result)));
                ctx2.request_repaint();
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
            JobEvent::FileProgress {
                index,
                total,
                done,
                chunks_total,
            } => {
                self.file_progress = (index, total);
                self.progress = (done, chunks_total);
                self.status = format!("файл {index}/{total}: {done}/{chunks_total} чанков");
            }
            JobEvent::FileDone {
                index,
                total,
                report,
            } => {
                // K/N обновляется на КАЖДОМ завершённом файле, включая
                // Failed/Skipped, — иначе прогресс пакета «замирает».
                self.file_progress = (index, total);
                self.progress = (report.chunks_done, report.chunks_total);
                self.status = format!("файл {index}/{total}: {}", report.status.label());
                self.push_log(&report.line());
            }
            JobEvent::Log(line) => self.push_log(&line),
            JobEvent::Finished(res) => {
                let warn = matches!(res.status, JobStatus::Partial | JobStatus::NoWork);
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
                    Some(Path::new(&key))
                };
                self.ffmpeg_check_err = app_core::sink::resolve_ffmpeg(explicit)
                    .err()
                    .map(|e| format!("{e:#}"));
                self.ffmpeg_check_key = Some(key);
                self.ffmpeg_checked_at = now;
            }
            ffmpeg_err = self.ffmpeg_check_err.clone();
        }

        let mut errs = Vec::new();
        let s = &self.settings;
        if s.text_file.trim().is_empty() {
            errs.push(if s.batch {
                "не выбран исходный каталог".into()
            } else {
                "не выбран текстовый файл".into()
            });
        } else if s.batch {
            if !Path::new(&s.text_file).is_dir() {
                errs.push(format!("исходный каталог не найден: {}", s.text_file));
            }
        } else if !Path::new(&s.text_file).is_file() {
            errs.push(format!("текстовый файл не найден: {}", s.text_file));
        }
        if s.voice_wav.trim().is_empty() {
            errs.push("не выбран WAV-голос".into());
        } else if !s.voice_wav.to_lowercase().ends_with(".wav") {
            errs.push("голосовой референс: поддерживается только WAV".into());
        } else if !Path::new(&s.voice_wav).is_file() {
            errs.push(format!("WAV-голос не найден: {}", s.voice_wav));
        }
        // модель — единым резолвером запуска (fast/slow через onnx_base)
        if let Err(e) = crate::job::resolve_model_dir(s) {
            errs.push(e.to_string());
        }
        if s.out_dir.trim().is_empty() {
            errs.push("не указана папка результата".into());
        } else if !s.batch && crate::job::derive_out_path(s).exists() {
            errs.push(format!(
                "итоговый файл уже существует: {} — не перезаписываем",
                crate::job::derive_out_path(s).display()
            ));
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
                    let stop = ui.add_enabled(true, egui::Button::new("Стоп"));
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
                if ui.button("Импорт из Python-GUI (только чтение)").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .set_title("Выберите settings_pocket_tts.json")
                        .add_filter("JSON", &["json"])
                        .pick_file()
                    {
                        match crate::settings::import_from_python(&p) {
                            Ok(s) => {
                                self.settings = s;
                                self.status = format!("импортировано (read-only): {}", p.display());
                            }
                            Err(e) => self.status = format!("импорт не удался: {e:#}"),
                        }
                    }
                }
            });
            if !errors.is_empty() {
                for e in &errors {
                    ui.label(
                        egui::RichText::new(format!("• {e}"))
                            .color(egui::Color32::from_rgb(230, 120, 60)),
                    );
                }
            }
            // Пакетный режим: ОТДЕЛЬНО прогресс по файлам K/N и по чанкам
            // текущего файла; одиночный — один бар по чанкам.
            if self.batch_running && self.file_progress.1 > 0 {
                let (fi, ft) = self.file_progress;
                let frac = if ft > 0 { fi as f32 / ft as f32 } else { 0.0 };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .text(format!("файл {fi}/{ft}"))
                        .show_percentage(),
                );
            }
            if self.job.is_some() || self.progress.1 > 0 {
                let (done, total) = self.progress;
                let frac = if total > 0 {
                    done as f32 / total as f32
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .text(if self.batch_running {
                            format!("чанки текущего файла: {done}/{total}")
                        } else {
                            format!("{done}/{total} чанков")
                        })
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
                    ui.horizontal(|ui| {
                        ui.label("Порог EOS:");
                        ui.add(
                            egui::DragValue::new(&mut s.eos_threshold)
                                .range(-20.0..=20.0)
                                .speed(0.1),
                        );
                        ui.weak("(Auto из runtime.json — не поддержано, задано вручную)");
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
                if ui.button("Очистить журнал").clicked() {
                    self.log.clear();
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

    /// Пакетные события: K/N файлов и отдельно чанки текущего файла.
    #[test]
    fn batch_events_update_ui_state() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        app.apply_event(crate::job::JobEvent::FileProgress {
            index: 2,
            total: 5,
            done: 1,
            chunks_total: 4,
        });
        assert_eq!(app.file_progress, (2, 5));
        assert_eq!(app.progress, (1, 4));
        assert!(app.status.contains("файл 2/5"), "{}", app.status);

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
        assert!(app.status.contains("файл 2/5"), "{}", app.status);
        assert!(app.log.contains("sub/a.txt") && app.log.contains("sub/a.wav"));
    }

    /// G3.1: K/N обновляется на Failed/Skipped, а частичный успех и пустой
    /// пакет явно предупреждают пользователя.
    #[test]
    fn batch_failed_skipped_and_partial_states() {
        let mut app = ShtormApp::new();
        app.batch_running = true;
        let file = crate::batch::BatchFile {
            input: PathBuf::from("C:/src/b.txt"),
            relative: PathBuf::from("b.txt"),
        };
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
        assert!(app.status.contains("ошибка"), "{}", app.status);

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
                batch: None,
            },
        )));
        assert!(app.status.starts_with("⚠ Нет работы"), "{}", app.status);
    }
}
