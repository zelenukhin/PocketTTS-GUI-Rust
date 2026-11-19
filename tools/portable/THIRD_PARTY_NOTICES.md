# Third-party notices — PocketTTS-GUI-Rust portable build

This package is assembled locally from the project sources and the following
third-party components. It is not published to the Internet.

## PocketTTS-GUI-Rust (this project)

License: GNU General Public License v3.0 only (GPL-3.0-only).
Source: the project repository from which this package was built.
See `LICENSES/GPL-3.0.txt`.

## ONNX Runtime 1.23.2

License: MIT. Copyright (c) Microsoft Corporation.
Files: `onnxruntime/cpu/*`, `onnxruntime/cuda/*`.
See `LICENSES/ONNX-Runtime-LICENSE.txt` and
`LICENSES/ONNX-Runtime-ThirdPartyNotices.txt`.

## CUDA Toolkit / cuDNN runtime DLLs

Files: `cudart64_12.dll`, `cublas64_12.dll`, `cublasLt64_12.dll`,
`cufft64_11.dll`, `cudnn*64_9.dll` in `onnxruntime/cuda/`.
These are NVIDIA redistributable runtime components. Their use and
redistribution are governed by the NVIDIA CUDA Toolkit End User License
Agreement and the cuDNN Software License Agreement. See
`LICENSES/NVIDIA-CUDA-NOTICE.txt` and the NVIDIA documentation:
  https://docs.nvidia.com/cuda/eula/index.html
  https://docs.nvidia.com/deeplearning/cudnn/latest/reference/eula.html

## Microsoft Visual C++ Redistributable runtime

Files: `msvcp140.dll`, `msvcp140_1.dll`, `vcruntime140.dll`,
`vcruntime140_1.dll` next to `PocketTTS-GUI-Rust.exe`.
These are Microsoft redistributable components, included so that a clean
Windows installation can start the application without a separately installed
VC++ Redistributable. See `LICENSES/MSVC-RUNTIME-NOTICE.txt`.

## FFmpeg 8.0.1 (gyan.dev full build)

License: GPLv3 (build configured with `--enable-gpl --enable-version3`).
File: `ffmpeg/ffmpeg.exe`.
The corresponding source code is available from https://ffmpeg.org/ and the
build provider https://www.gyan.dev/ffmpeg/builds/. See
`LICENSES/FFmpeg-GPLv3.txt` and `LICENSES/FFmpeg-NOTICE.txt`.

## SoundTouch 2.4.1

License: GNU Lesser General Public License v2.1 (LGPL-2.1).
File: `SoundTouchDLL.dll` (official C wrapper, SoundTouch 2.4.1,
SHA256 e8e56d3db0d72eed6f4ded298a31058398945f2c72cda6e8dc29b67f1330c3f6).
Compliance: the library is linked DYNAMICALLY — loaded at runtime with
`LoadLibraryW`/`GetProcAddress` only when an SRT cue must be accelerated;
no EXE in this package contains a static import of SoundTouch. Per LGPL 2.1
§4 you may replace `SoundTouchDLL.dll` with a compatible build of the same
library version. The exact source code shipped is
`third_party/soundtouch/soundtouch-2.4.1.tar.gz` (upstream tarball, SHA256
e07abf20ce8f95850c280132e1f61ad400fc1f4011b7fac698a503de6aab6733) of the
source project, together with the reproducible build script
`tools/build-soundtouch.ps1` (MSVC x64, /Brepro). See
`LICENSES/SoundTouch-LGPL-2.1.txt` and `LICENSES/SoundTouch-NOTICE.txt`.
Home page: https://codeberg.org/soundtouch/soundtouch

## Shtorm-PocketTTS-RU v2 fast/slow models

Files: `models/ru-v2-fast/*`, `models/ru-v2-slow/*`.
No explicit license file was found in the model source directory. The models
are included for the owner's personal/local use only. Do NOT redistribute
these model weights without explicit permission from their author. If you do
not have redistribution rights, remove the `models/` directory and place your
own licensed models there. See `LICENSES/MODEL-NOTICE.txt`.

## Voice reference

`voice/reference.wav` is user-supplied. The portable build does not claim a
redistribution license for any sample voice. Replace it with a WAV you have
the right to use.

## Rust dependencies

The GUI binary statically links Rust crates listed in `Cargo.lock` of the
source project. Their licenses are available from crates.io and the respective
projects. The GPL-3.0-only source of this project is the authoritative source
for the built binary.
