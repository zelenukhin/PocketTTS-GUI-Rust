//! Библиотечная часть GUI-крейта: форма, настройки, фоновое задание синтеза
//! (юнит-тесты без окна; bin — только запуск eframe).

pub mod app;
pub mod batch;
pub mod cli;
#[cfg(windows)]
pub mod hybrid;
pub mod ipc;
pub mod job;
pub mod resources;
pub mod settings;
pub mod window;
