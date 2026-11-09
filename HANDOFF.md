# HANDOFF.md — оперативный снимок проекта (читать первым вместе с AGENTS.md)

- **Дата обновления**: 2026-11-09.
- **HEAD**: `38507a8` (G1). Рабочее дерево: **чистое** (0 изменений).
- **Бэкап HEAD на NAS**: `\\DISKSTATION\Ableton 2\Temp2\ShtormTTS-Windows-backup-38507a8.zip`
  (113 файлов, CRC OK, верифицирован сравнением).

## Цель проекта

Нативный Windows x64 GUI на Rust (eframe/egui) для синтеза аудиокниг
моделями Shtorm-PocketTTS-RU v2 fast/slow (ONNX FP32, CPU/CUDA), потоковая
запись WAV/MP3/Opus без накопления аудио в памяти — функциональный порт
Python `pocket-tts_gui.py`. Постоянные правила и архитектура — `AGENTS.md`.

## Завершённые этапы (кратко; история — git log, детали — docs/)

- **A1** чанкование (golden 96, ветка comma-semantics Python).
- **A2** char_rules (golden 314; хвостовая запятая сохраняется).
- **A3** паузы teratts + склейка (golden 20; исправление: конец абзаца
  приоритетнее soft_break — задокументированное расхождение с Python-багом).
- **A4** пул воркеров (N × свой Pocket, скользящее окно backpressure,
  seed=base+индекс; детерминизм CPU/CUDA 1w≡2w побитово).
- **A5/A5.1/A5.2** потоковый вывод: StreamingWav + FfmpegSink (writer-поток,
  wait_with_timeout, kill-before-join), уникальные `.partial`,
  publish_no_replace (MoveFileW), RunStatus/StreamError/DiagMeta,
  диагностические журналы, Ctrl+C exit=2.
- **G1** каркас GUI (`crates/gui`, eframe 0.31): форма+валидация+настройки
  (round-trip unknown keys, атомарная запись, импорт Python read-only),
  кириллица (Segoe UI), anti_repeat реально в Pocket. Синтез из GUI — G2.

## Подтверждённые результаты

- `cargo test` **91 passed / 0 failed**; `cargo clippy --all-targets` **0**;
  сборка release OK (в т.ч. `shtorm-gui.exe` 7.2 МБ).
- WAV-паритет: потоковый A5 == A4 побитово (при одинаковых `--threads`;
  разное число intra-op потоков ORT меняет порядок редукций — это норма).
- Детерминизм: CPU и CUDA, 1w vs 2w — побитово (3 и 47 чанков).
- MP3 (`-V2`): ffprobe 24 kHz mono, 18.455 с, декод OK. Opus (`opus 64k`):
  48 kHz mono, 18.4615 с, декод OK (обе задержки — кодек, ≤ кадра).
- Память (доказательство streaming): 18.45 с vs 262.39 с текст, 1 воркер —
  пик Private 1591→1095 МБ (не растёт), буфер пула ≤ 856 КиБ.
- RTF (ru-v2-slow, 47 чанков): CPU 1w 0.728 / 2w 0.558; CUDA 1w 0.531 /
  2w 0.465. Рекомендация: CPU 2w, CUDA 1w.
- Сквозная отмена (WAV и MP3): exit=2, итогового файла нет, `.partial` и
  диагностический журнал (прогресс+seed) сохранены, сиротских ffmpeg нет.

## Состояние подсистем

- **CPU FP32** — работает (ORT CPU dll). **CUDA FP32** — работает
  (`use_tf32=0` принудительно; VRAM-дельта — обязательная проверка GPU,
  WDDM не отдаёт per-process). Денойз DPDFNet: код готов; без файла модели —
  явный статус Missing (реального прогона с моделью ещё не было).
- **WAV/MP3/Opus streaming** — готов; ffmpeg резолв: явный путь → рядом с
  exe → `../ffmpeg` → PATH; портативный: `C:\PocketTTS-GUI\ffmpeg\ffmpeg.exe`.

## Структура и ключевые файлы

```
Cargo.toml            # workspace: shtorm-engine, text-processing,
                      #   audio-processing, app-core, gui
crates/shtorm-engine/src/pocket.rs     # движок (FP32-only, Device, seed)
crates/text-processing/src/{chunking,char_rules,pauses}.rs
crates/audio-processing/src/stream.rs  # StreamingWav, publish_no_replace
crates/app-core/src/pipeline.rs        # synthesize_stream, RunStatus,
                                       #   StreamError, DiagMeta, write_diag_log
crates/app-core/src/pool.rs            # run_job (воркеры, окно, отмена)
crates/app-core/src/worker.rs          # real_worker_factory (Pocket+DSP)
crates/app-core/src/sink.rs            # FfmpegSink (writer-поток, abort/Drop)
crates/gui/src/{app,settings,main}.rs  # G1-каркас
tools/*.ps1, tools/gen_*_golden.py     # smoke и генераторы golden
tests/golden/*.json                    # 96 + 314 + 20 кейсов
docs/SAFETY_AUDIT.md, docs/GUI_PLAN.md, docs/NEXT_STEPS.md,
docs/FEATURE_PARITY.md, docs/BENCHMARKS.md
```

## Сборка / запуск / smoke (Windows, Git Bash)

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release                       # всё
cargo test && cargo clippy --all-targets    # норма: 0 failed / 0 warning
# smoke CPU:
ORT_DYLIB_PATH="C:\PocketTTS-GUI-Rust\ShtormTTS-Windows\deps\ort-cpu\
onnxruntime-win-x64-1.23.2\lib\onnxruntime.dll" ./target/release/smoke.exe \
  --model-dir <onnx\ru-v2-slow> --voice <wav> --text "Стр+елки час+ов." \
  --out out\smoke_cpu.wav --device cpu
# smoke CUDA: то же + PATH с deps\ort-gpu-wheel\x\onnxruntime\capi и
#   venv\...\torch\lib (cudart/cublas/cudnn), --device cuda
# CLI синтеза: ./target/release/synthesize.exe --help  (форматы/воркеры/пресеты)
# GUI: ./target/release/shtorm-gui.exe   (G1: форма без запуска синтеза)
```
Зависимости: Rust MSVC (rustup, компоненты rustfmt+clippy), ORT dylib в
`deps/ort-cpu|ort-gpu-wheel` (1.23.2; ort-крейт пин rc.11), CUDA/cuDNN DLL
— из torch-venv (портативный комплект `deps/portable`, 15 DLL, проверен),
ffmpeg — `C:\PocketTTS-GUI\ffmpeg`. Модели — см. AGENTS.md.

## Актуальные ограничения / нерешённое

- Auto-EOS (`runtime.json`-резолвер), `frames_after_eos`, пакетный режим
  (walkdir), перезапуск воркеров каждые N файлов, CUDA I/O Binding,
  портативная сборка — **не начаты** (не имитировать наличие).
- Денойз не прогонялся с реальной моделью DPDFNet; decode_steps>1 не мерили.
- WAV > ~4 ГиБ — явная ошибка (RF64/разбиение — будущая функция).
- GUI: диалоги rfd модальны (UI ждёт выбора — приемлемо, но не «повесить»
  долгую работу в UI-потоке).

## NEXT TASK — фиксы перед G2 + подключение синтеза к GUI

**Сначала исправить (дефекты подтверждены чтением кода 2026-11-09):**

1. `crates/app-core/src/sink.rs` — **два разных `Arc<AtomicU64>` для
   `bytes_fed`**: локальный `fed` (строка ~291, инкрементируется писателем) и
   поле структуры `bytes_fed` (~348, отдельный экземпляр, всегда 0; он же
   читается в сообщениях об ошибках). Объединить: один Arc, клон в писателя,
   поле структуры — тот же счётчик. Добавить тест на ненулевой bytes_fed
   после feeding (или доступность в сообщении об ошибке).
2. `crates/gui/src/settings.rs::save_settings` — tmp создаётся как
   `ensure!(!tmp.exists())` + `fs::write` (гонка). Заменить на
   **эксклюзивное создание** `OpenOptions::new().write(true).create_new(true)`
   (как write_wav16), сохраняя атомарный MoveFileExW после успеха.
3. `crates/gui/src/app.rs` — состояния сворачиваемых секций
   (`source_open/gen_open/out_open`) читаются для `default_open`, но
   **никогда не записываются** при переключении пользователем → в конфиг
   сохраняются стартовые значения. Сохранять фактические состояния
   (колбэк `CollapsingHeader::show` → обновлять поля перед save).
4. `pipeline.rs::synthesize_stream` — **проверить на реальном аварийном
   пути** (не мок), что ошибка `run_job()` действительно оборачивается в
   `StreamError` с корректным `chunks_done` (сейчас `done_ref` считается в
   progress-callback; убедиться, что sink-ошибки/воркер-провал дают
   реальный прогресс, а не 0; при необходимости скорректировать источник
   значения). Юнит-тест с моком уже есть; нужна проверка кодом чтения +
   при возможности smoke с заведомо битым путем модели.

**Затем G2 (реальный синтез из GUI):**

5. Кнопка «Запустить» (валидность уже проверяется) → в фоне (не UI-поток):
   инициализация ORT (`ort::init()` в процессе GUI; `ORT_DYLIB_PATH` —
   задать до init из настроек/поиска как в `resolve_ffmpeg`), выбор модели
   fast/slow/свой каталог, `PipelineOptions` **паритет CLI** (все поля,
   включая `anti_repeat`, `eos_threshold=-4.0` тератт-режим как в CLI),
   `make_sink` (WAV/MP3/Opus; ffmpeg через `resolve_ffmpeg`), вызов
   `app_core::pipeline::synthesize_stream` напрямую (не subprocess!).
6. Прогресс `done/total` → полоса (канал в UI-поток + `ctx.request_repaint`).
7. «Стоп» → общий `Arc<AtomicBool>` → статус Cancelled, путь `.partial`
   и диагностического журнала — в UI.
8. Статусы Completed/Cancelled/Failed и итоговая строка (RTF, паузы,
   денойз) — в журнал UI.
9. GUI **не должен** запускать внешний `synthesize.exe` — только app-core.

**Не начинать**: Auto-EOS/frames_after_eos, пакетную обработку,
CUDA I/O Binding, портативную сборку, длительные бенчмарки.

## Файлы для чтения перед NEXT TASK (по порядку)

1. `AGENTS.md` (правила/безопасность), этот файл.
2. `crates/app-core/src/pipeline.rs` — `synthesize_stream`, `StreamError`.
3. `crates/app-core/src/sink.rs` — `FfmpegSink` (writer/abort/finalize,
   `bytes_fed` — дефект №1).
4. `crates/app-core/src/pool.rs` — `run_job` (отмена/ошибки/окно).
5. `crates/gui/src/settings.rs` (дефект №2), `crates/gui/src/app.rs`
   (дефект №3, точки подключения G2).
6. `crates/app-core/src/bin/synthesize.rs` — эталон сборки PipelineOptions
   и обработки статусов для G2.
7. `docs/GUI_PLAN.md`, `docs/NEXT_STEPS.md`, `docs/SAFETY_AUDIT.md`.

## Критерии готовности следующего этапа

- Дефекты 1–4 исправлены, покрыты тестами (bytes_fed ≠ 0; tmp create_new —
  отказ при занятом имени; состояния секций переживают save/load;
  StreamError на аварийном пути подтверждён).
- G2: синтез из GUI на CPU и CUDA (короткий текст) — файл создаётся,
  прогресс/Стоп работают, отмена не публикует файл, `.partial`/diag-журнал
  показываются; `cargo test` зелёные, clippy 0; CLI-паритет параметров.
- HANDOFF.md обновлён после этапа.
