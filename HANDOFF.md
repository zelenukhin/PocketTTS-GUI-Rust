# HANDOFF.md — оперативный снимок проекта (читать первым вместе с AGENTS.md)

- **Дата обновления**: 2026-11-09 (G3: безопасный пакетный режим).
- **Последний функциональный коммит**: G3 — фактический хэш брать из Git
  (`git log -1 --oneline`); в документе не хардкодится.
- **HEAD**: только из Git (`git log -1 --oneline`), не из этого файла.
- **Бэкап на NAS**: полный zip `\\DISKSTATION\Ableton 2\Temp2\
  ShtormTTS-Windows-backup-9a3bbb6.zip`; изменённые файлы до G2.1 —
  `...\Temp2\research65\` (14 файлов, SHA256 14/14); текущий HEAD
  (G2.1+G3) — `...\Temp2\research66\` (16 файлов, плоско, SHA256 16/16).
  Примечание: «недоступность NAS» была вызвана экранированием `\\` при
  записи .ps1 через bash-heredoc (в файл попадал один `\`). Создавать
  .ps1-скрипты для UNC нужно инструментом записи файлов, а не heredoc;
  проверка — `Test-Path` в отдельном .ps1.

## Цель проекта

Нативный Windows x64 GUI на Rust (eframe/egui) для синтеза аудиокниг
моделями Shtorm-PocketTTS-RU v2 fast/slow (ONNX FP32, CPU/CUDA), потоковая
запись WAV/MP3/Opus без накопления аудио в памяти — функциональный порт
Python `pocket-tts_gui.py`. Постоянные правила и архитектура — `AGENTS.md`.

## Завершённые этапы (кратко; история — git log, детали — docs/)

- **A1** чанкование (golden 96), **A2** char_rules (golden 314), **A3** паузы
  (golden 20; конец абзаца > soft_break — задокументированное расхождение).
- **A4** пул воркеров (N × свой Pocket, backpressure-окно, seed=base+индекс,
  детерминизм CPU/CUDA 1w≡2w побитово).
- **A5/A5.1/A5.2** потоковый вывод: StreamingWav + FfmpegSink, уникальные
  `.partial`, publish_no_replace, RunStatus/StreamError/DiagMeta, Ctrl+C.
- **G1** каркас GUI (форма/валидация/настройки round-trip/импорт Python RO).
- **Фиксы A5.2/G1** (c6fdc1b, эта сессия):
  1) `synthesize_stream` возвращает ошибку `run_job` как ТИПИЗИРОВАННЫЙ
     `StreamError` (реальный прогресс done_ref/total, base_seed, путь
     partial) — `synthesize_stream_impl` с инъекцией фабрики; сквозной
     мок-тест (WavSink, сбой на 3-м чанке: chunks_done=2, черновик есть,
     итога нет) + реальный smoke CLI с битым путём модели.
  2) `FfmpegSink.bytes_fed` — ОДИН общий Arc между писателем и структурой
     (раньше поле читало всегда 0) + аксессор + тест 19200 байт после abort.
  3) `save_settings`: tmp создаётся эксклюзивно `create_new` (без TOCTOU);
     `write_exclusive` + тест отказа при занятом имени.
  4) Свёртки GUI управляются полем настроек: `.open(Some(field))` +
     `header_clicked` (клик переворачивает поле) — все 5 секций (поля
     `*_open` + новые model_open/pauses_open) реально сохраняются;
     headless-тест с симуляцией клика PointerButton + round-trip тест.
- **G2** (f114987) — реальный синтез из GUI, см. ниже.
- **G2.1** — безопасность ORT/ffmpeg, раздельные пресеты, предупреждения
  runtime.json, см. ниже.
- **G3** (последний функциональный коммит) — безопасный пакетный режим,
  см. ниже и `docs/G3_BATCH.md`.

## G2 — что сделано (f114987)

- `crates/gui/src/job.rs`: `execute_job` в фоновом потоке вызывает
  `app_core::pipeline::synthesize_stream` **напрямую** (CLI не запускается).
  - Модель: fast/slow от `onnx_base` (новое поле, дефолт
    `C:\Users\alexa\Downloads\pocket_tts_android\build\onnx`), custom —
    `model_dir`; общий резолвер для валидации и запуска.
  - ORT: `resolve_ort_dylib` (явный путь из настроек ort_cpu_dll/ort_gpu_dll
    → авто: CPU `deps/ort-cpu/…`, CUDA `deps/portable` (полный комплект
    providers+CUDA+cuDNN, 15 DLL) → `deps/ort-gpu-wheel`+torch-venv);
    PATH prepend и `ORT_DYLIB_PATH` — ТОЛЬКО в процессе GUI (система не
    трогается); `ort::init` один раз на процесс (`OnceLock`), смена
    CPU/CUDA внутри процесса — честная ошибка «перезапустите приложение».
  - Конвертация `GuiSettings → PipelineOptions` без потери параметров
    (тест на каждое поле); threads (новое поле, дефолт 4 = CLI), seed -1 →
    случайный, char_rules из `{model_dir}/runtime.json` (как CLI
    --char-rules; в текущих моделях файла нет → правила не применяются).
  - WAV/MP3/Opus через `make_sink` (ffmpeg из настроек → resolve_ffmpeg).
  - События `JobEvent {Progress, Log, Finished}` в mpsc-канал +
    `ctx.request_repaint`; итог `JobResult` — статус, сводка (RTF, паузы,
    денойз), пути итог/черновик/диагностический журнал/лог чанков.
- `app.rs`: «Запустить синтез» → поток (текст читается в потоке, UI не
  блокируется); «Стоп» → `Arc<AtomicBool>`; полоса прогресса X/N; запрет
  второго задания (кнопка + guard); закрытие окна при живом задании —
  `ViewportCommand::CancelClose` + отмена + автозакрытие по завершении
  потока; `JobHandle::drop` = cancel + join (сиротских ffmpeg нет).
- `write_chunk_log`/`log_path_for` перенесены в `app-core::pipeline`
  (CLI и GUI пишут одинаковый журнал чанков).
- GUI-логика в lib-крейте (`shtorm_gui`), main — только запуск eframe.

## G2.1 — что сделано

- `setup_ort_environment`: проверка `ORT_LOADED` **до** изменения
  `PATH`/`ORT_DYLIB_PATH`; повторное задание на том же runtime — no-op;
  CUDA-каталоги не добавляются в PATH повторно (`merge_path_prepend` с
  дедупликацией); смена DLL — явная ошибка без изменения env; при сбое
  `ort::init()` env откатывается. Тесты (`ort_init_needed`,
  `merge_path_prepend`) не загружают настоящий ORT.
- FFmpeg: валидация использует общий `resolve_ffmpeg` (явный путь →
  автопоиск рядом с exe/`../ffmpeg`/PATH); результат кэшируется в GUI
  (проверка процесса не каждый кадр, перепроверка ошибки раз в 2 с);
  ручной путь больше не обязателен при успешном автопоиске.
- Пресеты: `mp3_preset` и `opus_preset` разделены; `preset_for_format()`
  выбирает пресет по формату, UI показывает только пресеты своего формата;
  `encoder_args` строго проверяет соответствие и возвращает ошибку вместо
  тихого фолбэка (Opus 96k / MP3 -q:a 2 удалены). Импорт Python раскладывает
  opus-метку в `opus_preset`.
- `load_char_rules`: отсутствие `runtime.json` — штатное `Absent`; невалидный
  JSON, неразобранная или отсутствующая секция — `Warning`: в журнал GUI и
  в начало итоговой сводки (не молчаливое игнорирование).
- Документация: G1/G2 отмечены завершёнными, устаревшие «реализация не
  начата»/«до G2» удалены.

## G3 — что сделано (пакетный режим)

- `crates/gui/src/batch.rs`: снимок `plan_batch` (файл или каталог; DFS без
  symlink; `.txt/.md/.log`; стабильный порядок), безопасные выходные пути
  (`output_path_for`: без `..`/абсолютных/выхода за `out_root`), исключение
  выходной папки внутри источника (`out == source` — ошибка; `out`-предок
  не исключает файлы), `run_batch` с инъекцией синтеза для мок-тестов.
- `execute_batch` в `job.rs`: каждый файл через существующий `execute_job`
  (пул воркеров создаётся/освобождается на файл — без фиктивного
  «перезапуска каждые N»); ошибки изолированы, отмена не запускает
  следующий файл; существующие результаты — `Skipped` без перезаписи.
- `JobEvent::{FileProgress, FileDone}`, `BatchReport` в `JobResult`;
  UI: режим «Один файл / Каталог (пакетно)», две полосы прогресса
  (`файл K/N` и чанки текущего файла), отчёт по файлам в журнале.
- Тесты: 12 мок-тестов (вложенность, одинаковые имена, выход внутри
  источника/равенство/предок, `..`, существующий результат, изоляция,
  отмена, повтор, одиночный файл, symlink) + UI-мок + реальный CPU-пакет
  (5 файлов: готово 3 / ошибок 1 / пропущено 1, 9.5 с). Одиночный режим и
  WAV-паритет GUI==CLI сохранены (374444 байта).
- Переиспользование пула между файлами не реализовано: оценка изменений и
  рисков — `docs/G3_BATCH.md`.

## Подтверждённые результаты

- `cargo test` **116 passed / 0 failed** (5 ignored: 3 ONNX + real_matrix +
  real_batch_cpu); `cargo clippy --all-targets` **0**; release-сборка OK
  (shtorm-gui.exe запускается, окно живо).
- **Реальные проверки G2** (`cargo test -p shtorm-gui --release --lib
  real_matrix -- --ignored --nocapture`, второй прогон с
  `SHTORM_GUI_TEST_DEVICE=cuda`; CPU и CUDA — РАЗНЫЕ процессы, ORT в
  процессе грузится один раз):
  - CPU FP32 → WAV: 7.80 с аудио, RTF 0.882, Completed.
  - CPU FP32 → MP3 (-V2): Completed, RTF 0.908.
  - CUDA FP32 → WAV: Completed; **VRAM-дельта +1.4 ГиБ** (681→2136 МиB,
    WDDM per-process недоступен — тотал-поллинг nvidia-smi).
  - CUDA FP32 → Opus (64k): Completed.
  - Отмена после ≥1 готового чанка: Cancelled, итог НЕ опубликован,
    `.partial` и диагностический журнал существуют, прогресс в JobResult.
  - Повторный запуск после отмены (тот же путь): Completed, итог создан.
  - **GUI↔CLI побитовый WAV-паритет** при одинаковых настройках
    (threads=4, workers=2, seed=42, temp=0.3, EOS=-4, brightness=8,
    паузы 1.0/0.9, max_tokens=40): 374444 байта идентичны.
- **Реальный пакетный тест G3** (CPU FP32): 5 файлов (3 валидных во
  вложенных каталогах, 1 битый не-UTF8, 1 существующий результат) →
  `готово 3 | ошибок 1 | пропущено 1`, 9.5 с; относительные пути сохранены,
  существующий файл не перезаписан, прогресс K/N и чанков шёл событиями.
- Реальный аварийный путь (CLI, битый путь модели): типизированный
  StreamError «обработано 0/1, base_seed, черновик», диагностический
  журнал создан, итогового файла нет.

## Состояние подсистем

- **CPU FP32** — работает. **CUDA FP32** — работает (deps/portable, TF32
  выключен принудительно; VRAM-дельта — обязательная проверка GPU).
  Денойз DPDFNet: код готов; без файла модели — явный статус Missing.
- **WAV/MP3/Opus streaming** — готов (CLI и GUI).
- **GUI G2/G3** — одиночный файл и безопасный пакетный режим: полный цикл
  запуск/прогресс (K/N + чанки)/Стоп/статусы/журналы/отчёт/закрытие.

## Актуальные ограничения / нерешённое

- ORT грузится в процесс GUI один раз: смена CPU↔CUDA внутри одной сессии
  GUI требует перезапуска приложения (сообщение в UI).
- Auto-EOS (`runtime.json`-резолвер), `frames_after_eos`, CUDA I/O Binding,
  портативная сборка — **не начаты** (не имитировать наличие).
- Переиспользование пула воркеров между файлами не реализовано (сейчас пул
  создаётся/освобождается на файл); оценка изменений/рисков —
  `docs/G3_BATCH.md`. Фиктивного переключателя нет.
- Денойз не прогонялся с реальной моделью DPDFNet; decode_steps>1 не мерили.
- WAV > ~4 ГиБ — явная ошибка (RF64/разбиение — будущая функция).
- RTF GUI включает загрузку моделей (короткие тексты: RTF>1 — норма).
- Ручной клик-тест окна (кнопки/прогресс глазами) не выполнялся —
  автоматизированные проверки состояния/событий + запуск exe есть;
  полноценный интерактивный проход остаётся на усмотрение пользователя.

## Структура и ключевые файлы

```
Cargo.toml            # workspace: shtorm-engine, text-processing,
                      #   audio-processing, app-core, gui
crates/shtorm-engine/src/pocket.rs     # движок (FP32-only, Device, seed)
crates/text-processing/src/{chunking,char_rules,pauses}.rs
crates/audio-processing/src/stream.rs  # StreamingWav, publish_no_replace
crates/app-core/src/pipeline.rs        # synthesize_stream, StreamError,
                                       #   write_diag_log, write_chunk_log
crates/app-core/src/pool.rs            # run_job (воркеры, окно, отмена)
crates/app-core/src/sink.rs            # WavSink/FfmpegSink, bytes_fed
crates/gui/src/job.rs                  # G2: execute_job/execute_batch, ORT env
crates/gui/src/batch.rs                 # G3: план/обход/пути/отчёт (моки)
crates/gui/src/app.rs                  # форма + задание/прогресс K/N/Стоп
crates/gui/src/settings.rs             # GuiSettings (+batch/dll/threads/пресеты)
tools/*.ps1, tools/gen_*_golden.py     # smoke и генераторы golden
tests/golden/*.json                    # 96 + 314 + 20 кейсов
docs/SAFETY_AUDIT.md, docs/GUI_PLAN.md, docs/NEXT_STEPS.md,
docs/G3_BATCH.md, docs/FEATURE_PARITY.md, docs/BENCHMARKS.md
```

## Сборка / запуск / проверки (Windows, Git Bash)

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release                       # всё (вкл. shtorm-gui.exe)
cargo test && cargo clippy --all-targets    # норма: 0 failed / 0 warning
# реальные проверки GUI-задания (по одному устройству на процесс):
cargo test -p shtorm-gui --release --lib real_matrix -- --ignored --nocapture
SHTORM_GUI_TEST_DEVICE=cuda cargo test -p shtorm-gui --release --lib \
  real_matrix -- --ignored --nocapture
# реальный пакетный CPU-тест G3 (5 файлов, вкл. битый и существующий):
cargo test -p shtorm-gui --release --lib real_batch_cpu -- --ignored --nocapture
# GUI: ./target/release/shtorm-gui.exe
```
Настройки real_matrix-теста: ru-v2-slow, voice
`C:\PocketTTS-GUI\examples\Microsoft Brian Speed 1.wav`, ffmpeg
`C:\PocketTTS-GUI\ffmpeg\ffmpeg.exe`, seed 42, threads 4 (CPU 2w / CUDA 1w).

## NEXT TASK — G4 (НЕ НАЧИНАТЬ без явного задания)

1. Портативная сборка: exe + `deps/portable` + `ffmpeg.exe` + лаунчер,
   иконка; ассоциации не настраивать.
2. Решить: авто-EOS/`runtime.json`-резолвер (в моделях ru-v2 файла нет —
   согласовать, нужно ли «Авто» вообще); `frames_after_eos` — после
   согласования, не имитировать.
3. Переиспользование пула воркеров между файлами — только после замеров
   времени старта/памяти и отдельного разрешения (`docs/G3_BATCH.md`).
4. Денойз DPDFNet с реальной моделью; CUDA I/O Binding — по цифрам
   `docs/CUDA_FEASIBILITY.md`.
5. Полировка G5: интерактивный клик-проход, тема/копирование лога, RTF без
   времени загрузки моделей.

## Критерии готовности G4

- Портативная сборка запускается на чистой машине без Python и системного
  ORT/ffmpeg; модели/лицензии/размер описаны; safety-политика (create_new,
  без рекурсивных удалений) сохранена; `cargo test`/clippy зелёные.
