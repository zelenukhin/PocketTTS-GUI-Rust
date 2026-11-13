//! PocketTTS-GUI-Rust — основной бинарник с рендерингом через eframe/glow
//! (OpenGL). Логика (CLI/диагностика/форма/задание синтеза) — в библиотеке
//! `pockettts_gui_rust`; сюда входит только запуск оконного рантайма.
//!
//! G6.8: второй бинарник `PocketTTS-GUI-Rust-cpu` (src/sw_main.rs) использует
//! ту же библиотеку с программной отрисовкой на CPU (без OpenGL) — вся
//! функциональность, CLI и дочерние worker-процессы идентичны.
//!
//! G6.1.1: диспетчеризация аргументов строго проверяется (`pockettts_gui_rust::cli`)
//! — неизвестный/повторный/некорректный аргумент завершает процесс с кодом 2
//! и diagnostic-сообщением, окно НЕ открывается (раньше опечатка во флаге
//! молча запускала GUI).
//!
//! G6.2.3: release-бинар собирается подсистемой Windows GUI — консольное
//! окно не открывается ни при обычном запуске, ни у дочерних CUDA-процессов.
//! Диагностика `--portable-*` читается через явно перенаправленные
//! stdout/stderr (System.Diagnostics.Process, PowerShell-конвейер, pipes);
//! ошибки CLI при запуске без потоков (двойной клик) показываются
//! MessageBox (`cli_error_output`). Debug-сборка остаётся консольной —
//! логи разработки видны в терминале.

// Release: без консольного окна. Debug: консольная подсистема (логи dev).
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
        CliOutcome::Gui => std::process::exit(run_glow_gui()),
    }
}

/// Запуск окна через eframe/glow (OpenGL). Любая ошибка рантайма (например,
/// невозможность создать GL-контекст) печатается в stderr; при запуске
/// двойным кликом (нет stderr) — MessageBox. Код выхода 1.
fn run_glow_gui() -> i32 {
    // G6.4: стартовый размер — по рабочей области монитора запуска и его
    // DPI (не фиксированные 980×860). Окно не выходит за экран, на большом
    // мониторе не растягивается на весь экран, min_inner_size уменьшается
    // на маленьких дисплеях. Позиционирование — однократно в creation
    // callback (см. window.rs), без resize-циклов в update().
    let plan = pockettts_gui_rust::window::initial_plan();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(plan.inner_size)
            .with_min_inner_size(plan.min_inner_size)
            .with_title("PocketTTS-GUI-Rust v1.0"),
        ..Default::default()
    };
    match eframe::run_native(
        "PocketTTS-GUI-Rust",
        options,
        Box::new(|cc| {
            entry::install_cyrillic_fonts(&cc.egui_ctx);
            // Окно ещё скрыто (eframe показывает его после первого кадра):
            // переносим на монитор запуска и выравниваем размер до первого
            // отображения — без мерцания и повторных подстроек.
            pockettts_gui_rust::window::place_on_launch_monitor(cc);
            Ok(Box::new(pockettts_gui_rust::app::ShtormApp::new()))
        }),
    ) {
        Ok(()) => 0,
        Err(e) => {
            entry::gui_error_dialog(&format!("eframe/glow: {e}"));
            1
        }
    }
}
