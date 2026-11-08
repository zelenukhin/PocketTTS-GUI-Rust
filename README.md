# ShtormTTS-Windows

Нативный (Rust) движок Shtorm PocketTTS RU для Windows: FP32, CPU и CUDA, без
Python/PyTorch в рантайме. Каркас этапа «подготовка» — GUI будет позже.

- Движок — производное [teratts-android](https://github.com/9208499-ship-it/teratts-android)
  (ветка `teratts`, commit `edb614e8…`), лицензия **GPL-3.0** (см.
  `LICENSE-TERATTS-ANDROID`, происхождение файлов — в шапках исходников).
- Модели: локальные ONNX-каталоги Shtorm-v2 (`build/onnx/ru-v2-fast`, `ru-v2-slow`);
  загрузчик строго FP32 (int8-файлы игнорируются), HuggingFace не используется.
- onnxruntime подключается динамически: `ORT_DYLIB_PATH` → onnxruntime.dll 1.23.2
  (CPU- или GPU-сборка); для CUDA нужны DLL CUDA 12 + cuDNN 9 в PATH процесса.

## Быстрый старт

```powershell
tools\build.ps1                                   # сборка + тесты
tools\smoke-cpu.ps1  -ModelDir <onnx\ru-v2-slow> -Voice <voice.wav>
tools\smoke-cuda.ps1 -ModelDir <onnx\ru-v2-slow> -Voice <voice.wav>
# бенчмарк: target\release\bench.exe --model-dir … --voice … --device cuda --chunks 100 --tag x
# стресс  : bench.exe … --text-file book.txt --chunks 1000 --csv out\stress.csv --sample-every 10
# TF32/EP-проба: target\release\ep_probe.exe --model-dir … --voice …   (нужен GPU-dylib)
# график памяти: venv-питоном tools\plot_memory.py stress_cuda_slow.csv …
# golden чанкования: venv-питоном tools\gen_chunking_golden.py (git-ветка comma-semantics)
```

Движок всегда строгий FP32: `use_tf32=0` передаётся CUDA EP явно (дефолт
провайдера — включён, доказано ep_probe). int8-модели игнорируются.

Структура, решения, статус: `docs/PREPARATION_REPORT.md`, `docs/ARCHITECTURE.md`,
`docs/CUDA_FEASIBILITY.md`, `docs/FEATURE_PARITY.md`, `docs/BENCHMARKS.md`,
`docs/NEXT_STEPS.md`.
