# HANDOFF.md — оперативный снимок проекта (читать первым вместе с AGENTS.md)

- **Дата обновления**: 2026-11-09 (G5: удалён импорт Python-настроек, EOS
  без «Авто» (`Fixed(-4.0)`), добавлен явный флаг перезаписи с атомарной
  заменой готового аудиофайла; полировка журнала).
- **Последний функциональный коммит**: фактический хэш — из Git
  (`git log -1 --oneline`). Функциональный код меняли G4.1, CUDA-P2A
  (`ae47215`), CUDA-P2A.1 и G5 (этот этап); более ранние записи только о
  G4.1 были устаревшими.
- **HEAD**: только из Git (`git log -1 --oneline`), не из этого файла.
- **Бэкап на NAS (подтверждён; сделан ПЕРЕД изменениями CUDA-P2A.1)**: полный
  архив HEAD `9133450` —
  `\\DISKSTATION\Ableton 2\Temp2\ShtormTTS-Windows-full-9133450-20261109-232858.zip`,
  SHA256 `8709B433800205DFF49E06E4C19A0533C0F2A1BEC667A8E837C2A40DB5A1F09A`,
  144/144 blob совпадают. Предыдущий: `…full-374097b-20261109-223351.zip`
  (CUDA-P2A).

## CUDA-P2A.1 — стабилизация и включение I/O Binding по умолчанию

- **Статус**: I/O Binding — обычный путь для `Device::Cuda`. Обратимый
  диагностический переключатель: `SHTORM_CUDA_IO_BINDING=0` (или `off`/
  `false`) принудительно включает старый CUDA-путь; `1|on|true` — явно
  включает (на CPU — ошибка); не задан — CUDA: binding, CPU: прежний путь.
  Неизвестное значение — явная ошибка. Скрытого fallback на CPU нет.
- **Аудит lifetime (ort 2.0.0-rc.11 + ORT 1.23.2)**: `IoBinding` держит
  `Arc<SharedSessionInner>` (сессия жива до drop binding'а) и
  `Arc<ValueInner>` всех входов; `GetBoundOutputValues` возвращает
  refcounted `OrtValue`, освобождаемые в Drop; `SessionOutputs::remove` —
  Arc-клон. Порядок полей `Pocket` (`main` раньше `main_io`) безопасен,
  не менялся; `unsafe` не добавлялся. После каждого Run —
  `synchronize_outputs()`. Ошибка любого шага `IoStates::run` очищает
  состояния (следующий прогон — с нулей). Детали —
  `docs/CUDA_PROFILE_P2A.md` §13.
- **Новые CUDA-тесты** (`#[ignore]`, `SHTORM_TEST_CUDA=1`): 3 чанка одним
  Pocket old==new побитово (3.58 с → 1.02 с); принудительный anti-repeat
  retry (EOS-порог 100) old==new побитово, 232 320 vs 163 200 сэмплов без
  повтора; 4 цикла load→say→drop — пик 2552–2571 МБ, после drop 646–652 МБ
  без роста; 2 одновременных Pocket == 1 последовательный побитово.
- **13 чанков (ru-v2-slow)**: binding 1w wall 9.32 с / RTF 0.129 / main
  5.65 мс/кадр / VRAM 2688 МБ; 2w 8.91 с / 0.123 / 10.04 / 4738 МБ;
  legacy 1w 33.31 с / 0.472 / VRAM 2689 МБ. Чистые прогоны: 1w 8.79–8.89 с
  (0.126–0.127), 2w 8.53–8.56 с (0.122), legacy 32.35 с (0.462) — уровень
  P2A, регрессии нет. Все WAV матрицы побитово `5ca845c0…` (= P1/P2A).
- **Прочее**: fast `7a794f8f…` и slow `d516b3f7…` smoke old==new; GUI CUDA
  матрица (WAV/Opus, отмена во время генерации следующего чанка, повтор) —
  OK; CPU матрица и GUI↔CLI паритет 374444 байта — без изменений; VRAM
  после всех прогонов 539 МБ; `graph_meta` пишет только `create_new`
  (повторный запуск с тем же путём — ошибка, файл не изменён).
- **Портативный комплект P2A.1**:
  `C:\PocketTTS-GUI-Rust\dist\ShtormTTS-Portable-2026-11-09-p2a1-final`
  — 54 файла, 3403.2 MiB, manifest git `107acf3`, dirty=false, exe SHA256
  `156BD8D90AC8AAA92F05770446CE90A987FBEAFDF6AA53534CC69FA631279BB3`.
  Проверено из `C:\`: portable-check; CPU WAV RTF 0.898 `80CB7288…`;
  CUDA WAV RTF 0.414 `85A26977…` с логом `io binding active`; MP3/Opus;
  batch CUDA 2/2; `--portable-selftest-cancel` (Cancelled 1/6 без итога,
  `.partial` есть; повтор Completed 6 чанков); GUI-окно + сохранение
  настроек через UI Automation; паритет комплект-GUI == CLI; DLL только
  из комплекта. Тестовый `…-p2a1` (git `e99c056`) и `…-g41-final` не
  изменялись. Детали — `docs/PORTABLE_BUILD.md`.
- **Решение**: CUDA=1 — рекомендованный автоматический режим GUI
  (2 воркера дают ~4% wall при +2 ГБ VRAM). P2B не начинался.

### CUDA-P2A — исходный эксперимент (2026-11-09)

- **Новый путь**: `IoBinding` (ort 2.0.0-rc.11) для состояний
  `flow_lm_main`: выход кадра k привязывается входом k+1 без хост-копии.
  Два binding'а (ping-pong), состояния сопоставляются ПО ИМЕНАМ
  `state_*` ↔ `out_state_*`, checked типы/формы; `reset()` = одна H2D-копия
  нулей на чанк/попытку; на CPU читаются только `conditioning` (4 КиБ) и
  `eos_logit`. `flow`/`decoder`/CPU-путь не менялись. Код:
  `crates/shtorm-engine/src/io_states.rs`, в `pocket.rs` — `main_io` +
  `main_reset`/`main_run` (без binding — прежний `States::run`).
- **Результаты** (13 чанков/64.01 с речи, seed 42, temp 0.3, steps 1,
  EOS −4/5/3, FP32/TF32=0, прогрев + 3 повтора; чистый синтез без загрузки):
  old 1w 33.3 с (RTF 0.476), new 1w **8.15 с (RTF 0.116)** — 4.1×;
  new 2w 7.35 с (0.105), 3w 7.30 с (0.104). main.run 36.3 → **5.96 мс/кадр**
  при 1w; средний чанк 2.56 → 0.62 с. Новый 1w быстрее Python/PyTorch
  (8.7–9.35 с, RTF 0.135–0.146) на том же стенде.
- **WAV побитово**: 31 прогон (old/new × 1/2/3w × warmup/3 повтора/clean) +
  pilot — один SHA256 `5ca845c093ab73b5…`; равен эталону CUDA-P1. Smoke
  одной фразы old==new `ac2b77a9…`.
- **VRAM не вырос**: пики 2714/4766/6842 МБ (old 2715/4767/6825).
  Ping-pong +93.75 МиБ/воркер компенсируется исчезновением staging.
- **Устранение копий доказано**: контролируемый A/B на синтетическом графе
  с 18 стейтами как у main — 29.2–31.5 мс/прогон (старая схема) против
  0.63–0.65 мс (binding), 46–48×; трафик за задание ~78.8 ГиБ → ~0.61 ГиБ (~130×);
  `state[0].memory_info()` = CUDA; ORT-trace old/new идентичны (55 993
  события). Nsight/CUPTI на этой машине недоступны (nsys: «Connection to
  Agent lost», нужен elevation) — 14.6 мс из CUDA-P1 не считать GPU-временем.
- **Оставшееся узкое место**: shape-узлы CPU (222/кадр) и синхронизации;
  воркеры ≥2 почти не дают выигрыша (new wall 9.8/9.4/9.8 с; GPU avg 40–41%,
  пик 48–83%). Это цель P2B, не копии.
- **Проверки**: `cargo test` 153 passed / 0 failed; clippy 0; fmt; release OK;
  синтетические тесты (CPU+CUDA): ping-pong/reset/zero-size/пустая sequence/
  несовпадения имён-типов-динамики/50 циклов create-drop/замер копий.
  Модели: `crates/shtorm-engine/tests/models` (генератор
  `tools/gen_p2a_models.py`). Подробно, включая API и методику, —
  `docs/CUDA_PROFILE_P2A.md`, сырые данные `out/p2a/`.
- **Решение (историческое; заменено P2A.1)**: на момент P2A путь был
  оставлен экспериментальным за флагом. В P2A.1 он стабилизирован и включён
  по умолчанию для CUDA (см. раздел выше). P2B (flow-binding,
  shape-узлы/синхронизации, масштабирование воркеров) — только по
  отдельному заданию.

## CUDA-P1 — что сделано (диагностика, без оптимизаций)

- **Ключевой факт**: Python PocketTTS-GUI синтезирует через **PyTorch**
  (`pocket_tts` 3.1.0, веса на GPU, KV-кэш преаллоцирован, mimi-декодер —
  параллельный поток, синхронизация на кадр только `is_eos.item()`), а не
  через ONNX Runtime. Rust гоняет те же веса (ONNX-экспорт) через ORT CUDA
  EP без I/O Binding: полный круг CPU↔GPU всех 18 стейтов на каждый кадр.
- Честное сравнение (13 чанков / 64.01 с речи, один голос/seed/EOS/temp):
  Python 3 proc — синтез **8.7–9.3 с (RTF 0.135–0.146, GPU 54–80%)**;
  Rust 3w — 25.3 с (RTF ≈0.40, GPU 36–39%); VRAM 3.2 ГБ против 7.0 ГБ.
  Rust 1w RTF 0.475, CPU 1w 0.691. Воркеры ≥2 не масштабируются: время
  чанка растёт линейно (2.42→4.23→6.22→8.22 с при 1→2→3→4w), GPU Load не
  растёт, 3w×2t ≡ 3w×4t. 4w: VRAM пик 9012 МБ.
- Фазовые таймеры (`SHTORM_PHASE_TIMING=1`, оставлено в движке, выкл по
  умолчанию): main session.run = **93% времени** (33.8 мс/кадр при 1w,
  71.3 при 3w); сборка тензоров/извлечение/RNG <0.1% — «виноваты Vec-копии»
  НЕ подтвердилось.
- ORT Chrome-trace: GPU kernel+memcpy всего **14.6 мс/кадр** против
  33.8 мс wall — 57% времени не GPU-работа. Стейты 49.2 МБ в одну сторону
  (12×[1,1000,16,64] f32) → ~98 МБ PCIe на 80-мс кадр, синхронно;
  полоса PCIe не насыщена (utilization.memory 6–13%) — узкое место
  синхронность/латентность, не пропускная способность.
- CPU-fallback: только shape-узлы (≈222/кадр: Unsqueeze/Gather/Cast/
  Concat…), весь Gemm/MatMul/Conv на CUDA. Падения графа на CPU нет.
- Топ-3 узких места и план **CUDA-P2 (I/O Binding + резидентные стейты
  main/flow, ожидание RTF 1w 0.49→0.26–0.30, масштабирование воркеров)** —
  `docs/CUDA_PROFILE_P1.md` (там же методика, сырые данные в `out/cuda_p1/`).
  Оптимизация НЕ выполнялась — только по отдельному заданию.
- Паритет после инструментирования: WAV побитово `5ca845c0…` (до == после);
  clippy 0, `cargo test` 153 зелёных. Python-проект не менялся (драйвер
  `out/cuda_p1/py_driver.py` только импортирует его).

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
- **G3/G3.1** — безопасный пакетный режим, см. ниже и `docs/G3_BATCH.md`.

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

## G3.1 — что сделано (исправления пакетного режима)

- **Физические пути**: `batch::ensure_no_reparse_components` отказывает, если
  любой существующий компонент выхода — symlink/junction/иной reparse point;
  `physical_path`/`physically_under`/`physically_same` сравнивают пути после
  canonicalize существующего префикса. Поэтому выход, заданный альтернативным
  путём (source через junction, out через реальный путь), распознаётся как
  совпадающий/вложенный; обход источника не раскрывает ссылки.
- **Отмена**: `BatchReport.cancelled` — явный флаг, не выводимый из статусов
  файлов. Покрыты отмена до первого файла, после готового файла, одновременно
  с ошибкой текущего, перед запуском следующего; общий статус `Cancelled`
  даже при `Failed` текущего файла.
- **Конфликты**: план заранее находит `chapter.txt` + `chapter.md` →
  `chapter.<ext>` и регистронезависимые совпадения; все участники —
  `Skipped` с причиной, без синтеза/перезаписи/переименования; конфликты
  есть в `BatchReport.conflicts` и предупреждениях.
- **UI/статусы**: добавлены `BatchStatus::Partial|NoWork` и
  `JobStatus::Partial|NoWork`; K/N обновляется на каждом `FileDone`
  (Failed/Skipped тоже). Пустой/полностью пропущенный пакет — «Нет работы»,
  пакет с ошибками и успехами — «Частично / ЧАСТИЧНЫЙ УСПЕХ».
- Новые тесты: 17 (13 batch + 3 job + 1 UI), моки без ONNX.

## G4.1 — что сделано (настраиваемые параметры EOS, семантика teratts-android)

- **Движок** (`crates/shtorm-engine/src/pocket.rs`): новый `pub enum
  EosTail { Default, Fixed(u32) }`; поля `Pocket::eos_tail_short/long`
  (дефолт `Default` — оригинальная политика 5/3, поведение существующих
  вызовов не меняется). `prepare` разложен на чистую `prepare_text`
  (возвращает текст + число слов ДО паддинга) + оригинальную `prepare`.
  Счётчик EOS из `generate` вынесен в `EosGate` (та же логика, тесты без
  ONNX).
- **Семантика (проверена тестом `eos_gate_counts_frames_including_
  detection_frame` и реальными прогонами)**: при `eos_extra = N`
  генерируется ровно N кадров, ВКЛЮЧАЯ кадр срабатывания EOS (N−1 строго
  после него); N=0 — кадр срабатывания отбрасывается. Комментарий Python
  про «хвост ПОСЛЕ кадра» не соответствует коду teratts-android;
  пересчёт 5→6/3→4 НЕ переносится. Реальный прогон: хвост 10 против 3 на
  длинном чанке — ровно +7 кадров (55 vs 48); порог меняет только кадр
  срабатывания (кадры при том же seed идентичны).
- **None ≡ штатная политика**: WAV движка и конвейера с None/None побитово
  (SHA256) равен Fixed(5)/Fixed(3); `Fixed(0)` — другое состояние (0 кадров
  хвоста).
- **Цепочка**: `GuiSettings` (`eos_threshold`: «Авто»≡−4.0 | число;
  `eos_tail_short/long`: "none"|0–10) → `PipelineOptions` (`eos_tail_*`,
  `eos_threshold: f32` — «Авто» разрешается в −4.0 при конверсии) →
  `real_worker_factory` → `engine_params(opts)` (одинаково для всех
  воркеров) → поля Pocket. EOS из `runtime.json` НЕ читается (тест
  `eos_never_read_from_runtime_json`); `char_rules` читаются как раньше.
- **GUI**: группа «EOS» в «Параметрах генерации» — три ComboBox (Порог:
  Авто/−6…1; «Кадров хвоста EOS» ≤4 слов и ≥5 слов: None/0–10) + кнопка
  «Значения teratts-android» (−4/5/3). Tooltip хвостов описывает
  фактическую семантику счётчика. Дефолты новых конфигураций: −4.0 (Авто),
  Fixed(5), Fixed(3). Старые числовые `eos_threshold` читаются как Fixed
  (без скрытых преобразований); unknown-ключи round-trip как раньше.
  Импорт EOS из Python `settings_pocket_tts.json` НЕ выполняется (решение
  пользователя в ходе G4.1): при импорте остаются дефолты −4/5/3.
- **CLI**: `--eos-threshold` (как было), `--eos-tail-short`,
  `--eos-tail-long` (`none` — отдельное строковое значение; дефолты
  −4.0/5/3 = GUI). `smoke.exe` тоже принимает `--eos-tail-*` и печатает
  число латентных кадров (до trim_edges).
- **Побочный фикс пула** (`crates/app-core/src/pool.rs`): пре-существующая
  гонка — сообщение о сбое СТАРТА воркера могло прийти после завершения
  эмитов и терялось в дренаже (флейк `worker_start_failure_propagates`
  ~13% прогонов); теперь ошибки старта (index: None) фиксируются и в
  дренаже. Ошибки чанков/отмена — семантика не менялась.
- Тесты без ONNX (все 13 пунктов задания): граница 4/5 слов; пустой/
  односоставный/обычный чанк; хвосты 0/1/3/5/10; счётчик на заданной
  последовательности; прямая передача без +1; None ≠ Fixed(0); нестандартные
  пороги incl. 0; GUI→PipelineOptions→движок; одинаковые параметры для всех
  воркеров; save/load; миграция старого JSON; CLI-дефолты == GUI; EOS не
  из runtime.json.

## G4 — что сделано (портативная Windows-сборка)

- `crates/gui/src/resources.rs` — единый резолвер: явный путь → подкаталоги
  портативного комплекта (`models/`, `onnxruntime/cpu|cuda/`, `ffmpeg/`) →
  каталоги проекта (dev) → документированный fallback. Относительные пути
  настроек разрешаются от каталога exe, не от CWD. `Cargo.toml`/рабочий
  каталог исходников для портативного режима не нужны.
- CPU и CUDA ORT разрешаются независимо; CUDA-каталог добавляется только в
  PATH процесса; один ORT runtime на процесс (смена CPU↔CUDA — перезапуск).
- Настройки по умолчанию без абсолютных dev-путей: `onnx_base` пуст,
  портативный пример использует `models`, `onnxruntime/...`,
  `ffmpeg/ffmpeg.exe`, `output`.
- `tools/build-portable.ps1` + `tools/portable/*`: новый каталог вне
  репозитория, отказ при существующем, без удалений; проверка обязательных
  файлов, отсутствие INT8, размер `flow_lm_main.onnx`, запуск ffmpeg,
  JSON настроек, размер+SHA256 каждого файла, manifest. Добавлены
  `-VcRuntimeDir`, `-HardLinkPayload` (проверенные жёсткие ссылки для
  крупных файлов на том же томе).
- В комплект входят: exe, MSVC++ runtime (4 DLL), CPU ORT (2 DLL),
  CUDA ORT + providers + CUDA/cuDNN (15 DLL), `ffmpeg.exe`, FP32 fast/slow
  (6 файлов каждый, без INT8), пример настроек с относительными путями,
  README, notices, GPL/ORT/FFmpeg/NVIDIA/model/MSVC лицензии, Cargo.lock.
- Диагностические режимы exe: `--portable-check`, `--portable-selftest`,
  `--portable-selftest-batch` (тот же execute_job/execute_batch, без кликов).
- Документация: `docs/PORTABLE_BUILD.md`, обновлены `GUI_PLAN.md`,
  `NEXT_STEPS.md`.

## G5 — что сделано (полировка GUI: настройки, EOS, безопасная перезапись)

- **Импорт Python-GUI удалён**: `settings::import_from_python` и кнопка
  «Импорт из Python-GUI (только чтение)» убраны; приложение читает только
  собственный `shtorm_gui_settings.json`. Старые конфиги Python больше не
  подхватываются автоматически.
- **EOS без «Авто»**: в ComboBox порога остались только числа
  (−6…1); дефолт `GuiSettings` — `Fixed(-4.0)`. Устаревший `"auto"` в
  старом JSON читается как `Fixed(-4.0)`, новый JSON всегда пишет число.
  Enum `Auto` сохранён только как совместимый алиас.
- **`overwrite_existing: bool`** в `GuiSettings` (дефолт `false`), чекбокс
  «Перезаписывать существующие файлы» в группе вывода. При выключенном
  флаге существующий результат — не ошибка, а `JobStatus::Skipped`
  («Пропущено: файл уже существует»), файл остаётся побайтово неизменным.
- **Безопасная замена** (`audio-processing::stream::publish_replace`,
  `sink::{WavSink,FfmpegSink}::create_with_overwrite`,
  `make_sink_with_overwrite`): sink пишет уникальный `.partial`, старый
  файл НЕ удаляется заранее. При успешном `finalize` — атомарная
  `MoveFileExW REPLACE_EXISTING|WRITE_THROUGH`; при ошибке/отмене старый
  файл остаётся, `.partial` сохраняется. Схема «удалить → переименовать»
  не используется. Каталоги и reparse-точки в качестве итога отвергаются.
- **Гонка «воркеры готовы → Стоп → публикация»** закрыта в
  `pipeline.rs`: отмена до `sink.finalize()` понижает статус до
  `Cancelled`, ничего не публикуется.
- **Пакетный режим**: `run_batch(..., overwrite, ...)` пропускает
  существующие результаты только при `overwrite=false`; конфликты имён
  по-прежнему запрещены даже при перезаписи. Отчёт помечает заменённые
  файлы «Файл обновлён».
- **Защита путей** в одиночном режиме: `job::ensure_output_path_safe`
  (reparse-точки, выход за каталог результата, совпадение с исходным
  текстом/голосом/моделью) — та же физическая проверка, что в G3.1.
- **Полировка журнала**: кнопка «Копировать журнал» (в буфер обмена),
  «Очистить журнал» очищает только отображаемую копию и сообщает, что
  файлы на диске не тронуты.
- **Тема**: переключатель «Тёмная тема» (дефолт — тёмная, `theme_dark` в
  конфиге); применяется только при изменении настройки.
- **Ошибки ORT/CUDA**: при сбое `ort::init()` сообщение дополняется
  подсказкой по устройству (CUDA — драйвер/провайдеры/VRAM; CPU — VC++
  runtime и целостность DLL).
- Тесты: `overwrite_existing_default_off_and_round_trip`, миграция
  `auto`→`-4.0`, `overwrite_true_resynthesizes_existing_result`,
  перезапись/отказ/каталог в `stream.rs`, `Skipped` в `job.rs`.
  `cargo test` — 158 passed / 0 failed; clippy 0.

## Подтверждённые результаты

- `cargo test` **158 passed / 0 failed** (14 ignored: синтетические ONNX/
  real-тесты, в т.ч. новые CUDA-тесты P2A.1); `cargo clippy --all-targets`
  **0**; release-сборка OK (shtorm-gui.exe запускается, окно живо; флейк
  worker_start_failure устранён фиксом дренажа пула).
- **Реальные проверки G2** (`cargo test -p shtorm-gui --release --lib
  real_matrix_device -- --ignored --nocapture`, второй прогон с
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
- **Реальный пакетный тест G3/G3.1** (CPU FP32): 5 файлов (3 валидных во
  вложенных каталогах, 1 битый не-UTF8, 1 существующий результат) →
  `готово 3 | ошибок 1 | пропущено 1`, статус **«Частично / ЧАСТИЧНЫЙ
  УСПЕХ»**, 9.3 с; относительные пути сохранены, существующий файл не
  перезаписан, прогресс K/N и чанков шёл событиями.
- **G4 портативный комплект**:
  `C:\PocketTTS-GUI-Rust\dist\ShtormTTS-Portable-2026-11-09-g4e` — 54 файла,
  логический размер 3403.1 MiB, manifest со SHA256 (git `5a6a766`,
  dirty=false); exe SHA256 `54268A82…DF6C44E`. Тестовые сборки g4c/g4d имеют
  тот же exe SHA256; финальная g4e собрана после docs-коммита с чистыми
  метаданными и актуальным docs/PORTABLE_BUILD.md.
  Крупные файлы собраны `-HardLinkPayload` (фактический расход диска на том
  же томе мал; при копировании папки материализуются).
  - `--portable-check` из `C:\` без Cargo.toml: модель/ORT/ffmpeg из
    относительных путей — OK.
  - CPU FP32 WAV: Completed, 6.80 с, RTF 0.932, 326444 байта.
  - CUDA FP32 WAV (RTX 3060): Completed, 6.80 с, RTF 0.783, 326444 байта.
  - MP3/Opus: Completed через комплектный ffmpeg (ID3/OggS).
  - Batch: 2/2 Completed, 12.6 с, подкаталоги сохранены.
  - **WAV-паритет GUI==CLI**: CPU 326444 байта, одинаковый SHA256
    `80CB7288…`; CUDA 326444 байта, одинаковый SHA256 `85A26977…`.
  - CPU-процесс загрузил только `onnxruntime\cpu\onnxruntime.dll`;
    CUDA-процесс — `onnxruntime\cuda\onnxruntime.dll`, providers, CUDA/cuDNN
    из комплекта. MSVC++ runtime загружен из корня комплекта, не из System32.
    `cudnn_adv64_9.dll`/`cudnn_cnn64_9.dll` присутствуют, но в тесте не
    загружались (оставлены как стандартный набор cuDNN).
  - Чистая Windows VM без Python/Torch/ORT/FFmpeg **не проверялась**.
- Реальный аварийный путь (CLI, битый путь модели): типизированный
  StreamError «обработано 0/1, base_seed, черновик», диагностический
  журнал создан, итогового файла нет.
- **G4.1 реальные EOS-прогоны** (Shtorm-v2 slow, seed 42, RTF CPU ≈0.87–
  0.93 / CUDA ≈0.61–0.76; движок `smoke.exe` — кадры ДО trim_edges,
  конвейер `synthesize.exe` — 2 чанка, длительность ПОСЛЕ trim_edges):
  - Android-дефолт −4/5/3 CPU: короткий чанк 21 кадр (1.68 с), длинный 48
    кадров (3.84 с); конвейер 7.80 с (речь 7.30 + паузы 0.50), WAV
    374444 байта — как в G2/G4 (регрессии нет).
  - Android-дефолт −4/5/3 CUDA (VRAM Δ+1.4 ГиБ): те же 21/48 кадров,
    7.80 с; RTF 0.746–0.762.
  - Минимальный хвост −1/0/0 CPU: короткий 20 кадров, длинный 48 (EOS при
    −1 срабатывает позже; конвейер 7.86 с, речь 7.36 — хвостовые кадры
    почти полностью срезаются trim_edges, т.к. это тишина).
  - Увеличенный хвост −4/10/10 CPU: длинный 55 кадров = ровно +7 к хвосту 3
    (48) — прямое подтверждение «N кадров, включая кадр срабатывания».
  - Штатный fallback −4/None/None CPU: побитово (SHA256) равно −4/5/3 —
    и в движке, и в конвейере.
  - **GUI↔CLI WAV-паритет**: дефолты −4/5/3 — 374444 байта (real_matrix,
    CPU); нестандартные −1/0/0 — 377324 байта и None/None — 374444 байта
    (real_eos_parity, CPU) — побитово совпадают.
- **G4.1 портативный комплект**: финальный
  `dist\ShtormTTS-Portable-2026-11-09-g41-final` (manifest git `824f2a1`,
  dirty=false, 54 файла, 3403.1 MiB, exe SHA256 `6A23DDE8…D9797963`;
  штатные настройки — новый формат auto/5/3) и тестовый `-g41` (тот же exe):
  `--portable-check`, CPU selftest (старый числовой конфиг — миграция;
  `"auto"`/5/3 — WAV побитово `80CB7288…` = базе G4), −1/0/0 (параметры
  проходят), CUDA selftest (RTF 0.762–0.767), batch 2/2, окно GUI, паритет
  комплект-GUI == CLI (`80CB7288…`). Детали — `docs/PORTABLE_BUILD.md`;
  комплекты G4 не тронуты.

## Состояние подсистем

- **CPU FP32** — работает. **CUDA FP32** — работает (deps/portable, TF32
  выключен принудительно; VRAM-дельта — обязательная проверка GPU).
  Денойз DPDFNet: код готов; без файла модели — явный статус Missing.
- **WAV/MP3/Opus streaming** — готов (CLI и GUI).
- **GUI G2/G3/G3.1/G4/G4.1/G5** — одиночный файл, безопасный пакетный
  режим, портативная сборка, настраиваемые EOS (группа «EOS»: порог + оба
  хвоста + кнопка teratts-android) и G5-полировка (без импорта Python,
  EOS без «Авто», явный флаг перезаписи с атомарной заменой): полный цикл
  запуск/прогресс (K/N + чанки)/Стоп/статусы/журналы/отчёт/закрытие;
  exe-relative модели/ORT/ffmpeg, CPU/CUDA раздельно, комплектный ffmpeg
  и VC++ runtime.

## Актуальные ограничения / нерешённое

- ORT грузится в процесс GUI один раз: смена CPU↔CUDA внутри одной сессии
  GUI требует перезапуска приложения (сообщение в UI).
- ~~Auto-EOS/`frames_after_eos`~~ — закрыто в G4.1/G5: параметры EOS
  задаются явно (−4/5/3 по teratts-android); G5 убрал «Авто» из списка —
  дефолт и миграция старого `"auto"` дают `Fixed(-4.0)`; EOS из
  `runtime.json` намеренно не читается. CUDA I/O Binding — **обычный путь
  с CUDA-P2A.1**;
  старый путь — `SHTORM_CUDA_IO_BINDING=0` (диагностика). Ограничения
  пути: только CUDA, только main-граф (flow/decoder — старый путь),
  статические формы состояний.
- CUDA-P2A/P2A.1: 2 воркера дают лишь ~4% wall (1w 8.79–8.89 с против
  8.53–8.56 с), 3-й не нужен; причина — shape-узлы CPU и синхронизации
  (см. `docs/CUDA_PROFILE_P2A.md`).
- Портативный пакет проверен на машине разработки; чистая Windows VM без
  Python/Torch/ORT/FFmpeg **не проверялась**. Модели и голосовой пример не
  имеют подтверждённой лицензии на публичную редистрибуцию (личное
  использование; notice в комплекте). `cudnn_adv64_9.dll`/
  `cudnn_cnn64_9.dll` входят в набор, но в тестовом прогоне не загружались.
  Локальные сборки g4c/g4d/g4e используют `-HardLinkPayload` для крупных
  файлов; при копировании папки на другой диск они материализуются.
- MSVC++ runtime (4 DLL) входит в комплект; без него чистая Windows без
  VC++ Redistributable не запустит exe/ORT.
- Переиспользование пула воркеров между файлами не реализовано (сейчас пул
  создаётся/освобождается на файл); оценка изменений/рисков —
  `docs/G3_BATCH.md`. Фиктивного переключателя нет.
- Денойз не прогонялся с реальной моделью DPDFNet; decode_steps>1 не мерили.
- WAV > ~4 ГиБ — явная ошибка (RF64/разбиение — будущая функция).
- RTF GUI включает загрузку моделей (короткие тексты: RTF>1 — норма).
- Ручной клик-тест окна (кнопки/прогресс глазами) не выполнялся —
  автоматизированные проверки состояния/событий + запуск exe есть;
  полноценный интерактивный проход остаётся на усмотрение пользователя.
- G5-перезапись проверена юнит-тестами и моками (WAV `.partial` +
  MoveFileExW REPLACE, ffmpeg `-n` → замена после кодирования, каталог/
  reparse-точка отвергаются, старый файл сохраняется при ошибке/отмене).
  Реальный CPU WAV: успешная замена старого файла подтверждена
  (`real_overwrite_wav`: «ФАЙЛ ОБНОВЛЁН», `.partial` убран); отмена до
  публикации оставляет старый файл целым и `.partial` для диагностики.
  Реальные MP3/Opus и CUDA с overwrite — по отдельному заданию.

## Структура и ключевые файлы

```
Cargo.toml            # workspace: shtorm-engine, text-processing,
                      #   audio-processing, app-core, gui
crates/shtorm-engine/src/pocket.rs     # движок (FP32-only, Device, seed)
crates/shtorm-engine/src/io_states.rs  # CUDA-P2A: IoBinding + GPU-стейты main
crates/shtorm-engine/examples/graph_meta.rs  # метаданные графов (диагностика)
crates/shtorm-engine/tests/models/     # синтетические ONNX (P2A; ген. tools/gen_p2a_models.py)
crates/text-processing/src/{chunking,char_rules,pauses}.rs
crates/audio-processing/src/stream.rs  # StreamingWav, publish_no_replace
crates/app-core/src/pipeline.rs        # synthesize_stream, StreamError,
                                       #   write_diag_log, write_chunk_log
crates/app-core/src/pool.rs            # run_job (воркеры, окно, отмена)
crates/app-core/src/sink.rs            # WavSink/FfmpegSink, bytes_fed
crates/gui/src/job.rs                  # G2: execute_job/execute_batch, ORT env
crates/gui/src/resources.rs            # G4: exe-relative модели/ORT/ffmpeg
crates/gui/src/batch.rs                 # G3: план/обход/пути/отчёт (моки)
crates/gui/src/app.rs                  # форма + задание/прогресс K/N/Стоп
crates/gui/src/settings.rs             # GuiSettings (+batch/dll/threads/пресеты)
crates/gui/src/main.rs                 # eframe + --portable-check/selftest
tools/build-portable.ps1               # G4: сборка комплекта (без удалений)
tools/portable/*                       # пример настроек, notices, тексты
tools/*.ps1, tools/gen_*_golden.py     # smoke и генераторы golden
tests/golden/*.json                    # 96 + 314 + 20 кейсов
docs/SAFETY_AUDIT.md, docs/GUI_PLAN.md, docs/NEXT_STEPS.md,
docs/G3_BATCH.md, docs/PORTABLE_BUILD.md,
docs/FEATURE_PARITY.md, docs/BENCHMARKS.md,
docs/CUDA_PROFILE_P1.md, docs/CUDA_PROFILE_P2A.md
```

## Сборка / запуск / проверки (Windows, Git Bash)

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release                       # всё (вкл. shtorm-gui.exe)
cargo test && cargo clippy --all-targets    # норма: 0 failed / 0 warning
# реальные проверки GUI-задания (по одному устройству на процесс):
cargo test -p shtorm-gui --release --lib real_matrix_device -- --ignored --nocapture
SHTORM_GUI_TEST_DEVICE=cuda cargo test -p shtorm-gui --release --lib \
  real_matrix_device -- --ignored --nocapture
# реальный пакетный CPU-тест G3 (5 файлов, вкл. битый и существующий):
cargo test -p shtorm-gui --release --lib real_batch_cpu -- --ignored --nocapture
# G4.1 реальный GUI↔CLI паритет нестандартных EOS (−1/0/0 и none/none):
cargo test -p shtorm-gui --release --lib real_eos_parity -- --ignored --nocapture
# CUDA-P2A синтетика (CPU; на CUDA добавить SHTORM_P2A_DEVICE=cuda и PATH deps/portable):
ORT_DYLIB_PATH=deps/ort-cpu/onnxruntime-win-x64-1.23.2/lib/onnxruntime.dll \
  cargo test --release -p shtorm-engine --lib io_states -- --ignored --nocapture
# GUI: ./target/release/shtorm-gui.exe
# G4 portable (новый каталог вне репозитория; см. docs/PORTABLE_BUILD.md):
powershell -ExecutionPolicy Bypass -File tools/build-portable.ps1 `
  -Destination C:\PocketTTS-GUI-Rust\dist\ShtormTTS-Portable-<date> `
  -ModelsDir <onnx> -Ffmpeg <ffmpeg.exe> -VoiceFile <voice.wav>
```
Настройки real_matrix-теста: ru-v2-slow, voice
`C:\PocketTTS-GUI\examples\Microsoft Brian Speed 1.wav`, ffmpeg
`C:\PocketTTS-GUI\ffmpeg\ffmpeg.exe`, seed 42, threads 4 (CPU 2w / CUDA 1w).

## NEXT TASK (НЕ НАЧИНАТЬ без явного задания)

CUDA-P1, CUDA-P2A, CUDA-P2A.1 и G5 (настройки/EOS/перезапись) завершены
(см. разделы выше). Кандидаты следующего этапа:

1. **CUDA-P2B** — только по отдельному заданию. Приоритеты (обоснование —
   `docs/CUDA_PROFILE_P2A.md` §9): (а) shape-узлы/межEP-синхронизации
   (222 CPU-узла/кадр, главный простой GPU), (б) масштабирование воркеров
   (3w ≈ 1w по wall: 9.8/9.4/9.8 с), (в) flow/decoder I/O Binding (малый
   выигрыш: flow 1.37 мс/кадр при 1w). Не начинать автоматически.
2. **G5 — остаток полировки**: интерактивный клик-проход GUI, темы
   оформления, RTF без времени загрузки моделей, уточнение текстов ошибок
   CUDA/VRAM. Реальный прогон синтеза с `overwrite_existing=true` на
   CPU/CUDA (WAV/MP3/Opus) — по отдельному заданию.
3. Переиспользование пула воркеров между файлами — после замеров старта/
   памяти (`docs/G3_BATCH.md`).
4. Денойз DPDFNet с реальной моделью.
5. Чистая Windows VM без Python/Torch/ORT/FFmpeg — отдельная проверка
   портативного пакета.

## Критерии готовности G5

- [x] Импорт Python-GUI удалён; EOS без «Авто», дефолт `Fixed(-4.0)`.
- [x] `overwrite_existing` + безопасная атомарная замена аудио.
- [ ] Интерактивный проход GUI выполнен и описан; регрессии не выявлены.
- [x] `cargo test`/clippy зелёные; safety-политика (create_new, без
  рекурсивных удалений; замена только с явного согласия и только после
  успешного синтеза) сохранена.
- Портативный комплект не изменяется без отдельного задания; модели и
  лицензии остаются задокументированными.
