//! Слой оркестрации над движком. На этом этапе: настройки генерации
//! (совместимые по именам полей с settings_pocket_tts.json Python-GUI) и
//! диагностика памяти/VRAM для smoke/bench. Пул воркеров и конвейер файлов —
//! следующий этап (см. docs/NEXT_STEPS.md).

pub mod diagnostics;
pub mod persistent;
pub mod pipeline;
pub mod pool;
pub mod settings;
pub mod sink;
pub mod trace_dir;
pub mod worker;

pub use settings::SynthSettings;

/// G6.2.3: спавн консольного дочернего процесса (ffmpeg, nvidia-smi) из
/// GUI-подсистемы Windows без всплывающего окна консоли. Явно
/// перенаправленные stdio (piped/null/output) продолжают работать;
/// ExitCode не меняется. GUI-бинар с G6.2.3 собран подсистемой `windows` —
/// без этого флага ОС выделяла бы каждому консольному ребёнку собственное
/// консольное окно.
#[cfg(windows)]
pub fn hide_console(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Не-Windows: флаг создания процесса отсутствует — no-op.
#[cfg(not(windows))]
pub fn hide_console(_cmd: &mut std::process::Command) {}
