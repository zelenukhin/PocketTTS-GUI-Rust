//! ShtormTTS GUI (G1). Кириллица: подключаем системный шрифт Segoe UI
//! (есть на любой Windows) с приоритетом — гарант корректного отображения.

mod app;
mod settings;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 860.0])
            .with_min_inner_size([760.0, 560.0])
            .with_title("ShtormTTS — нативный GUI (G1)"),
        ..Default::default()
    };
    eframe::run_native(
        "ShtormTTS",
        options,
        Box::new(|cc| {
            install_cyrillic_fonts(&cc.egui_ctx);
            Ok(Box::new(app::ShtormApp::new()))
        }),
    )
}

fn install_cyrillic_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // Segoe UI содержит кириллицу; берём системную копию
    for cand in [
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\tahoma.ttf",
        r"C:\Windows\Fonts\arial.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(cand) {
            fonts
                .font_data
                .insert("system_ui".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "system_ui".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("system_ui".into());
            break;
        }
    }
    ctx.set_fonts(fonts);
}
