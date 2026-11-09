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
  * CPU and CUDA use different ONNX Runtime DLLs. A process can initialize only
    one runtime; after switching CPU <-> CUDA inside one GUI session, restart
    the application.
  * Existing audio files are never overwritten.
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
  shtorm-gui.exe --portable-check
      Print resolved model / ORT / ffmpeg paths and exit.
  shtorm-gui.exe --portable-selftest [--device cpu|cuda] [--format wav|mp3|opus]
      Run a short synthesis from the portable directory and exit.
  shtorm-gui.exe --portable-selftest-batch [--device cpu|cuda]
      Run a two-file batch into a new output subdirectory and exit.
