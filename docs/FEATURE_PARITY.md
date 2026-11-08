# FEATURE_PARITY — Python GUI ↔ Rust-движок/каркас

Статусы: ✅ готово в каркасе · 🔁 есть в teratts-android (вендорить при этапе) · 📝 порт из Python (нет готового Rust) · ⬜ не начато. «Источник» — где взять реализацию.

| Функция Python GUI | Статус | Источник | Объём изменений |
|---|---|---|---|
| **Загрузка модели Shtorm-v2 (ONNX FP32)** | ✅ | pocket.rs (вендорен) | уже: FP32-only, выбор каталога |
| **Загрузка голоса: WAV любой частоты/битности, 30 с, кэш** | ✅ | pocket.rs `read_voice`/`voice` | нет |
| **Нотация ударений `+`/U+0301 (NFC, ё)** | ✅ | text-processing (порт `to_model_stress_notation`) | нет; тесты 1:1 |
| **temperature / decode_steps / eos_threshold** | ✅ | pocket.rs поля | нет |
| **EOS-хвост «≤4 слов → 5, иначе 3» (teratts)** | ✅ | pocket.rs `prepare` | нет |
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
| **Паузы × масштаб, пауза абзаца, вставка тишины при склейке (сквозной конвейер)** | ⬜ (A3) | база готова: pauses.rs chunk_pause_s | склейка чанков + WAV/ffmpeg-стриминг — этап A5 |
| **«Задваивать запятые» (чекбокс test-GUI)** | 📝 | `_patch_dup_log.py` | тривиально после чанкования |
| **Параллельные воркеры (1–12, очереди, порядок, отмена)** | ⬜ | новая реализация | потоки + crossbeam/каналы; семантика = GUI (per-chunk seed делает порядок неважным) |
| **Перезапуск воркеров каждые N файлов** | ⬜ | GUI `pool_factory` | в Rust пересоздание `Pocket` в потоке; следить за VRAM/RAM (bench покажет, нужен ли вообще) |
| **Потоковая запись WAV / MP3 / Opus (ffmpeg-пайп)** | ⬜ | GUI `_synthesize_file_streaming` | hound-стример/WAV напрямую; MP3/Opus — `std::process` ffmpeg с теми же флагами кодирования |
| **Лог чанков `{имя}_log.txt`** | ⬜ | GUI `_write_chunk_log` | тривиально |
| **Рекурсивная обработка папки (.txt/.md/.log)** | ⬜ | GUI `_process_directory_recursively` | `walkdir` |
| **Отмена (Stop), прогресс** | ⬜ | GUI cancel_event/progress_queue | AtomicBool + каналы |
| **GUI (eframe/egui), свёртки, тёмная тема, settings.json** | ⬜ | — | этап после конвейера |
| **WSOLA-темп (SoundStretch)** | 🔁 (опция) | stretch.rs | не входил в скоуп GUI; решить по необходимости |
| Гомографы/автоударения | — | — | **отказано по требованию** (вход подготовлен) |
| xVibe/safetensors | — | — | **вне скоупа** (ставка на Shtorm-v2 ONNX) |

## Порядок порта (зависимости)

чанкование → char_rules → паузы+склейка → воркеры → ffmpeg/WAV-стриминг → файловый конвейер → GUI → портативная сборка (см. NEXT_STEPS.md).
