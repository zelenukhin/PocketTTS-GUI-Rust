# HANDOFF.md — оперативный снимок проекта (читать первым вместе с AGENTS.md)

- **Дата обновления**: 2026-11-10 (G5.1: порядок проверки путей, фазы
  задания и честная отмена публикации, семантика сообщений перезаписи).
- **HEAD**: только из Git (`git log -1 --oneline`), не из этого файла.
- **Бэкап на NAS (верифицирован, сделан ПЕРЕД изменениями G5.1)**: полный
  архив HEAD `4e3da62` —
  `\\DISKSTATION\Ableton 2\Temp2\ShtormTTS-Windows-full-4e3da62-20261110-023058.zip`,
  SHA256 `B4685DFA8F1066F4F5045DC3E4BF80A46E60EEE17A2B1EE1728F2D419F71F6E2`,
  144/144 blob совпадают. Прежние архивы не тронуты.

## Текущий функционал

Нативный Windows x64 GUI (eframe/egui) для синтеза аудиокниг моделями
Shtorm-PocketTTS-RU v2 fast/slow (ONNX FP32, CPU и CUDA RTX 3060):
чанкование/char_rules/паузы с golden-паритетом к Python, потоковая запись
WAV/MP3/Opus без накопления аудио в памяти, одиночный и безопасный пакетный
режим, портативная сборка без установки (модели/ORT/ffmpeg/VC++ runtime),
настраиваемые EOS (−4/5/3 teratts-android по умолчанию), явный флаг
перезаписи с атомарной заменой, тёмная тема. Приложение читает только
свой `shtorm_gui_settings.json` (импорта Python-настроек нет).

## Архитектура (кратко; подробно — AGENTS.md, docs/ARCHITECTURE.md)

- `crates/shtorm-engine` — ONNX-движок (порт teratts-android), FP32-only,
  `Device::{Cpu,Cuda}`; CUDA: I/O Binding состояний main — обычный путь
  (`io_states.rs`; реверс `SHTORM_CUDA_IO_BINDING=0`; только main-граф).
- `crates/text-processing` — chunking (golden 96), char_rules (314),
  паузы (20; задокументированное расхождение приоритетов с Python).
- `crates/audio-processing` — `stream.rs`: `StreamingWav` (уникальный
  `.partial`, create_new), `finish()` закрывает контейнер, отдельная
  необратимая `FinalizedWav::publish()` (MoveFileW без замены /
  MoveFileExW REPLACE|WRITE_THROUGH при overwrite), RIFF-лимит.
- `crates/app-core` — `pipeline.rs` (`synthesize_stream` с фазами
  `RunPhase::{Generating,Finalizing,Publishing}`, `StreamError` c
  `publish_failed`), `pool.rs` (воркеры, backpressure, отмена), `sink.rs`
  (`AudioSink::finish` + `FinalizedSink::publish`; WavSink/FfmpegSink),
  CLI-бинари (smoke/bench/synthesize).
- `crates/gui` — `settings.rs` (round-trip, атомарная запись),
  `app.rs` (форма, фазы → отключение «Стоп» и статус «Сохранение
  результата…»), `job.rs` (execute_job/execute_batch, безопасность путей:
  проверка ДО создания каталогов, `create_dir_all_no_reparse`,
  повторная валидация), `batch.rs` (план/обход/отчёт), `resources.rs`
  (exe-relative резолвер), `main.rs` (+ `--portable-*` режимы).
- `tools/build-portable.ps1` — портативный комплект; `tests/golden/` —
  golden-кейсы; `docs/` — подробные документы.

## G5.1 — что сделано (текущий этап)

- **Порядок проверки путей** (`gui/job.rs`): `ensure_output_path_safe`
  (reparse-точки, физическая принадлежность каталогу результата,
  совпадение с текстом/голосом/моделью) выполняется СТРОГО ДО создания
  каталогов; недостающие каталоги создаёт `create_dir_all_no_reparse` —
  каждый существующий компонент проверяется на junction/symlink, по
  ссылкам создание не идёт (обычный `create_dir_all` прошёл бы молча);
  после создания пути валидируются повторно (окно TOCTOU закрыто
  canonicalize-сравнением). Ошибка резолва модели не пропускает проверку
  молча — используется уже разрешённый `model_dir`.
- **Фазы задания и точка решения о публикации**: `AudioSink` разделён на
  `finish()` (дожать энкодер/закрыть контейнер) и `FinalizedSink::publish()`
  (необратимый перенос/замена). Точка принятия решения — единая, между
  генерацией и финализацией: отмена до неё → Cancelled без публикации,
  после неё конвейер доходит до конца независимо от флага. UI получает
  `JobEvent::Phase`: на Finalizing/Publishing кнопка «Стоп» отключается,
  статус показывает «Сохранение результата…»; в пакете между файлами
  «Стоп» возвращается. Отмена после точки решения НЕ занижает статус:
  задание честно завершается Completed.
- **Сообщения перезаписи** (одиночный и пакетный режимы — одна семантика):
  «ФАЙЛ ОБНОВЛЁН (старый заменён)» — только когда готовый файл реально
  существовал; «ГОТОВО (создан новый файл)» — новый файл (даже при
  overwrite=true); «Пропущено» — существующий при выключенной перезаписи;
  «ОТМЕНЕНО … замена НЕ выполнена — старый файл сохранён побайтово»;
  «ОШИБКА ПУБЛИКАЦИИ» — сбой собственно публикации (кодирование прошло),
  старый файл не изменён. `JobResult.replaced_existing` отделает замену
  от создания; пакетный отчёт помечает «файл обновлён (старый заменён)» /
  «создан новый файл».
- **Тесты**: детерминированные барьерные тесты конкурентной отмены
  (порядок событий — Barrier + флаги, без sleep): отмена внутри
  Publishing не предотвращает замену и не занижает статус; отмена во время
  генерации не вызывает publish вовсе; последовательность фаз. Песочницы
  %TEMP%: junction в родителе результата — отказ ДО создания (по ту
  сторону ссылки ничего не создаётся), create_dir_all_no_reparse на
  чистых/junction/file-путях, выход внутри каталога модели.

## Результаты последних проверок (2026-11-10, Windows, release)

- `cargo test` — **167 passed / 0 failed / 18 ignored** (engine 15+11i,
  app-core 48, gui 62+7i, audio 16, text 24 + 2 интеграционных;
  ignored — реальные CUDA/ONNX-прогоны: запускать по командам ниже);
  `cargo clippy --all-targets` — 0 предупреждений; fmt; release OK.
- **CPU FP32** (real_matrix_device): WAV RTF 0.894, MP3 RTF 0.916;
  отмена после чанка → Cancelled, `.partial` + диагностика; повторный
  запуск → Completed; **WAV-паритет GUI==CLI — 374444 байта побитово**
  (= G2/G4/G5). EOS-паритет нестандартных настроек (−1/0/0 → 377324;
  none/none → 374444) — побитово.
- **CUDA FP32** (real_matrix_device + real_overwrite_wav, RTX 3060,
  I/O Binding по умолчанию): WAV RTF 0.287–0.370, Opus Completed;
  перезапись: «ФАЙЛ ОБНОВЛЁН», новый файл — «создан новый файл», отмена
  до публикации — старый файл побайтово цел, повтор после отмены —
  Completed.
- **Перезапись WAV CPU/CUDA**: существующий → атомарная замена после
  успешного синтеза, `.partial` убран; при отмене старые байты
  неизменны (побайтовое сравнение — строгое superset SHA256-проверки),
  `.partial` сохранён.
- **MP3/Opus через ffmpeg**: существующие файлы заменены корректными
  контейнерами (ID3/OggS), новые созданы; сиротских процессов ffmpeg
  после заданий нет (tasklist-счётчик до/после равен).
- **Ошибка ffmpeg при существующем результате** (cmd-заглушка, падающая
  на аргументах энкодера): Failed, старый файл побайтово цел, черновик
  для диагностики.
- **Пакетная перезапись** (2 файла: существующий + новый, overwrite=true):
  оба Completed с правильными пометками; повтор с overwrite=false — оба
  Skipped, файлы не тронуты. Регрессия G3-пакета (5 файлов) — та же.
- **Производительность CUDA P2A.1** (13 чанков/64.01 с речи, 1 воркер,
  bench_clean): 9.42–9.49 с, RTF 0.135–0.136 (при фоновой загрузке GPU
  ~20% во время замера; уровень P2A.1 8.79–8.89 с сохранён с точностью
  до среды), **WAV побитово `5CA845C0…BD83FF` = эталону P1/P2A/P2A.1**.

## Актуальные ограничения

- ORT грузится в процесс GUI один раз: смена CPU↔CUDA — перезапуск
  приложения (сообщение в UI). Скрытого fallback CPU↔CUDA нет.
- После начала необратимой публикации отмена не отзывает замену — UI
  отключает «Стоп» и показывает «Сохранение результата…»; это заданное
  поведение единой точки решения, а не гонка.
- CUDA I/O Binding: только main-граф (flow/decoder — обычный путь),
  статические формы состояний; 2 воркера дают ~4% wall, 3-й не нужен
  (shape-узлы CPU и синхронизации — материал CUDA-P2B).
- Пул воркеров создаётся/освобождается на каждый файл пакета
  (переиспользование не реализовано — `docs/G3_BATCH.md`).
- Денойз DPDFNet не прогонялся с реальной моделью; decode_steps>1 не мерили.
- WAV > ~4 ГиБ — явная ошибка (RF64/разбиение — будущая функция).
- RTF GUI включает загрузку моделей (короткие тексты: RTF>1 — норма).
- Портативный комплект проверен на машине разработки; чистая Windows VM
  без Python/Torch/ORT/FFmpeg не проверялась. Модели/голос без
  подтверждённой лицензии на редистрибуцию (личное использование).
- Ручной интерактивный клик-проход окна не выполнялся (автоматизированные
  проверки состояний/событий + запуск exe есть).

## Сборка / запуск / проверки (Windows, Git Bash)

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
cargo test && cargo clippy --all-targets   # норма: 0 failed / 0 warning
# реальные проверки GUI (по ОДНОМУ устройству на процесс):
cargo test -p shtorm-gui --release --lib real_matrix_device -- --ignored --nocapture
SHTORM_GUI_TEST_DEVICE=cuda cargo test -p shtorm-gui --release --lib \
  real_matrix_device -- --ignored --nocapture
cargo test -p shtorm-gui --release --lib real_overwrite_wav -- --ignored --nocapture
cargo test -p shtorm-gui --release --lib real_overwrite_mp3_opus -- --ignored --nocapture
cargo test -p shtorm-gui --release --lib real_ffmpeg_error_existing_result -- --ignored --nocapture
cargo test -p shtorm-gui --release --lib real_batch_overwrite_mixed -- --ignored --nocapture
cargo test -p shtorm-gui --release --lib real_batch_cpu -- --ignored --nocapture
cargo test -p shtorm-gui --release --lib real_eos_parity -- --ignored --nocapture
# CUDA-бенчмарк P2A.1 (13 чанков, binding):
bash out/p2a1/bench_clean.sh auto 1 <tag>
```
Настройки real-тестов: ru-v2-slow, voice
`C:\PocketTTS-GUI\examples\Microsoft Brian Speed 1.wav`, ffmpeg
`C:\PocketTTS-GUI\ffmpeg\ffmpeg.exe`, seed 42, threads 4 (CPU 2w / CUDA 1w).

## NEXT TASK (НЕ НАЧИНАТЬ без явного задания)

Кандидаты следующего этапа (G5.1 завершён; CUDA-P2B не начинать без
отдельного задания):

1. **CUDA-P2B** — shape-узлы/межEP-синхронизации (222 CPU-узла/кадр),
   масштабирование воркеров, flow/decoder I/O Binding — обоснование и
   приоритеты в `docs/CUDA_PROFILE_P2A.md` §9.
2. Интерактивный клик-проход GUI (кнопки/прогресс/фазы глазами).
3. Переиспользование пула воркеров между файлами — после замеров
   старта/памяти (`docs/G3_BATCH.md`).
4. Денойз DPDFNet с реальной моделью; чистая Windows VM для портативного
   комплекта.

## Подробные документы

- `docs/SAFETY_AUDIT.md` — инцидент и постоянные правила безопасности.
- `docs/GUI_PLAN.md` — план и фактическое состояние GUI.
- `docs/G3_BATCH.md` — пакетный режим, оценка переиспользования пула.
- `docs/PORTABLE_BUILD.md` — портативные комплекты (p2a1-final актуален;
  не перезаписывать без задания).
- `docs/CUDA_PROFILE_P1.md`, `docs/CUDA_PROFILE_P2A.md` — диагностика и
  оптимизация CUDA (методика замеров).
- `docs/BENCHMARKS.md`, `docs/FEATURE_PARITY.md`, `docs/ARCHITECTURE.md`,
  `docs/NEXT_STEPS.md`.
- История этапов (A1–A5, G1–G5, CUDA-P1/P2A/P2A.1) — `git log` и
  документы выше; сырые данные — `out/`.
