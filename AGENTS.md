# AGENTS.md — постоянные инструкции для агентов ZCode в этом проекте

Проект: `C:\PocketTTS-GUI-Rust\ShtormTTS-Windows` — нативный Windows x64
GUI для Pocket-TTS на Rust. Оперативное состояние — **HANDOFF.md** (читать
первым). Этот файл — только постоянные правила.

## Назначение проекта

- Нативный Windows x64 GUI для Pocket-TTS (порт Python `pocket-tts_gui.py`).
- Модели: **Shtorm-PocketTTS-RU v2 fast/slow** (ONNX, каталог
  `C:\Users\alexa\Downloads\pocket_tts_android\build\onnx\ru-v2-fast|slow`).
- ONNX Runtime (`ort` 2.0.0-rc.11, load-dynamic, `ORT_DYLIB_PATH`),
  CPU и NVIDIA CUDA (RTX 3060).
- **Только FP32.** Запрещены INT8, FP16, BF16, TF32 и автоматическое
  предпочтение `flow_lm_main_int8.onnx` (загрузчик всегда берёт
  `flow_lm_main.onnx`). CUDA: `use_tf32=0` всегда (дефолт провайдера —
  включён, доказано; изоляция — фича `diag-tf32`, в обычной сборке флага нет).
- Не менять семантику EOS, seed, чанкования, пауз и синтеза без явного
  задания. Исправленный приоритет «конец абзаца > soft_break» в паузах —
  согласованная семантика (расхождение с Python задокументировано в
  `docs/BENCHMARKS.md` / golden-тестах).
- Исходный Python PocketTTS-GUI (`C:\PocketTTS-GUI`) — **только чтение**
  (golden-снимки через `git show comma-semantics:...`).

## Архитектура (Cargo workspace)

- `crates/shtorm-engine` — ONNX-движок (порт teratts-android `pocket.rs`:
  BPE-токенизатор на чистом Rust, stateful-графы, EOS/анти-повторы,
  `Device::{Cpu, Cuda}`; `denoise.rs` — DPDFNet; `dsp.rs` — trim/fade).
  Файлы: `src/pocket.rs` (ядро), `src/denoise.rs`, `src/dsp.rs`.
- `crates/text-processing` — `chunking.rs` (порт `chunk_text`+
  `annotate_soft_breaks`+`terminate_bare_chunks`, golden 96 кейсов),
  `char_rules.rs` (порт `apply_char_rules`, golden 314), `pauses.rs`
  (`trailing_pause_s`, `chunk_pause_s` с исправленной приоритетностью;
  golden 20 кейсов).
- `crates/audio-processing` — `lib.rs` (write_wav16 create_new),
  `stream.rs` (`StreamingWav`: потоковый WAV, уникальный `.partial`,
  `publish_no_replace` = MoveFileW без замены, RIFF-лимит).
- `crates/app-core` — оркестрация: `pipeline.rs` (`synthesize_stream`,
  RunStatus/StreamError/DiagMeta/write_diag_log), `pool.rs` (N воркеров,
  скользящее окно backpressure, catch_unwind, строгий порядок, отмена),
  `worker.rs` (RealWorker: Pocket+DSP на воркер, base_seed),
  `sink.rs` (AudioSink: WavSink/FfmpegSink — writer-поток stdin,
  wait_with_timeout, kill только своего child), `trace_dir.rs` (уникальные
  каталоги трасс, только создание), `diagnostics.rs`, `settings.rs`
  (SynthSettings — имена полей Python GUI), бинари `bin/{smoke,bench,
  ep_probe,synthesize}.rs` (CLI — тестовый инструмент; GUI не должен их
  запускать, а звать app-core напрямую).
- `crates/gui` — eframe/egui 0.31: `settings.rs` (GuiSettings, round-trip
  unknown keys, атомарная запись, импорт Python read-only), `app.rs`
  (форма G1 + валидация), `main.rs` (шрифты/кириллица). G2 подключит
  `synthesize_stream` в фоновом потоке — **напрямую к app-core**.
- `docs/` — SAFETY_AUDIT, GUI_PLAN, FEATURE_PARITY, NEXT_STEPS,
  BENCHMARKS, ARCHITECTURE, CUDA_FEASIBILITY, PREPARATION_REPORT.
- `tests/golden/` — chunking (96), char_rules (314), pauses (20).
  Генераторы: `tools/gen_*_golden.py` (venv-питон `C:\PocketTTS-GUI\venv`).

## Критически важная безопасность

**В проекте был серьёзный инцидент**: ошибочный `remove_dir_all` по
пользовательскому пути удалил часть домашнего профиля Windows и git-репозиторий
(подробности и хронология — `docs/SAFETY_AUDIT.md`). Поэтому:

- **Категорически запрещено рекурсивное удаление** файлов/каталогов
  (`remove_dir_all`, `Remove-Item -Recurse`, `rd /s`, `git clean -fd`,
  `git reset --hard` и эквиваленты). В кодовой базе рекурсивных удалений нет —
  не возвращать их.
- Не проводить негативные файловые тесты на реальных пользовательских
  каталогах (дом, корень проекта, диск) — только уникальные песочницы %TEMP%.
- **Не перезаписывать существующие выходные файлы**: аудио — `create_new` +
  уникальный `.partial` (`{stem}.{нс}.{pid}.{счётчик}.partial.{ext}`) +
  публикация `MoveFileW` БЕЗ замены (`publish_no_replace`); ffmpeg `-n`.
- Единственная разрешённая замена файла — собственный конфиг GUI
  `shtorm_gui_settings.json` (atomic: unique tmp + `MoveFileExW
  REPLACE_EXISTING|WRITE_THROUGH`); не расширять это на другие файлы.
- Не изменять: Python-проект `C:\PocketTTS-GUI`, системные настройки/PATH/
  CUDA/профиль Windows/SSH/GPG/глобальные инструменты, модели Shtorm.
- Перед значительными изменениями: `git status` чист + верифицированный
  резервный zip на NAS (`\\DISKSTATION\Ableton 2\Temp2\ShtormTTS-Windows-
  backup-<hash>.zip`, CRC-проверка). Нет бэкапа — сначала создать.
- Тесты не удаляют файлы (артефакты остаются в %TEMP%); юнит-тесты «на
  реальных путях» запрещены; зависающие сценарии — только в дочернем
  процессе с внешним таймаутом.

## Правила разработки

- Сначала изучать существующую реализацию и тесты; не переписывать
  работоспособное без необходимости; не начинать следующий этап без задания.
- Изменения — небольшими проверяемыми шагами; после каждого: `cargo fmt`,
  `cargo clippy --all-targets` (0 предупреждений — норма проекта),
  `cargo test` (все зелёные).
- Изменения, затрагивающие синтез → короткие CPU/CUDA smoke
  (`tools/smoke-cpu.ps1` / `smoke-cuda.ps1`; VRAM-дельта — признак GPU).
- Golden-паритет с Python обязателен (генераторы идемпотентны, Python не
  менять). Детерминизм: seed+индекс чанка, CPU/CUDA 1w≡2w — побитово.
- Историю Git не переписывать; логические этапы — отдельными коммитами.
- Длительные бенчмарки/стрессы — только по явному заданию.
- Логи/трассы/большие исходники не вставлять в ответы целиком: писать в
  `out/` и сообщать краткие итоги и пути.

## Правила сопровождения документов

- После значимого этапа обновлять `HANDOFF.md` (оперативный снимок).
- `AGENTS.md` менять только при изменении постоянных правил/архитектуры.
- Основной источник контекста следующего чата — `HANDOFF.md`; не
  дублировать в документах многословные исторические отчёты (история — в
  git log и docs/SAFETY_AUDIT).
