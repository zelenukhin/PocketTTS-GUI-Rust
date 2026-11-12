# PocketTTS-GUI-Rust

Нативное (Rust) приложение для Windows x64: движок Shtorm PocketTTS RU
(ONNX) + GUI для синтеза аудиокниг. FP32 **и INT8**, CPU и NVIDIA CUDA,
без Python/PyTorch в рантайме. Готовые сборки — в
[Releases](https://github.com/zelenukhin/PocketTTS-GUI-Rust/releases)
(портативные `v1.0 CPU` / `v1.0 GPU`, установки не требуют).

## Модели

Модель **Shtorm-PocketTTS-RU v2** (fast/slow) взята отсюда:
**https://huggingface.co/ArtShtorm/Shtorm-PocketTTS-RU**

Приложение работает с её ONNX-экспортом (`ru-v2-fast`, `ru-v2-slow`):

- **FP32** — режим по умолчанию, все пять графов FP32;
- **INT8** — явный выбор в списке моделей (динамически-квантованные
  `flow_lm_main/flow_lm_flow/mimi_decoder` + FP32 `mimi_encoder/
  text_conditioner`); на CPU быстрее FP32 на 25–32 % и вдвое легче по
  памяти; на CUDA — экспериментальный режим (квантованные узлы идут на
  CPU EP, ~2.7× медленнее FP32). Молчаливых переключений точности нет:
  отсутствие INT8-графа — явная ошибка.

HuggingFace в рантайме не используется — модели загружаются локально.

## Возможности

- Чанкинг / char_rules (runtime.json) / паузы по пунктуации —
  golden-паритет с Python-оригиналом (96 + 314 + 20 кейсов).
- Потоковая запись **WAV / MP3 / Opus** без накопления аудио в памяти;
  уникальные `.partial`, атомарная публикация, явный флаг перезаписи.
- Безопасный пакетный режим: preflight, защита путей (junction/symlink),
  конфликты имён, skip/overwrite существующих результатов.
- **Единый постоянный чанк-пул P/W/C/T** (G6.7): файлы каталога — строго
  последовательно, чанки текущего файла — параллельно; профили
  «Оптимальный»/«Производительный»/«Свой»; мультипроцессный чанк-конвейер
  (Named Pipes, PCM бинарными кадрами, Job Object — без сиротских
  процессов). Детерминизм: WAV зависит только от модели/точности/T/seed.
- EOS / анти-повторы / «яркость» (high-shelf +8 дБ) / денойз DPDFNet —
  как в teratts-android.
- Настройки — собственный JSON с round-trip неизвестных ключей.

## Сборка из исходников

Требуется Rust (stable, MSVC) и ONNX Runtime 1.23.2 (CPU и/или GPU DLL;
`ORT_DYLIB_PATH` / авто-поиск `deps/ort-*` относительно корня).

```powershell
cargo build --release          # GUI: target\release\PocketTTS-GUI-Rust.exe
cargo test                     # юнит + golden (без GPU)
tools\smoke-cpu.ps1  -ModelDir <onnx\ru-v2-slow> -Voice <voice.wav>
tools\smoke-cuda.ps1 -ModelDir <onnx\ru-v2-slow> -Voice <voice.wav>
tools\build-portable.ps1 -?    # портативный комплект
```

## Лицензия и происхождение

Движок — производное [teratts-android](https://github.com/9208499-ship-it/teratts-android)
(ветка `teratts`, commit `edb614e8…`), лицензия **GPL-3.0** (полный текст —
`LICENSE-TERATTS-ANDROID`, происхождение файлов — в шапках исходников).
Модель Shtorm-PocketTTS-RU — [ArtShtorm/Shtorm-PocketTTS-RU](https://huggingface.co/ArtShtorm/Shtorm-PocketTTS-RU).

## Документация

Архитектура и этапы разработки — в `docs/` (`G6_7_UNIFIED_CHUNK_POOL.md` —
актуальная архитектура параллелизма, `ARCHITECTURE.md`, `BENCHMARKS.md`,
`SAFETY_AUDIT.md`, `PORTABLE_BUILD.md` и др.).
