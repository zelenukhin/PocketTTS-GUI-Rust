ShtormTTS-GUI — portable Windows x64 build
==========================================

Run: shtorm-gui.exe

Layout:
  msvcp140.dll, msvcp140_1.dll,
  vcruntime140.dll, vcruntime140_1.dll  MSVC++ redistributable runtime
  shtorm-gui.exe                       GUI executable
  models/ru-v2-fast, models/ru-v2-slow  ONNX models (FP32 only)
  onnxruntime/cpu/onnxruntime.dll       CPU ONNX Runtime 1.23.2
  onnxruntime/cuda/                     CUDA ONNX Runtime + providers + CUDA/cuDNN DLLs
  ffmpeg/ffmpeg.exe                     MP3/Opus encoder (GPLv3 build)
  voice/reference.wav                   Voice reference; replace with your own 1-30 s WAV
  input/example.txt                     Example source text
  output/                               Default result directory
  shtorm_gui_settings.json              Default settings with RELATIVE paths

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
    the application.
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
  * ONNX Runtime is MIT-licensed; see LICENSES/ONNX-Runtime-LICENSE.txt.
  * CUDA/cuDNN DLLs are NVIDIA redistributables; see
    LICENSES/NVIDIA-CUDA-NOTICE.txt.

Diagnostics:
  Diagnostic output is written to stdout/stderr. The GUI is a Windows GUI
  subsystem application (no console window), so redirect the streams to
  capture the output, e.g. in PowerShell:
      & .\shtorm-gui.exe --portable-check > check.txt 2>&1
  or use System.Diagnostics.Process with RedirectStandardOutput/Error.
  Exit codes are preserved (0 OK, 1 check failed, 2 bad arguments).

  shtorm-gui.exe --portable-check
      Print resolved model / ORT / ffmpeg paths and exit.
  shtorm-gui.exe --portable-selftest [--device cpu|cuda] [--format wav|mp3|opus]
      Run a short synthesis from the portable directory and exit.
  shtorm-gui.exe --portable-selftest-batch [--device cpu|cuda]
      Run a two-file batch into a new output subdirectory and exit.
  shtorm-gui.exe --portable-selftest-cancel [--device cpu|cuda]
      Run a multi-chunk job, cancel after the first finished chunk, verify
      that the final file is not published and the draft remains, then re-run
      on the same path and verify completion.
