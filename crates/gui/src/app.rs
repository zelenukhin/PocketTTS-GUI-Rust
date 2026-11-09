//! G1: статическая форма GUI (eframe/egui) с рабочей валидацией.
//! Синтез через кнопки подключается на G2 — сейчас «Запустить» показывает
//! состояние валидации и явно помечен как G2 (фиктивного успеха нет).

use eframe::egui;
use std::path::{Path, PathBuf};

use crate::settings::{
    load_settings, save_settings, settings_path, DeviceChoice, GuiSettings, ModelChoice,
    OutputFormatChoice,
};

pub struct ShtormApp {
    pub settings: GuiSettings,
    log: String,
    status: String,
    last_saved: Option<PathBuf>,
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
        }
    }

    /// Валидация путей/параметров (без блокировок: только локальные проверки).
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let s = &self.settings;
        if s.text_file.trim().is_empty() {
            errs.push("не выбран текстовый файл".into());
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
        match s.model {
            ModelChoice::Custom => {
                if !Path::new(&s.model_dir).join("flow_lm_main.onnx").is_file() {
                    errs.push(format!(
                        "в каталоге модели нет flow_lm_main.onnx: {}",
                        s.model_dir
                    ));
                }
            }
            _ => {
                if !Path::new(&s.model_dir).join("flow_lm_main.onnx").is_file() {
                    errs.push(format!(
                        "укажите каталог модели Shtorm-v2 (flow_lm_main.onnx не найден): {}",
                        s.model_dir
                    ));
                }
            }
        }
        if s.out_dir.trim().is_empty() {
            errs.push("не указана папка результата".into());
        }
        if s.format != OutputFormatChoice::Wav {
            let ff = s.ffmpeg_path.trim();
            if ff.is_empty() {
                errs.push("для MP3/Opus укажите путь к ffmpeg".into());
            } else if !Path::new(ff).is_file() {
                errs.push(format!("ffmpeg не найден: {ff}"));
            }
        }
        errs
    }
}

impl eframe::App for ShtormApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("ShtormTTS (нативный GUI, G1)");
                ui.separator();
                ui.label(egui::RichText::new(&self.status).weak());
            });
        });
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.add_space(4.0);
            let errors = self.validate();
            ui.horizontal(|ui| {
                let run = ui.add_enabled(errors.is_empty(), egui::Button::new("Запустить синтез"));
                if run.clicked() {
                    // G2: подключение synthesize_stream в фоновом потоке.
                    // Фиктивного успеха не показываем — только статус.
                    self.status = "Синтез будет подключён на этапе G2".into();
                }
                if errors.is_empty() {
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
                let _ = ui.add_enabled(false, egui::Button::new("Стоп (G2)"));
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
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            let s = &mut self.settings;
            egui::CollapsingHeader::new("Источник и голос")
                .default_open(s.source_open)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Текстовый файл:");
                        ui.text_edit_singleline(&mut s.text_file);
                        if ui.button("Обзор…").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_title("Выберите текстовый файл")
                                .add_filter("Текст", &["txt", "md", "log"])
                                .pick_file()
                            {
                                s.text_file = p.display().to_string();
                            }
                        }
                    });
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
            ui.add_space(4.0);

            egui::CollapsingHeader::new("Модель и устройство")
                .default_open(true)
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
                    ui.weak(format!(
                        "Воркеров: {} (авто: CPU 2 / CUDA 1)",
                        s.effective_workers()
                    ));
                });
            ui.add_space(4.0);

            egui::CollapsingHeader::new("Параметры генерации")
                .default_open(s.gen_open)
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
            ui.add_space(4.0);

            egui::CollapsingHeader::new("Паузы и воркеры")
                .default_open(true)
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
                });
            ui.add_space(4.0);

            egui::CollapsingHeader::new("Вывод")
                .default_open(s.out_open)
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
                        if s.format.mp3_preset_needed() {
                            egui::ComboBox::from_id_salt("preset")
                                .selected_text(&s.mp3_preset)
                                .show_ui(ui, |ui| {
                                    for p in [
                                        "-V0 (Максимальное)",
                                        "-V2 (Высокое)",
                                        "-V4 (Среднее)",
                                        "192k",
                                        "128k",
                                        "opus 96k",
                                        "opus 64k",
                                        "opus 32k",
                                    ] {
                                        ui.selectable_value(&mut s.mp3_preset, p.to_string(), p);
                                    }
                                });
                        }
                        ui.checkbox(&mut s.chunk_log, "Лог чанков");
                    });
                    if s.format.mp3_preset_needed() {
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
