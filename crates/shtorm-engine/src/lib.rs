//! Shtorm PocketTTS RU — движок для Windows (FP32, CPU/CUDA).
//!
//! ПРОИСХОЖДЕНИЕ КОДА: teratts-android (GPL-3.0),
//! https://github.com/9208499-ship-it/teratts-android, ветка `teratts`,
//! commit edb614e81186ea82a27294b34d567f1b4937bb20:
//! - `pocket.rs`  <- rust/src/pocket.rs   (адаптация: FP32-only, Device, seed; см. шапку файла)
//! - `denoise.rs` <- rust/src/denoise.rs  (сессия через локальный билдер, CPU)
//! - `dsp.rs`     <- rust/src/tera.rs     (trim_edges / fade_out, fs — параметр)
//!
//! Загрузка ONNX Runtime — динамическая (feature `load-dynamic`): путь к dylib
//! задаётся переменной окружения `ORT_DYLIB_PATH` до первого обращения к ort.
//! Для CUDA нужен GPU-сборка onnxruntime.dll + библиотеки CUDA 12 / cuDNN 9,
//! доступные процессу (например, через PATH) — см. tools/smoke-cuda.ps1.

pub mod denoise;
pub mod dsp;
pub mod pocket;

pub use pocket::CleanOutcome;
pub use pocket::{
    high_shelf, read_voice, resample, Bpe, Device, LoadOptions, Pocket, POCKET_MAX_CHUNK_CHARS,
    POCKET_RATE,
};
