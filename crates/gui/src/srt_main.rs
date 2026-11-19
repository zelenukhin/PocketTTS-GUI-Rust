//! PocketTTS-GUI-Rust-srt — SRT-версия основного бинарника (eframe/glow,
//! OpenGL): озвучка файлов субтитров .srt с выравниванием по таймкодам
//! (SoundTouch-ускорение без изменения высоты голоса). Вся логика — в
//! библиотеке `pockettts_gui_rust` (флейвор `AppFlavor::Srt`); конфиг —
//! отдельный `PocketTTS-GUI-Rust-srt.json` (обычный конфиг не трогается).
//!
//! Требует SoundTouchDLL.dll рядом с exe ПРИ необходимости ускорения
//! реплик (без неё файлы, влезающие в свои окна, озвучиваются без
//! ускорения; первой «переполненной» реплике — явная ошибка).
//!
//! CLI идентичен основному EXE (`--portable-*` читают SRT-конфиг).

// Release: без консольного окна (как у основного EXE). Debug — консоль.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use pockettts_gui_rust::entry::{self, CliOutcome};
use pockettts_gui_rust::AppFlavor;

fn main() {
    entry::init_logging();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match entry::dispatch_flavor(&args, AppFlavor::Srt) {
        CliOutcome::Exit(code) => std::process::exit(code),
        CliOutcome::Gui => std::process::exit(run_glow_srt_gui()),
    }
}

fn run_glow_srt_gui() -> i32 {
    let plan = pockettts_gui_rust::window::initial_plan();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(plan.inner_size)
            .with_min_inner_size(plan.min_inner_size)
            .with_title(AppFlavor::Srt.window_title()),
        ..Default::default()
    };
    match eframe::run_native(
        AppFlavor::Srt.label(),
        options,
        Box::new(|cc| {
            entry::install_cyrillic_fonts(&cc.egui_ctx);
            pockettts_gui_rust::window::place_on_launch_monitor(cc);
            Ok(Box::new(pockettts_gui_rust::app::ShtormApp::new_srt()))
        }),
    ) {
        Ok(()) => 0,
        Err(e) => {
            entry::gui_error_dialog(&format!("eframe/glow (SRT): {e}"));
            1
        }
    }
}
