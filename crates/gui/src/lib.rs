//! Библиотечная часть GUI-крейта: форма, настройки, фоновое задание синтеза
//! (юнит-тесты без окна; bin — только запуск рантайма).
//!
//! G6.8: два бинарника на одной библиотеке — `PocketTTS-GUI-Rust`
//! (eframe/glow, OpenGL; feature `glow`) и `PocketTTS-GUI-Rust-cpu`
//! (программная отрисовка CPU, feature `sw`). Общая точка входа/CLI —
//! [`entry`]; общий кадр UI — [`app::ShtormApp::update_ui`].

pub mod app;
pub mod batch;
pub mod chunkpipe;
pub mod cli;
pub mod entry;
#[cfg(windows)]
pub mod hybrid;
pub mod ipc;
pub mod job;
pub mod resources;
pub mod settings;
#[cfg(feature = "sw")]
pub mod sw;
pub mod window;
