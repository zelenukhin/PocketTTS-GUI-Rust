# FEATURE_PARITY — Python GUI ↔ Rust-движок/каркас

Статусы: ✅ готово в каркасе · 🔁 есть в teratts-android (вендорить при этапе) · 📝 порт из Python (нет готового Rust) · ⬜ не начато. «Источник» — где взять реализацию.

| Функция Python GUI | Статус | Источник | Объём изменений |
|---|---|---|---|
| **Загрузка модели Shtorm-v2 (ONNX FP32)** | ✅ | pocket.rs (вендорен) | уже: FP32-only, выбор каталога |
| **Загрузка голоса: WAV любой частоты/битности, 30 с, кэш** | ✅ | pocket.rs `read_voice`/`voice` | нет |
| **Нотация ударений `+`/U+0301 (NFC, ё)** | ✅ | text-processing (порт `to_model_stress_notation`) | нет; тесты 1:1 |
| **temperature / decode_steps / eos_threshold** | ✅ | pocket.rs поля | нет |
| **EOS-хвост «≤4 слов → 5, иначе 3» (teratts)** | ✅ | pocket.rs `prepare`/`prepare_text` | нет |
| **Настраиваемые параметры EOS (G4.1/G5, намеренное отличие от Python)** | ✅ (2026-11-09) | pocket.rs `EosTail`/`EosGate` | **НЕ порт Python**: порог (−6…1; G5 убрал «Авто», дефолт и миграция `auto` → `Fixed(-4.0)`) + хвосты None/0–10 на ветку числа слов; значение = `eos_extra` teratts-android напрямую (N кадров, включая кадр срабатывания), Python-пересчёт 6/4 не используется; EOS из runtime.json не читается; GUI/CLI/JSON-дефолты −4/5/3 |
| **Детерминированный seed на чанк (seed+index)** | ✅ | добавлено в pocket.rs | проверено побитово |
| **Анти-повторы (2 попытки, лимит 11 симв/с+1с)** | ✅ | pocket.rs `say` | вынесен в флаг `anti_repeat` |
| **High-shelf яркость (RBJ, клип-гард 0.98)** | ✅ | pocket.rs `high_shelf` | нет |
| **DPDFNet-денойз (48 кГц, −12 дБ, стримингово)** | ✅ код / ⬜ сквозной прогон | denoise.rs | модель-файл положить в каталог; HF-качалки нет намеренно |
| **trim_edges (−45 дБ, lead/tail)** / **fade_out 10 мс** | ✅ | dsp.rs (из tera.rs) | fs-параметр вместо 44100 |
| **Ресемплинг (sinc 16 tap)** | ✅ | pocket.rs `resample` | нет |
| **WAV 16-бит запись** | ✅ | audio-processing | нет |
| **Настройки (имена полей = GUI, clamp'ы)** | ✅ (каркас) | app-core settings.rs | поля чанкования/ffmpeg добавятся по мере порта |
| **CPU FP32 инференс (потоки настраиваются)** | ✅ | session_builder | нет |
| **CUDA FP32 инференс + диагностика fallback** | ✅ базово | session_builder + smoke | io-binding — оптимизация (CUDA_FEASIBILITY) |
| RAM/VRAM-диагностика | ✅ | app-core diagnostics | нет |
| **char_rules (runtime.json: char_map, regex_rules, dash_context, bracket_comma, delete_chars + нормализация)** | ✅ (A2, 2026-11-08) | text-processing `char_rules.rs` (порт ветки comma-semantics) | **golden 314 кейсов** (tools/gen_char_rules_golden.py): кавычки/тире/проценты/скобки/U+0301/хвостовая запятая — побуквенно |
| **Паузы teratts по знаку (0.15–0.65 с), trailing_pause_s** | ✅ (база A3) | text-processing `pauses.rs` | эталон с Python (tools/pause_reference.py); **исправление**: приоритет is_last над soft_break — пауза абзаца больше не теряется после terminate_bare_chunks (регрессионный тест; Python-баг задокументирован) |
| **Чанкование (абзацы→предложения→:;—→запятая→«и»→midpoint, байтовый лимит, soft_break)** | ✅ (A1, 2026-11-08) | text-processing `chunking.rs` (порт pocket-tts_gui.py) | **совпадение с Python побуквенно: golden 96 кейсов** (tests/golden/chunking_golden.json, генератор tools/gen_chunking_golden.py из ветки comma-semantics через `git show`, проект не менялся) |
| **comma-semantics: `terminate_bare_chunks`, сохранение хвостовой запятой, comma_pause=0** | ✅ (A1) | ветка GUI `comma-semantics` | `terminate_bare_chunks` в том же golden-конвейере; сохранение хвостовой запятой — в char_rules (A2, ещё не портировано); comma_pause=0 — в паузах (A3) |
| **Паузы × масштаб, пауза абзаца, вставка тишины, склейка в один WAV (конвейер A3)** | ✅ (A3, 2026-11-08) | app-core `pipeline.rs` + CLI `synthesize` | **golden пауз 20 кейсов** (чанки+паузы+сэмплы; расхождение только задокументированное is_last>soft_break — 4 узла, сверка с python/main и rust-эталоном); порядок/без дубликатов/без паузы после последнего — юнит-тесты assemble; CPU/CUDA смоук: 3-абзацный текст → один WAV 18.45 с (речь 16.66 + паузы 1.80), RTF 0.87 CPU / 0.61 CUDA |
| **«Задваивать запятые» (чекбокс test-GUI)** | 📝 | `_patch_dup_log.py` | тривиально после чанкования |
| **Параллельные воркеры (1–N, очереди, порядок, отмена)** | ✅ (A4, 2026-11-08) | app-core `pool.rs` + `worker.rs` + `pipeline::synthesize_parallel` | N воркеров × собственный Pocket; ограниченные очереди + скользящее окно (backpressure, буфер ≤ окна); порядок строгий; seed = base+индекс (не зависит от воркера); ошибка чанка/старта/sink останавливает задание; отмена без зависаний (send_timeout + join); Pocket дропаются в потоках воркеров до возврата; **детерминизм: CPU и CUDA 1w vs 2w — побитово** (3 и 47 чанков); CLI `--workers`, `--seed -1` → base_seed в логе. Сценарные тесты пула — на моках без ONNX (8 шт.: порядок/1v2/ошибка/отмена/ранний-долгий/reuse/старт-сбой) |
| **Перезапуск воркеров каждые N файлов** | ⬜ | GUI `pool_factory` | в Rust пересоздание `Pocket` в потоке; следить за VRAM/RAM (bench покажет, нужен ли вообще) |
| **Потоковая запись WAV / MP3 / Opus (ffmpeg-пайп)** | ✅ (A5/A5.1, 2026-11-09) | `audio-processing::stream::StreamingWav` + `app-core::sink::FfmpegSink` | PCM сразу в файл/stdin ffmpeg; аргументы 1:1 (libmp3lame -q/-b, libopus -b); уникальный `.partial` за запуск + **MoveFileW-публикация без замены**; `create_new`; RIFF-лимит; stderr до EOF (хвост 8 КиБ); abort/Drop; Ctrl+C → Cancelled + диагностический журнал, exit 2; ffprobe/декод проверены |
| **Лог чанков `{имя}_log.txt`** | ✅ (A5) | CLI `--chunk-log` | текст/номер/воркер/время/речь/пауза/denoise/seed/устройство/статус; ошибка журнала не маскирует аудио |
| **Рекурсивная обработка папки (.txt/.md/.log)** | ⬜ | GUI `_process_directory_recursively` | `walkdir` |
| **Отмена (Stop), прогресс** | ✅ backend (A5.1) | `synthesize_stream(cancel, progress)` + CLI Ctrl+C | единый RunStatus Completed/Cancelled/Failed; GUI-кнопка Stop подключит тот же флаг; прогресс-хук `(done,total)`; неполный файл не публикуется |
| **GUI (eframe/egui), свёртки, тёмная тема, settings.json** | ⬜ | — | этап после конвейера |
| **WSOLA-темп (SoundStretch)** | 🔁 (опция) | stretch.rs | не входил в скоуп GUI; решить по необходимости |
| Гомографы/автоударения | — | — | **отказано по требованию** (вход подготовлен) |
| xVibe/safetensors | — | — | **вне скоупа** (ставка на Shtorm-v2 ONNX) |

## Порядок порта (зависимости)

чанкование → char_rules → паузы+склейка → воркеры → ffmpeg/WAV-стриминг → файловый конвейер → **GUI G1 ✅ (каркас)** → G2 синтез из GUI → портативная сборка (см. NEXT_STEPS.md).
