# AGENTS.md — постоянные инструкции для агентов ZCode в этом проекте

Проект: приложение **PocketTTS-GUI-Rust** (полное переименование из
ShtormTTS — G6.5) — нативный Windows x64
GUI для Pocket-TTS на Rust; репозиторий —
`C:\PocketTTS-GUI-Rust\ShtormTTS-Windows` (каталог пока носит прежнее имя —
см. план миграции в HANDOFF.md, G6.5). Оперативное состояние — **HANDOFF.md**
(читать первым). Этот файл — только постоянные правила.

## Назначение проекта

- Нативный Windows x64 GUI для Pocket-TTS (порт Python `pocket-tts_gui.py`).
- Модели: **Shtorm-PocketTTS-RU v2 fast/slow** (ONNX, каталог
  `C:\Users\<user>\Downloads\pocket_tts_android\build\onnx\ru-v2-fast|slow`).
- ONNX Runtime (`ort` 2.0.0-rc.11, load-dynamic, `ORT_DYLIB_PATH`),
  CPU и NVIDIA CUDA (RTX 3060).
- **Точность графов (G6.3).** FP32 — исходный режим и значение по умолчанию
  (все пять графов FP32). INT8 — только ЯВНЫЙ выбор (варианты списка
  «Модель» Fast/Slow INT8): динамически-квантованные
  `flow_lm_main_int8/flow_lm_flow_int8/mimi_decoder_int8` + всегда-FP32
  `mimi_encoder/text_conditioner` (INT8-версий нет); молчаливое
  переключение точностей запрещено (отсутствие файла — явная ошибка).
  INT8 быстрее на CPU (−25…−32%), но на CUDA — экспериментальный режим
  (квант-узлы идут на CPU EP, ~2.7× медленнее FP32; предупреждение в GUI).
  FP16/BF16/TF32 по-прежнему запрещены; CUDA: `use_tf32=0` всегда
  (дефолт провайдера — включён, доказано; изоляция — фича `diag-tf32`,
  в обычной сборке флага нет). Подробности — `docs/G6_3_INT8.md`.
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
  `stream.rs` (`StreamingWav`: потоковый WAV, уникальный `.partial`;
  G5.1 `finish()` закрывает контейнер БЕЗ публикации, отдельная
  необратимая `FinalizedWav::publish()` — `MoveFileW` без замены по
  умолчанию, G5 `overwrite=true` → `MoveFileExW REPLACE|WRITE_THROUGH`;
  RIFF-лимит).
- `crates/app-core` — оркестрация: `pipeline.rs` (`synthesize_stream`
  с фазами G5.1 `RunPhase::{Generating,Finalizing,Publishing}` и ЕДИНОЙ
  точкой решения о публикации — отмена до неё не публикует, после неё
  конвейер доходит до конца; RunStatus/StreamError (с `publish_failed`)/
  DiagMeta/write_diag_log, `write_chunk_log`/`log_path_for` — общий журнал
  чанков CLI и GUI; G6.1 `synthesize_stream_with_worker_factory` — файл
  целиком на внешнем Pocket), `pool.rs` (N воркеров, скользящее окно
  backpressure, catch_unwind, строгий порядок, отмена), `persistent.rs`
  (G6.1: постоянный пул для пакетов — слоты, одалживание `Loaned` с
  per-file seed, параллельный `spawn_workers`, динамическая очередь
  `run_dynamic_queue`, отравление слота при сбое движка; G6.7:
  `synthesize_stream_pooled` + `pooled_real_worker_factory`/`PoisonLoan`/
  `live_slots` — чанки файла на постоянном пуле, `build_file_tasks` —
  единый план чанков), `worker.rs`
  (RealWorker: Pocket+DSP на воркер, base_seed, G6.1 `set_seed`),
  `sink.rs` (AudioSink: `finish()` +
  `FinalizedSink::publish()`; WavSink/FfmpegSink — writer-поток stdin,
  wait_with_timeout, kill только своего child), `trace_dir.rs` (уникальные
  каталоги трасс, только создание), `diagnostics.rs` (память/VRAM; G6.1
  `gpu_free_mb` — префлайт), `settings.rs`
  (SynthSettings — имена полей Python GUI), бинари `bin/{smoke,bench,
  ep_probe,synthesize,diag_p2b2,diag_p2b3}.rs` (CLI/диагностика —
  тестовый инструмент; GUI не должен их
  запускать, а звать app-core напрямую).
- `crates/gui` — eframe/egui 0.31: `window.rs` (G6.4: стартовый размер по
  РАБОЧЕЙ области монитора запуска и DPI; 1080×900 с ограничениями
  560…1440×420…1050, миним не больше экрана; однократное
  `SetWindowPos` в физических пикселях до первого кадра; никаких
  resize-циклов), `settings.rs` (GuiSettings, round-trip
  unknown keys, атомарная запись tmp create_new + MoveFileExW; G5 — без
  импорта Python-настроек, EOS по умолчанию `Fixed(-4.0)`, флаг
  `overwrite_existing`), `app.rs` (форма, валидация, прогресс/Стоп/статусы,
  G6.4 — верхняя панель + центральная прокручиваемая форма в жёстком
  слоте content_w (≤1160) + закреплённые кнопки/статус/прогресс и журнал
  с собственной прокруткой и регулируемой высотой; 2 колонки при ширине
  ≥two_columns_threshold() (~616 пт, из минимальных размеров содержимого),
  иначе 1; блок «Модель и устройство» — всегда вертикальный; строки путей
  LTR с фиксированной кнопкой «Обзор…»;
  мягкие переносы длинных путей; однократная подстройка под смену
  монитора, без ресайза каждый кадр),
  G5.1 фазы: на Finalizing/Publishing «Стоп» отключён и статус «Сохранение
  результата…», управляемые свёртки `.open(Some(field))`, G6.2.3 — машина
  закрытия `CloseFlow` (Idle/WaitingForJob/Saving/SaveFailed/Closing) с
  автосохранением настроек при штатном закрытии: при живом задании сначала
  отмена и завершение потока, затем сохранение; ошибка записи — окно
  удерживается с диалогом Повторить/Выйти без сохранения; непрочитанный
  при старте конфиг НЕ перезаписывается; `on_exit` — страховка, на Drop не
  полагаемся), `job.rs` (G2: резолв модели fast/slow, ORT env + init
  один раз на процесс, конвертация GuiSettings→PipelineOptions с паритетом
  CLI, execute_job в фоновом потоке — напрямую app-core, НЕ subprocess;
  G6.7: execute_batch — файлы каталога СТРОГО последовательно, чанки
  текущего файла параллельно на постоянном пуле (P=1 → InProcessChunkPool
  min(W,C) Pocket; P>1 → chunkpipe); topology/префлайты в decide_topology;
  диагностика G6.6 — SHTORM_GUI_G66_FILE_POOL=1 / SHTORM_GUI_BATCH_POOL=0;
  G5.1: `ensure_output_path_safe` ДО создания каталогов,
  `create_dir_all_no_reparse`, повторная валидация, семантика
  «создан/обновлён/пропущен/отменён/ошибка публикации»),
  `resources.rs` (G4: единый exe-relative резолвер моделей/ORT CPU/CUDA/
  ffmpeg; портативный комплект не требует Cargo.toml/CWD), `cli.rs`
  (G6.1.1: строгая диспетчеризация аргументов — неизвестный/повторный
  флаг или плохое значение опции завершает процесс кодом 2 с диагностикой
  ДО открытия окна и загрузки Pocket; `--help` без окна; G6.2: внутренняя
  команда `--internal-batch-worker <канал>` и `--portable-selftest-hybrid`),
  `ipc.rs` (G6.2: локальный IPC диспетчера с дочерними процессами —
  Named Pipes, duplex, OVERLAPPED I/O, кадры u32+JSON, проверка PID
  клиента, PIPE_REJECT_REMOTE_CLIENTS; синхронные handle для duplex-
  протокола непригодны — взаимная блокировка чтения/записи),
  `hybrid.rs` (G6.2: гибридная IPC-инфраструктура — Named Pipes duplex,
  протокол Cmd/Ev, внутренний worker-режим `--internal-batch-worker`
  (Pocket на процесс), Job Object KILL_ON_JOB_CLOSE, watchdog,
  контролируемый shutdown; файловый мультипроцесс G6.6 сохранён как
  диагностический), `chunkpipe.rs` (G6.7: мультипроцессный ЧАНК-конвейер —
  диспетчер владеет файлами строго последовательно, чанки текущего файла
  распределяются между Pocket дочерних процессов, PCM возвращается
  бинарными кадрами f32 LE за JSON-заголовком ChunkDone, упорядоченная
  сборка/публикация у диспетчера, окно C+2; см.
  `docs/G6_7_UNIFIED_CHUNK_POOL.md`), `main.rs`
  (шрифты/кириллица + диагностические `--portable-*` режимы через cli.rs;
  G6.2.3: release — `windows_subsystem="windows"` (консольного окна нет;
  debug остаётся консольным), диагностика в том же EXE через явные
  redirects, MessageBox только при отсутствии stderr; внутренний
  worker-режим НИКОГДА не открывает окно).
- `tools/build-portable.ps1` + `tools/portable/` — G4-сборка комплекта:
  новый каталог вне репозитория, без удалений, проверка размеров/SHA256,
  manifest; лицензии/notices, VC++ runtime, FFmpeg, FP32-модели.
- `docs/` — SAFETY_AUDIT, GUI_PLAN, FEATURE_PARITY, NEXT_STEPS,
  BENCHMARKS, ARCHITECTURE, CUDA_FEASIBILITY, PREPARATION_REPORT,
  G3_BATCH, PORTABLE_BUILD.
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
- **Существующие выходные файлы по умолчанию не перезаписываются**: аудио —
  `create_new` + уникальный `.partial` (`{stem}.{нс}.{pid}.{счётчик}
  .partial.{ext}`) + публикация `MoveFileW` БЕЗ замены
  (`publish_no_replace`); ffmpeg `-n`.
- **G5 — единственное исключение для аудио**: при явном
  `overwrite_existing=true` (GUI-чекбокс) разрешена замена готового файла
  назначения. Старый файл НЕ удаляется заранее; замена — атомарная
  `MoveFileExW REPLACE_EXISTING|WRITE_THROUGH` только после успешной
  финализации sink (G5.1: решение принимается в ЕДИНОЙ точке после
  генерации; отмена после неё уже не отзывает замену — UI отключает
  «Стоп» и показывает «Сохранение результата…»). При ошибке/отмене старый
  файл остаётся, `.partial` сохраняется для диагностики. Каталоги,
  reparse-точки и пути вне каталога результата отвергаются ДО создания
  каталогов (`create_dir_all_no_reparse` — без следования по ссылкам).
  Входные тексты, WAV-голос, модели и настройки замене не подлежат.
- Другая разрешённая замена — собственный конфиг GUI
  `PocketTTS-GUI-Rust.json` (atomic: unique tmp + `MoveFileExW
  REPLACE_EXISTING|WRITE_THROUGH`; G6.2.3: автосохранение при штатном
  закрытии окна; непрочитанный при старте конфиг автоматически НЕ
  перезаписывается; G6.5: при отсутствии нового файла настройки один раз
  переносятся из прежнего `shtorm_gui_settings.json` — он только читается,
  не удаляется и не изменяется; повреждённый новый конфиг не подменяется
  старым).
- **G6.2.3 — окна консоли**: release-GUI — Windows GUI-подсистема;
  консольные дочерние процессы (`ffmpeg`, `nvidia-smi`) спавнятся через
  `app_core::hide_console()` (CREATE_NO_WINDOW), дочерние worker-процессы —
  тот же GUI-exe. Не возвращать `ShowWindow(SW_HIDE)`/`FreeConsole()` как
  решение: можно скрыть консоль пользователя или потерять диагностику;
  вывод диагностики должен оставаться доступным через перенаправленные
  stdout/stderr и ExitCode.
- **G6.2 — дочерние процессы синтеза**: GUI-процесс запускает ТОЛЬКО
  собственный exe (`--internal-batch-worker`) и завершает/kill'ит только
  свои дочерние PID (плюс Windows Job Object KILL_ON_JOB_CLOSE — гибель GUI
  не оставляет сирот). Дочерний процесс ПОВТОРНО проверяет безопасность
  пути результата перед записью. Таймаут постановки в очередь писателя
  ffmpeg — `STDIN_STALL_TIMEOUT` 5 с (4 одновременных ffmpeg под
  CUDA-нагрузкой; зависший энкодер по-прежнему диагностируется).
- Не изменять: Python-проект `C:\PocketTTS-GUI`, системные настройки/PATH/
  CUDA/профиль Windows/SSH/GPG/глобальные инструменты, модели Shtorm.
- Перед значительными изменениями: `git status` чист + верифицированный
  резервный zip на NAS (`<NAS>\Temp2\ShtormTTS-Windows-
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
