//! PocketTTS-GUI-Rust-srt-cpu — SRT-версия бинарника с полностью
//! программной отрисовкой GUI на CPU (без OpenGL/WGL/DirectX): для машин,
//! где eframe/glutin не может создать GL-контекст. Функциональность — та
//! же библиотека `pockettts_gui_rust` (флейвор `AppFlavor::Srt`);
//! «cpu» относится ТОЛЬКО к отрисовке, CPU/CUDA-инференс не отключается.
//!
//! Конфиг — общий с glow-SRT EXE `PocketTTS-GUI-Rust-srt.json`.

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
        CliOutcome::Gui => std::process::exit(run_software_srt_gui()),
    }
}

fn run_software_srt_gui() -> i32 {
    match pockettts_gui_rust::sw::run_flavor(AppFlavor::Srt) {
        Ok(()) => 0,
        Err(e) => {
            entry::gui_error_dialog(&format!("программный рендер (CPU, SRT): {e:#}"));
            1
        }
    }
}
