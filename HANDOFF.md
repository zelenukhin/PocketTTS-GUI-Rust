# HANDOFF.md — оперативный снимок проекта (читать первым вместе с AGENTS.md)

- **Дата обновления**: 2026-11-10 (CUDA-P2B.1: исследование масштабирования
  независимых Rust-процессов; вывод — многопроцессный GUI не нужен, нужен
  P2B.2 по покадровому пути ORT/CUDA; G5.2 — финальная проверка GUI).
- **HEAD**: только из Git (`git log -1 --oneline`), не из этого файла.
- **Бэкап на NAS (верифицирован, сделан ПЕРЕД изменениями G5.2)**: полный
  архив HEAD `1e4603d` —
  `\\DISKSTATION\Ableton 2\Temp2\ShtormTTS-Windows-full-1e4603d-20261110-040125.zip`,
  SHA256 `1E5FDAFFA6D91559D7EE933DD5397BE2236BE29857232DC3D39BEF65542D1E0D`,
  144/144 blob совпадают. Прежние архивы не тронуты.
- **CUDA-P2B.1**: NAS на момент сессии недоступен (SMB error 1702, как
  G2.1) — бэкап HEAD `ad232ec` создать не удалось; **исходники Rust не
  менялись**, добавлены только диагностические скрипты `tools/bench-
  cuda-*.ps1` и отчёт `docs/CUDA_PROFILE_P2B1.md`.

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

## G5.1 — что сделано (этап G5.1)

- **Порядок проверки путей** (`gui/job.rs`): `ensure_output_path_safe`
  (reparse-точки, принадлежность каталогу результата, совпадение с
  текстом/голосом/моделью) строго ДО создания каталогов; недостающие
  каталоги создаёт `create_dir_all_no_reparse` (без следования по
  junction/symlink); после создания — повторная валидация. Повторная
  проверка предотвращает обнаруженные сценарии подмены через junction,
  но НЕ гарантирует абсолютной защиты от конкурентной подмены пути между
  проверкой и открытием (присущее check-then-open окно остаётся).
- **Фазы и точка решения о публикации**: `AudioSink::finish()` (закрыть
  контейнер) отделён от `FinalizedSink::publish()` (необратимый
  перенос/замена); решение — единая точка между генерацией и
  финализацией. Отмена до неё → Cancelled без публикации; после неё —
  конвейер доходит до конца, UI отключает «Стоп» и показывает «Сохранение
  результата…» (в пакете «Стоп» возвращается между файлами), статус
  честно Completed.
- **Сообщения перезаписи** (одна семантика в одиночном/пакетном режиме):
  «ФАЙЛ ОБНОВЛЁН (старый заменён)» только при реально существовавшем
  файле; «ГОТОВО (создан новый файл)»; «Пропущено» при выключенной
  перезаписи; «ОТМЕНЕНО … замена НЕ выполнена — старый файл сохранён
  побайтово»; «ОШИБКА ПУБЛИКАЦИИ» — сбой публикации, старый файл не
  изменён. `replaced_existing` отличает замену от создания.
- **Тесты**: барьерные тесты конкурентной отмены (Barrier, без sleep);
  песочницы %TEMP% — junction в родителе результата (отказ ДО создания),
  create_dir_all_no_reparse, выход внутри каталога модели.

## G5.2 — что сделано (текущий этап: финальная проверка + комплект)

- **Реальный клик-проход GUI** (впервые): ручной визуальный контроль
  пользователем в живом окне + автоматизированный UIA-проход
  (System.Windows.Automation + скриншоты). Подтверждено руками: запуск,
  кириллица; переключение тёмной/светлой темы; модель Fast/Slow; CPU/CUDA
  и «Воркеров вручную»; выбор голосового WAV через файловый диалог
  («Обзор…», rfd); форматы WAV/MP3/Opus с раздельными пресетами; флажок
  перезаписи сохраняется после перезапуска; синтез кнопкой «Запустить
  синтез» (прогресс K/N чанков, отзывчивость), «Стоп» во время генерации,
  повторный запуск после отмены до Completed (в песочнице остались
  `.partial` и диагностический журнал отмены + готовый WAV). UIA-проходом:
  отсутствие импорта Python-настроек, EOS-список без «Авто» (только числа),
  дефолты −4/5/3 в JSON, свёртки, кнопки журнала.
- **Найден и исправлен баг темы** (коммит G5.2): при `theme_dark=true`
  окно стартовало со светлым дефолтом eframe — `theme_applied`
  инициализировался равным настройке, `set_visuals` не вызывался ни разу.
  Теперь первый кадр применяет тему из настроек (замер средней яркости
  окна при старте: 32.1 против ~205 до фикса; регрессионный тест
  `first_frame_must_apply_theme`).
- **Наблюдения UI (не блокеры, без правок оформления)**: exe консольной
  подсистемы — рядом с окном открывается консоль (нужна для
  `--portable-*` диагностики в stdout); при полностью раскрытых секциях и
  окне ~807 px нижние секции уходят за край — пользователь разворачивает
  окно или сворачивает секции; JSON настроек с BOM от внешнего редактора
  не читается — честное «настройки не загружены» + дефолты (безопасно);
  UIA rect'ы элементов за клипом egui смещены относительно рендера
  (ограничение автоматизации, не приложения).
- **Финальный портативный комплект**:
  `C:\PocketTTS-GUI-Rust\dist\ShtormTTS-Portable-2026-11-10-g52-final` —
  git `44014e5` (dirty=false), 54 файла, 3403.2 MiB, exe SHA256
  `3BE40CA88AAC2F59BE25F6F2B50AB4DAFE7A670EA70765136A2505FAF99BC888`;
  CUDA I/O Binding включён по умолчанию (лог «io binding active» в
  selftest). Подробности проверок — `docs/PORTABLE_BUILD.md` (раздел G5.2).

## CUDA-P2B.1 — что сделано (текущий этап исследования)

- Отчёт: `docs/CUDA_PROFILE_P2B1.md` (таблицы A/B/C, Python, график
  `out/cuda_p2b1/p2b1_gpu_throughput.svg`). Исходники Rust/GUI/Python-
  проект/модели/комплекты не менялись; новые инструменты —
  `tools/bench-cuda-multiprocess.ps1`, `bench-cuda-gpu-monitor.ps1`,
  `bench-cuda-cpu-sampler.ps1` (raw — `out/cuda_p2b1/`).
- Стенд: 13 чанков/64.01 с речи, seed 42, temp 0.3, steps 1, EOS −4/5/3,
  FP32/TF32=0, I/O binding on; каждый процесс — свой `Pocket`/ORT CUDA EP,
  свой WAV. Прогрев + 3 повтора. Барьера нет (NAS-блокер, исходники не
  менялись): внешние окна по таймстемпам логов; старты A3 ≤64 мс,
  одновременность 98–99%.
- **A (независимые процессы × 1 воркер)**: 1→2→3 процесса — throughput
  8.34→9.76→10.05 речи-с/с синтеза (**+17%/+21%**), wall группы
  9.81→15.71→22.14 с, окно процесса 7.68→13.11→19.00 с, GPU 42→72→80%,
  VRAM 2.75→4.90→7.05 ГБ, main.run 6.0→11.2→16.5 мс/кадр.
- **B (1 процесс × 1/2/3 воркера)**: окно 7.76→6.78→6.60 с
  (**+14%/+18%**), main.run 6.0→10.7→15.6 мс/кадр, GPU 43→45→48%,
  VRAM 2.75→4.83→6.87 ГБ; третий воркер +2.7% за +2 ГБ.
- **C (равный объём)**: 3 задания последовательно 29.27 с (6.56 речи-с/с)
  vs параллельно 22.14 с (8.67) → speedup **1.32×**, не 3×.
- **Python/PyTorch** (тот же текст): 1/2/3 процесса — 17.32/11.15/9.52 с
  синтеза (1.82×); 3 независимых задания 35.43 с (7.55 речи-с/с).
  Абсолютно Rust быстрее (A3 22.14 с / 10.05), но 7.05 vs 3.23 ГБ VRAM.
- **Причина**: не один процесс ORT (3 процесса ≡ 3 воркера), не intra-op
  потоки (threads 1/2/4 ≡), не GPU/PCIe/VRAM/питание (76 Вт из 170,
  mem ctrl 28%, PCIe <0.7 ГБ/с). Покадровая латентность: ~540 CUDA-ядер
  + 222 CPU-узла + синхронизации на кадр, GPU простаивает ~57% при
  1 воркере. Отдельные процессы добавляют +58% GPU-busy на текст
  (WDDM time-slicing контекстов); воркеры одного процесса — нет.
- **WAV**: 50/50 файлов SHA256 `5ca845c0…` = эталону P1/P2A.
- **Проверки**: fmt OK, clippy 0, `cargo test` 168 passed / 0 failed /
  18 ignored, release OK.
- **Решение**: многопроцессный режим GUI **не внедрять** (3 процесса —
  2.6× VRAM и 3× работа загрузки моделей за +21%); максимум 2 воркера
  в одном процессе (+14%, 4.8 ГБ). CUDA-P2B.2 — покадровый путь
  ORT/CUDA, не параллелизм.

## Результаты последних проверок (2026-11-10, Windows, release)

- `cargo test` — **168 passed / 0 failed / 18 ignored** (engine 15+11i,
  app-core 48, gui 63+7i (+1 тест темы G5.2), audio 16, text 24 +
  2 интеграционных; ignored — реальные CUDA/ONNX-прогоны: запускать по
  командам ниже); `cargo clippy --all-targets` — 0 предупреждений; fmt;
  release OK. Real-тесты G5.2: CPU real_matrix (WAV RTF 0.912 / MP3
  0.916, паритет 374444 байта), CUDA real_matrix (RTF 0.436/0.342),
  real_overwrite_wav CPU+CUDA («ОТМЕНЕНО … старый файл сохранён
  побайтово», partial), mp3_opus, ffmpeg-ошибка (BrokenPipe → Failed,
  черновик сохранён), batch_mixed, batch_cpu, eos_parity (377324 /
  374444) — все зелёные; сиротских ffmpeg нет (счётчик 0).
- **CUDA P2A.1-бенчмарк G5.2** (13 чанков/64.01 с, 1 воркер, bench_clean,
  после прогрева, 3 прогона): 9.37 / 9.42 / 9.50 с, RTF 0.134–0.136,
  wall 9.6–9.8 с. Фон на момент замера: GPU ~17% (DWM/оверлей NVIDIA/
  медиа-плеер), CPU 21%, VRAM 605 MiB системными; монитор-прогон:
  GPU util avg 42.9% / max 49%, VRAM max 2770 MiB. **WAV SHA256
  `5CA845C0…BD83FF` = эталону P1/P2A/P2A.1 во всех прогонах** — путь
  вычислений идентичен, отличие от 8.79–8.89 с объясняется фоновой
  загрузкой стенда, не кодом. `SHTORM_CUDA_IO_BINDING=0` включает legacy
  (33.21 с, RTF 0.474, тот же SHA256).
- **Портативность g52-final** (копия `C:\ShtormG52PortTest`, запуск из
  `C:\`): `--portable-check` OK (все пути из каталога exe); CPU WAV
  selftest RTF 0.968; CUDA WAV/MP3/Opus RTF 0.387/0.404/0.404 с
  комплектным ffmpeg и «io binding active»; batch 2/2 (подкаталог
  сохранён); cancel → Cancelled на 1/6 + повтор → Completed (RTF 0.167);
  модули CUDA-прогона — все ORT/cuDNN/cublas/MSVC DLL из комплекта, из
  системы только драйвер nvcuda и API-set; GUI-окно открывается, тема
  при старте тёмная (38.9), «Лог чанков» toggle+save → JSON true/false,
  настройки после теста восстановлены из example (SHA256 равен).

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
- exe — консольной подсистемы (ради `--portable-*` диагностики): рядом с
  GUI открывается консоль; при полностью раскрытых секциях и окне ~807 px
  нижние секции уходят за край — свернуть секции или развернуть окно.
- JSON настроек с BOM (внешний редактор) не читается: честное сообщение
  и дефолты. Приложение само пишет JSON без BOM.
- README-PORTABLE.txt комплектов до g52-final включительно содержит
  формулировку «Existing audio files are never overwritten» — с G5
  неточно: перезапись возможна явным чекбоксом (атомарно, после успешного
  синтеза). Исходник `tools/portable/README-PORTABLE.txt` исправлен для
  будущих сборок; комплект g52-final задним числом не менялся.

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

CUDA-P2B.1 завершён (отчёт `docs/CUDA_PROFILE_P2B1.md`). Следующий —
**CUDA-P2B.2 — оптимизация покадрового пути ORT/CUDA**, НЕ многопроцессный
пул (независимые процессы дали лишь +21% за 2.6× VRAM и ≡ воркерам одного
процесса). Приоритеты (P2B.1 §13):

1. Убрать 222 CPU-EP узла/кадр и межEP-синхронизации main-графа
   (переэкспорт с shape-операциями на CUDA/статическими формами) —
   главный кандидат; затем сократить ~540 запусков ядер/кадр
   (фьюзинг, при возможности ORT CUDA Graph), flow/decoder I/O Binding.
2. Если нужен запас по batch — максимум 2 воркера в одном процессе
   (+14%, 4.8 ГБ VRAM), 3-й не нужен.
3. Переиспользование пула воркеров между файлами (`docs/G3_BATCH.md`),
   денойз DPDFNet с реальной моделью, чистая Windows VM для комплекта.

## Подробные документы

- `docs/SAFETY_AUDIT.md` — инцидент и постоянные правила безопасности.
- `docs/GUI_PLAN.md` — план и фактическое состояние GUI.
- `docs/G3_BATCH.md` — пакетный режим, оценка переиспользования пула.
- `docs/PORTABLE_BUILD.md` — портативные комплекты (g52-final актуален;
  прежние комплекты не перезаписывать без задания).
- `docs/CUDA_PROFILE_P1.md`, `docs/CUDA_PROFILE_P2A.md`,
  `docs/CUDA_PROFILE_P2B1.md` — диагностика и оптимизация CUDA (методика,
  замеры, выводы P2B.1).
- `docs/BENCHMARKS.md`, `docs/FEATURE_PARITY.md`, `docs/ARCHITECTURE.md`,
  `docs/NEXT_STEPS.md`.
- История этапов (A1–A5, G1–G5, CUDA-P1/P2A/P2A.1/P2B.1) — `git log` и
  документы выше; сырые данные — `out/`.
