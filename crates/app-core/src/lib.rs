//! Слой оркестрации над движком. На этом этапе: настройки генерации
//! (совместимые по именам полей с settings_pocket_tts.json Python-GUI) и
//! диагностика памяти/VRAM для smoke/bench. Пул воркеров и конвейер файлов —
//! следующий этап (см. docs/NEXT_STEPS.md).

pub mod diagnostics;
pub mod settings;
pub mod trace_dir;

pub use settings::SynthSettings;
