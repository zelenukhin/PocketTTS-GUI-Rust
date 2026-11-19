PocketTTS-GUI-Rust — portable Windows x64 build
===============================================

This package contains FOUR executables:

  PocketTTS-GUI-Rust.exe        Text (TXT/MD/LOG) narration, OpenGL rendering
  PocketTTS-GUI-Rust-cpu.exe    Text narration, software (CPU) rendering
  PocketTTS-GUI-Rust-srt.exe    Subtitle (.srt) narration, OpenGL rendering
  PocketTTS-GUI-Rust-srt-cpu.exe Subtitle narration, software rendering

No OpenGL / no GPU driver? Use a "-cpu" executable instead: it renders the
whole interface on the CPU (GDI) and never touches OpenGL, Direct3D or the
GPU. "cpu" in the file name refers ONLY to drawing the GUI — CPU and CUDA
inference work the same as in the OpenGL builds; a -cpu EXE does NOT
disable CUDA.

Text vs SRT executables:
  * The Text executables narrate plain text files (TXT/MD/LOG) with the
    Standard or "TeraTTS Android" chunking algorithm, Fast/Slow models,
    FP32/INT8 precision, single-file and batch modes.
  * The SRT executables narrate .srt subtitles only: they read timecodes,
    clean markup, align speech to the subtitle timeline (SoundTouch
    tempo acceleration without pitch change) and write a timing log.
    A .txt file is not accepted as an SRT source.
  * Settings are separate: PocketTTS-GUI-Rust.json for both Text EXEs,
    PocketTTS-GUI-Rust-srt.json for both SRT EXEs (created on first run).
    An SRT executable never reads or writes the Text config and vice versa.

Layout:
  msvcp140.dll, msvcp140_1.dll,
  vcruntime140.dll, vcruntime140_1.dll  MSVC++ redistributable runtime
  PocketTTS-GUI-Rust.exe                 Text GUI (eframe/glow, OpenGL)
  PocketTTS-GUI-Rust-cpu.exe             Text GUI (software CPU rendering,
                                       no OpenGL/D3D/GPU required)
  PocketTTS-GUI-Rust-srt.exe             SRT GUI (eframe/glow, OpenGL)
  PocketTTS-GUI-Rust-srt-cpu.exe         SRT GUI (software CPU rendering)
  SoundTouchDLL.dll                      SoundTouch 2.4.1 (SRT tempo
                                       acceleration; loaded at runtime ONLY
                                       by the SRT EXEs when a cue must be
                                       sped up; the Text EXEs never load it)
  models/ru-v2-fast, models/ru-v2-slow   ONNX models (FP32 default; also
                                       *_int8.onnx graphs: flow_lm_main_int8,
                                       flow_lm_flow_int8, mimi_decoder_int8 —
                                       the INT8 model variants; mimi_encoder
                                       and text_conditioner stay FP32 in both
                                       modes, quantized versions do not exist)
                                       runtime.json in each model directory
                                       holds the Standard-mode char_rules.
  onnxruntime/cpu/onnxruntime.dll       CPU ONNX Runtime 1.23.2
  onnxruntime/cuda/                     CUDA ONNX Runtime + providers + CUDA/cuDNN DLLs
  ffmpeg/ffmpeg.exe                     MP3/Opus encoder (GPLv3 build)
  voice/reference.wav                   Voice reference; replace with your own 1-30 s WAV
  input/example.txt                     Example source text
  output/                               Default result directory
  PocketTTS-GUI-Rust.json                Default Text settings with RELATIVE paths

Important:
  * Relative paths in settings are resolved from the EXE directory, not from
    the current working directory. You can launch the EXE from any folder or
    shortcut.
  * Settings are saved AUTOMATICALLY when you close the window normally
    (X button / Alt+F4). The "Сохранить настройки" button is only an
    immediate manual save. If the settings file cannot be read at startup,
    it is never overwritten automatically (the reason is shown in the
    status bar).
  * The application runs without a console window (Windows GUI subsystem);
    child CUDA worker processes and ffmpeg also do not open console windows.
  * CPU and CUDA use different ONNX Runtime DLLs. A process can initialize only
    one runtime; after switching CPU <-> CUDA inside one GUI session, restart
    the application. CUDA mode additionally requires a compatible installed
    NVIDIA display driver (the driver itself is NOT part of this package).
  * Model variants (G6.3): the "Модель" list combines the model and precision —
    "Shtorm-v2 Fast/Slow (FP32)" is the original mode (default); "(INT8)"
    selects the quantized main/flow/decoder graphs plus FP32 encoder/conditioner.
    INT8 is noticeably faster on CPU; on CUDA it WORKS but is measured ~2.7x
    SLOWER than FP32 (quantized nodes run on the CPU EP with per-frame PCIe
    copies) — the GUI warns about this combination and never switches the
    selection automatically. Missing INT8 files is an explicit error; there is
    no silent fallback to FP32.
  * SRT alignment (SRT EXEs): "Максимальное ускорение" (max tempo, x1.1..x3.0,
    default x2.0) limits how much an overlong cue may be compressed;
    "Минимальная пауза" (0..500 ms) keeps a gap to the next cue when borrowing
    time. Cues that fit their window are never processed by SoundTouch
    (tempo = 1.0). If even the maximum tempo is not enough, the speech is NOT
    cut: the cue simply runs late and this is recorded in the timing log
    ({name}_log.txt). SoundTouchDLL.dll must stay next to the SRT EXEs;
    without it, files whose cues all fit their windows still work, but the
    first overlong cue is an explicit error.
  * The SRT batch mode processes files strictly one by one (multi-process
    chunk pipeline P>1 is not used for SRT; a single-process chunk pool is
    used instead — a documented limitation).
  * Existing audio files are NOT overwritten by default: an existing result
    is skipped. If you explicitly enable "Перезаписывать существующие
    файлы" (overwrite existing files), a finished file is replaced
    atomically ONLY after a successful synthesis; on error/cancel the old
    file is left intact.
  * The voice reference is NOT included with a known redistribution license.
    Put your own WAV at voice/reference.wav.
  * Model weights have no explicit license file in the source tree. This
    package is intended for the owner's local use. Do not redistribute the
    models without permission.
  * FFmpeg is a GPLv3 build; see LICENSES/FFmpeg-GPLv3.txt and
    LICENSES/FFmpeg-NOTICE.txt.
  * SoundTouch is LGPL-2.1; it is linked DYNAMICALLY (LoadLibraryW at
    runtime, no static import in any EXE). See LICENSES/SoundTouch-LGPL-2.1.txt
    and LICENSES/SoundTouch-NOTICE.txt. You may replace SoundTouchDLL.dll
    with a compatible build of the same version; the source code is
    third_party/soundtouch/soundtouch-2.4.1.tar.gz of the source project.
  * ONNX Runtime is MIT-licensed; see LICENSES/ONNX-Runtime-LICENSE.txt.
  * CUDA/cuDNN DLLs are NVIDIA redistributables; see
    LICENSES/NVIDIA-CUDA-NOTICE.txt.

Diagnostics:
  Diagnostic output is written to stdout/stderr. The GUI is a Windows GUI
  subsystem application (no console window), so redirect the streams to
  capture the output, e.g. in PowerShell:
      & .\PocketTTS-GUI-Rust.exe --portable-check > check.txt 2>&1
  or use System.Diagnostics.Process with RedirectStandardOutput/Error.
  Exit codes are preserved (0 OK, 1 check failed, 2 bad arguments).
  ALL FOUR executables accept the same diagnostic commands; the SRT EXEs
  apply them to the SRT settings file.

  PocketTTS-GUI-Rust.exe --portable-check
      Print resolved model / ORT / ffmpeg paths and exit.
  PocketTTS-GUI-Rust.exe --portable-selftest [--device cpu|cuda] [--format wav|mp3|opus]
      Run a short synthesis from the portable directory and exit.
  PocketTTS-GUI-Rust.exe --portable-selftest-batch [--device cpu|cuda]
      Run a two-file batch into a new output subdirectory and exit.
  PocketTTS-GUI-Rust.exe --portable-selftest-cancel [--device cpu|cuda]
      Run a multi-chunk job, cancel after the first finished chunk, verify
      that the final file is not published and the draft remains, then re-run
      on the same path and verify completion.
