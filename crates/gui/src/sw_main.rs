//! PocketTTS-GUI-Rust-cpu — бинарник с полностью программной отрисовкой GUI
//! на CPU (G6.8): окно растеризуется `egui_software_backend` и выводится
//! через `softbuffer` (на Windows — GDI), БЕЗ OpenGL/WGL, DirectX и GPU.
//! Создан для машин, где eframe/glutin не может создать GL-контекст
//! (Windows 10 x64 22H2: «extension to create ES context with wgl is not
//! present»). Вся функциональность синтеза — та же библиотека
//! `pockettts_gui_rust`; «cpu» в имени относится ТОЛЬКО к отрисовке
//! интерфейса, CPU/CUDA-инференс не отключается.
//!
//! CLI идентичен основному EXE (включая `--portable-*` диагностику и
//! внутренний worker-режим `--internal-batch-worker` — он выполняется до
//! создания окна, поэтому мультипроцессный конвейер G6.7 из cpu-EXE
//! spawning'ит cpu-EXE без графического рантайма).

// Release: без консольного окна (как у основного EXE). Debug — консоль.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use pockettts_gui_rust::entry::{self, CliOutcome};

fn main() {
    entry::init_logging();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match entry::dispatch(&args) {
        CliOutcome::Exit(code) => std::process::exit(code),
        CliOutcome::Gui => std::process::exit(run_software_gui()),
    }
}

/// Запуск окна в программном рантайме (crate::sw). GL-контекст не создаётся
/// ни при каких условиях: в этом бинарнике eframe вообще отсутствует
/// (feature `sw` без `glow`), fallback на OpenGL невозможен by construction.
fn run_software_gui() -> i32 {
    match pockettts_gui_rust::sw::run() {
        Ok(()) => 0,
        Err(e) => {
            entry::gui_error_dialog(&format!("программный рендер (CPU): {e:#}"));
            1
        }
    }
}
