# G6.1.1 — исправление пакетного прогресса и CLI-диспетчеризации

Дата: 2026-11-10. База: HEAD `5658c4f` (G6.1), верифицированный бэкап на NAS
создан ДО изменений:
`<NAS>\Temp2\ShtormTTS-Windows-full-5658c4f-20261110-162255.zip`,
SHA256 `24285EB50ED1EC8CCD2969B55C61A845280A21B42A3187482B0C3184C84292D5`,
156/156 blob совпадают с HEAD. Корректирующий этап без изменения математики
синтеза и производительности CUDA; G6.2 не начинался.

## 1. Дефект №1 — события preflight терялись в пакетном GUI

### Причина

В постоянном CUDA-пуле `execute_batch_persistent_pool()` вызывала
`preflight_batch()`, которая уже отправляла `JobEvent::FileDone` для
пропущенных файлов (существующий результат, конфликты имён, небезопасные
пути) и для файлов с ошибкой подготовки путей. Но `JobEvent::BatchStarted`
отправлялся только ПОСЛЕ создания и загрузки пула Pocket, а обработчик
`FileDone` в `app.rs` обновляет счётчики только при существующем
`batch_state` — который создаётся исключительно `BatchStarted`. Ранние
`FileDone` попадали в журнал, но НЕ в визуальные счётчики пакета.

Проявление: пакет из 12 файлов, 2 уже существуют (overwrite=false) — GUI
показывал бы «10/12» в конце вместо «12/12» (счётчики CLI-отчёта при этом
всегда были верны — дефект чисто визуальный).

### Исправление

- `job.rs`: `BatchStarted { total, workers: 0 }` отправляется в
  `execute_batch()` СРАЗУ после успешного `plan_batch` (пустой план — ранний
  возврат без события) — до preflight обоих путей и до любых `FileDone`.
- Новое событие `JobEvent::BatchWorkersReady { workers }`: пул сообщает
  число реально загруженных воркеров после `spawn_workers`; в `app.rs` оно
  ТОЛЬКО обновляет `batch_state.workers` — состояние не пересоздаётся,
  счётчики ранних `FileDone` не сбрасываются.
- Последовательный путь (`execute_batch_sequential`): собственный
  `BatchStarted` удалён (иначе двойной сброс после preflight — та же
  ошибка); вместо него шлёт `BatchWorkersReady { workers: 1 }` до preflight.
- Второй `BatchStarted` в пакете не отправляется нигде.
- `app.rs`, `BatchUiState`: защита от двойного учёта — `done_indices:
  HashSet<usize>`; повторный `FileDone` того же индекса не увеличивает
  счётчики и диагностируется в журнале («не учтено дважды»), а не молча
  игнорируется. `BatchWorkersReady` до `BatchStarted` — также диагностика
  («протокол»), состояние не создаётся.
- Строка статуса: при `workers == 0` честное «воркеры готовятся…» вместо
  вводящего в заблуждение «одновременно 0»; после загрузки пула — «одновременно
  N». Если пакет завершился до загрузки воркеров (всё пропущено / пул не
  запустился), UI не показывает работающих воркеров.

### Новый порядок событий batch

Пул (CUDA, ≥2 кандидатов):

```
execute_batch
  └─ BatchStarted { total, workers: 0 }        ← сразу после plan_batch
  └─ (пул) Log(модель), preflight_batch:
  │     FileDone(Skipped/Failed) × рано решённые файлы   ← УЧИТЫВАЮТСЯ
  │     FileDone(Failed) × ошибки подготовки путей
  └─ пусто → Finished (BatchWorkersReady не будет)
  └─ spawn_workers(N)
       ├─ сбой → Log(«пул не запущен») + FileDone(Failed)×кандидаты → Finished
       └─ успех → BatchWorkersReady { workers: N }
  └─ run_prepared_parallel: FileStarted/FileProgress/FilePhase/FileDone
  └─ Finished
```

Последовательный путь (CPU, 1 файл, откат `SHTORM_GUI_BATCH_POOL=0`):

```
BatchStarted { total, workers: 0 } → BatchWorkersReady { 1 } → preflight
FileDone(…) → FileStarted/FileProgress/FilePhase/FileDone × файлы → Finished
```

### Ранние завершения (все пути проверены тестами)

| Сценарий | События | Счётчики UI |
|---|---|---|
| Всё Skipped (overwrite=false) | BatchStarted → FileDone×N Skipped → Finished(NoWork) | finished=N, skipped=N, workers=0 («готовятся» → итог NoWork) |
| Все отклонены preflight | BatchStarted → FileDone×N (Skipped/Failed) → Finished | = отчёту |
| Ошибка подготовки каталога | FileDone(Failed) на файле, остальные идут | = отчёту |
| Сбой загрузки пула | Log + FileDone(Failed)×кандидаты, БЕЗ BatchWorkersReady | failed=кандидаты, workers=0 |
| Отмена до/во время preflight | FileDone(Skipped «отменено до запуска»)×N | = отчёту, Cancelled |
| Пустой пакет | ранний возврат до BatchStarted | batch_state нет, честный NoWork |
| Отказ до ORT/модели | BatchStarted + Log → Finished(Failed) | 0/N, статус ошибки |

## 2. Дефект №2 — неизвестный CLI-флаг открывал GUI

### Причина

`main.rs` распознавал аргументы через `args.iter().any(|a| a == "--portable-…")`;
при несовпадении ВСЕХ известных имён выполнение проваливалось в
`eframe::run_native` — опечатка `--portable-batch-selftest` (реальный инцидент
визуальной приёмки G6.1) молча открывала окно. `arg_value` не проверял
наличие значения; `apply_overrides` молча трактовал любое значение `--device`
как cpu, `--format` — как wav.

### Исправление

Новый модуль `crates/gui/src/cli.rs` — строгий парсер (минимальная
проверяемая реализация, без новых зависимостей):

- `parse_cli_args(&[String]) -> Result<CliCommand, String>`;
  `CliCommand::{Gui, Help, PortableCheck, PortableSelftest,
  PortableSelftestBatch, PortableSelftestCancel}`.
- Без аргументов — `Gui` (как раньше).
- `--help` / `-h` / `/?` — справка (`usage_text()`), код 0, окно не открывается.
- Ровно одна диагностическая команда; вторая (включая повтор той же) — ошибка
  «несовместимы».
- Опции `--device cpu|cuda`, `--format wav|mp3|opus`, `--hold-ms <u64>` —
  только вместе с диагностической командой (до или после — оба порядка
  поддержаны, существующие скрипты не ломаются); значение обязательно,
  значение-опция (`--device --format`) — ошибка, опечатка значения — ошибка
  с перечислением допустимых.
- Любой другой аргумент — «неизвестный аргумент … используйте --help».
- `main.rs`: при `Err` — сообщение + справка в stderr, `exit(2)`, выход ДО
  `eframe::run_native` и до любой загрузки Pocket/чтения настроек; файлы не
  создаются, пользовательские настройки не меняются.
- Пути с пробелами/Unicode в аргументах не участвуют (значения опций —
  закрытые множества); `std::env::args` отдаёт Unicode без изменений.

## 3. Изменённые файлы

| Файл | Изменение |
|---|---|
| `crates/gui/src/job.rs` | `BatchWorkersReady`; `BatchStarted{workers:0}` в `execute_batch` до preflight; последовательный путь — `BatchWorkersReady{1}` вместо своего `BatchStarted`; пул — `BatchWorkersReady{workers}` после загрузки; тесты событий (3) + real-тест partial-skip |
| `crates/gui/src/app.rs` | Обработчик `BatchWorkersReady` (только `workers`); дедупликация `FileDone` по `done_indices` с диагностикой; статус «воркеры готовятся…» при workers=0; UI-тесты Test A–F |
| `crates/gui/src/cli.rs` | НОВЫЙ: парсер + справка + 4 юнит-теста |
| `crates/gui/src/main.rs` | Диспетчеризация через `cli::parse_cli_args`; ошибки — exit(2) без окна; `--help` |
| `crates/gui/src/lib.rs` | `pub mod cli` |

Архитектура пулов (`persistent.rs`, `pipeline.rs`, `batch.rs`), математика
Pocket, seed/EOS/anti-repeat/DSP/chunking, CUDA I/O Binding, публикация
G5/G5.1, отмена, `.partial`, `SHTORM_GUI_BATCH_POOL=0` — НЕ менялись.

## 4. Таблица тестов

Юнит/модельные (без CUDA), `cargo test --release` — **207 passed / 0 failed /
23 ignored** (было 194/23; +13):

| Тест | Проверка |
|---|---|
| `app::batch_early_skipped_survive_workers_ready` (Test A) | точная последовательность §4: total=5, finished=5, completed=2, skipped=2, failed=1, active=0; BatchWorkersReady не сбрасывает счётчики |
| `app::batch_all_skipped_never_shows_running_workers` (Test B) | все Skipped учтены; workers=0; итог NoWork; нет ложного «воркеры работают» |
| `app::batch_pool_load_failure_counters_match_report` (Test C) | 2 Skipped preflight + 2 Failed «пул не запущен»; счётчики = отчёту |
| `app::batch_cancel_during_preflight_keeps_counters` (Test D) | отмена в preflight: без FileStarted, счётчики не сброшены, итог Cancelled |
| `app::batch_workers_ready_is_not_reinit_and_duplicate_filedone_diagnosed` (Test E) | повторный BatchWorkersReady не переинициализирует; дубль FileDone диагностируется без двойного счёта |
| `app::batch_sequential_single_started_then_single_file_mode` (Test F, UI) | последовательный путь: один BatchStarted + WorkersReady(1); одиночный синтез без пакетного состояния |
| `job::sequential_batch_emits_single_started_before_any_filedone` (Test F, события) | реальный порядок событий execute_batch: BatchStarted до всех FileDone |
| `job::all_skipped_batch_events_have_no_workers_ready` | всё пропущено — ни FileStarted, ни BatchWorkersReady |
| `job::cancelled_batch_events_order` | BatchStarted до FileDone отмены |
| `cli::dispatch_table` / `options_valid_in_any_order` / `options_invalid_are_errors` / `error_messages_are_diagnostic` | таблица §6: все команды, опечатка инцидента, отсутствие значения, конфликты, значения-множества, справка |

Smoke реального EXE в изолированной песочнице (`tools/g611_cli_smoke.ps1`,
копия exe + собственный `shtorm_gui_settings.json`): **ALL PASS** — 9
ошибочных вариантов (exit 2, сообщение в stderr, самовыход <0.5 с — окно не
открывалось, т.к. живое окно eframe блокирует выход), `--help`/`-h` (exit 0),
`--portable-check` и «опции до команды» (PORTABLE CHECK OK), GUI без
аргументов (процесс жил ≥4 с — окно открыто; закрыт мягко, exit 0; управление
только собственным PID).

Headless GPU через EXE (`tools/g611_headless_gpu.ps1`, изолированная песочница,
CUDA WAV, seed 42): **ALL PASS** — `--portable-selftest` (Завершено, io
binding active), `--portable-selftest-batch` (пул 2 воркера, «Pocket загружен
2 раз(а)», 2/2), `--portable-selftest-cancel` (CANCEL SELFTEST OK); сиротских
ffmpeg 0.

## 5. Real-проверки CUDA/CPU (RTX 3060, набор main12 P2B.3, seed 42, Леванов)

| Тест | Результат |
|---|---|
| `real_batch_cuda_pool_partial_skip_events` — НОВЫЙ (§7: часть WAV существует, overwrite=false) | **ok**: 6 Skipped пришли ДО `BatchWorkersReady`, 6 синтезированы пулом 2; Pocket ровно 2 загрузки; существующие побайтово целы; счётчики событий (редукция apply_event) == BatchReport (12/6/6/0); 22.0 с |
| `real_batch_cuda_pool` (паритет) | **ok**: old 68.9 с → пул 44.9 с; 12/12 WAV побитово; Pocket 2 загрузки; VRAM 584→722 МБ (без утечки) |
| `real_batch_cuda_pool_cancel` | **ok**: Cancelled (1 готов + 10 пропущено), повтор — Completed 11 доделано / 1 пропущен |
| `real_batch_cuda_pool_overwrite` | **ok**: skip без изменений; перезапись «файл обновлён», без .partial |
| `real_matrix_device` (cuda) | **ok**: одиночный WAV RTF 0.370 + Opus 0.293 |
| `real_overwrite_wav` (cuda) | **ok**: обновлён/создан/отмена с побайтово целым старым/повтор |
| `real_batch_cpu` | **ok**: 3 готово + 1 ошибка + 1 пропущено (ЧАСТИЧНЫЙ УСПЕХ), события файлов идут |

Контроль после всех прогонов: сиротских ffmpeg 0, VRAM 554 МБ (база).

## 6. Результат визуальной проверки G6.1

Полной ручной визуальной приёмки живого окна на идущем пакете в этом этапе НЕ
проводилось (пользователь окно не открывал). Доступными средствами
подтверждено:

- обычный GUI открывается и штатно закрывается (smoke gui-no-args: процесс
  жив ≥4 с, мягкое закрытие, exit 0);
- пакетный CUDA-прогон через headless EXE: пул 2 воркера, 2 загрузки Pocket,
  2/2 файла;
- РЕАЛЬНАЯ последовательность событий на смешанном пакете (6 существует /
  6 синтез) соответствует модели UI: ранние Skipped учитываются до загрузки
  пула, итоговые счётчики совпадают с отчётом (Test A + real-тест) — то, что
  показала бы полоса «файлы K/N (готово X | ошибок Y | пропущено Z)»;
- отмена и повторный запуск на реальном пакете — зелёные (cancel-тест);
- счётчики, «Стоп» между файлами, per-file прогресс параллельных файлов —
  модельно (unit-тесты apply_event), как и в G6.1 §11.

Непроверенное глазами (ограничение, как в G6.1): одновременное отображение
двух активных файлов с независимым прогрессом и нажатие «Стоп» в живом окне
во время синтеза. Diagnostics не деградировали: все `--portable-*` режимы
работают, включая опции до/после команды.

## 7. Оставшиеся ограничения

- Визуальная проверка живого окна на идущем пакете — по-прежнему не «глазами»
  (см. §6); модельные и headless-проверки закрывают состояние/счётчики.
- Дедупликация `FileDone` — защита UI-счётчиков; протокольная аномалия
  диагностируется в журнале, но не прерывает задание.
- `--hold-ms`, `--device`, `--format` по-прежнему только диагностические
  (GUI-режиму опции не передаются — раньше они молча игнорировались, теперь
  это явная ошибка).
- CLI остаётся минимальным (без внешних парсер-библиотек — по заданию);
  `/?` распознаётся наряду с `--help`.
- G6.2 (гибрид 2×2) не начинался; производительность CUDA не менялась
  (замер паритета: 44.9 с ≈ 43.5 с G6.1 в пределах разброса стенда).

## 8. Команды воспроизведения

```bash
# юнит-тесты (без CUDA)
cargo test -p shtorm-gui --lib
# real-сценарий «часть существует» (CUDA, ~67 с)
SHTORM_GUI_TEST_DEVICE=cuda cargo test -p shtorm-gui --release --lib \
  real_batch_cuda_pool_partial_skip_events -- --ignored --nocapture --test-threads=1
# smoke CLI/headless EXE
powershell -NoProfile -ExecutionPolicy Bypass -File tools/g611_cli_smoke.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File tools/g611_headless_gpu.ps1
```
