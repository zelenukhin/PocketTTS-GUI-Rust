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
pub mod srt_job;
#[cfg(feature = "sw")]
pub mod sw;
pub mod window;

/// SRT.6: флейвор приложения — обычные EXE (текстовые книги) и SRT-версии
/// (только озвучка субтитров). Определяет имя конфига, заголовок окна,
/// источник и блок настроек SRT; общий код формы/заданий один.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppFlavor {
    Text,
    Srt,
}

impl AppFlavor {
    /// Заголовок окна (единый для обоих рантаймов флейвера).
    pub fn window_title(&self) -> &'static str {
        match self {
            AppFlavor::Text => "PocketTTS-GUI-Rust v1.1",
            AppFlavor::Srt => "PocketTTS-GUI-Rust — SRT v1.1",
        }
    }

    /// Короткое имя для журналов/статусов.
    pub fn label(&self) -> &'static str {
        match self {
            AppFlavor::Text => "PocketTTS-GUI-Rust",
            AppFlavor::Srt => "PocketTTS-GUI-Rust — SRT",
        }
    }
}
