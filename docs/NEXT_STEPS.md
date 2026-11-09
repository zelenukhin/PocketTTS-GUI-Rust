# NEXT_STEPS — состояние и план

Обновлено 2026-11-09 (после A4-фиксов и A5). Дубли прежних статусов A4
устранены: ниже — единый список состояния.

## Готово (закрыто)

- **A1 Чанкование** — golden 96 кейсов (ветка comma-semantics).
- **A2 char_rules** — golden 314 кейсов; хвостовая запятая сохраняется.
- **Паузы teratts** — `trailing_pause_s` (эталон Python) + `chunk_pause_s`
  с исправленным приоритетом «конец абзаца > soft_break»; golden пауз
  20 кейсов; регрессионный тест на голый конец абзаца после terminate.
- **A4 Пул воркеров** — `pool.rs`: N воркеров × собственный `Pocket`,
  ограниченные очереди + скользящее окно (backpressure), строгий порядок,
  seed = base+индекс, отмена/ошибки/паники (catch_unwind, join с проверкой,
  Ok только при полной выдаче, валидация индексов), panic-тест в дочернем
  процессе с таймаутом. Per-chunk `CleanOutcome` (Applied/Off/Missing/
  LoadFailed/RunFailed) без глобального флага; при denoise=true без модели
  каждый чанк получает Missing.
- **A5 Потоковый вывод** — `audio-processing::stream::StreamingWav`
  (PCM16, паузы блоками, RIFF-лимит с явной ошибкой, `{stem}.partial.wav` +
  `hard_link`-публикация, `create_new`) и `app-core::sink::FfmpegSink`
  (аргументы 1:1 с Python GUI: libmp3lame -q/-b, libopus -b; PCM в stdin;
  stderr-поток; BrokenPipe/ненулевой код — ошибки; `-n`; частичный
  `{stem}.partial.{ext}`; резолв ffmpeg: явный -> рядом с exe -> ../ffmpeg ->
  PATH). CLI: `--format wav|mp3|opus`, `--mp3-preset`, `--ffmpeg`, `--workers`,
  `--chunk-log` (журнал: воркер/время/речь/пауза/денойз/seed/устройство/
  статус; сбой журнала не маскирует аудио).
  Проверено: A5-WAV побитово == A4 (при совпадающих `--threads`; при
  разных threads ORT меняет порядок редукций); CPU и CUDA 1w==2w побитово;
  MP3/Opus: ffprobe 18.455/18.4615 с, полное декодирование без ошибок;
  память: 14x более длинного текста не увеличивает пик (Private 1591->1095 МБ),
  буфер переупорядочивания <= 856 КиБ.
- **A5.1 стабилизация** — MoveFileW-публикация без замены (NTFS проверено,
  exFAT — на уровне API, носителя нет), уникальные partial-имена за запуск,
  stderr-дренаж до EOF (хвост 8 КиБ, child-тест 128 КиБ с таймаутом),
  abort/Drop lifecycle, Ctrl+C (exit 2 + диагностический журнал), StreamError
  с прогрессом/seed/черновиком. WAV-паритет с A5 сохранён; CPU/CUDA ×
  wav/mp3/opus — ffprobe/декод OK. План GUI — `docs/GUI_PLAN.md`.
- Надёжность: SAFETY_AUDIT-политика (create_new, partial+hard_link→MoveFileW, без
  рекурсивных удалений), TF32 изолирован фичей `diag-tf32`, smoke exit!=0 при
  CUDA-fallback, bench-гварды, trace_dir (уникальный каталог за запуск).

## Осталось до GUI

1. decode_steps 2/4 и пограничные параметры — короткие замеры (Этап B).
2. Denoise с реальным файлом DPDFNet (положить `dpdfnet2_48khz_hr.onnx` в
   каталог модели) — сквозной прогон + замер.
3. Опционально: io-binding CUDA (решение по цифрам CUDA_FEASIBILITY).
4. Пакетный режим (обход папок, несколько файлов за задание) — перенос
   `_process_directory_recursively` из GUI.
5. Портативная сборка: exe + deps/portable (15 DLL) + ffmpeg.exe + лаунчер.

## Этап C: GUI (eframe/egui)

6. ~~G1 каркас~~ ✅ (2026-11-09, см. GUI_PLAN.md): форма/валидация/настройки/
   импорт RO/кириллица; anti_repeat реально в Pocket (A5.2+G1).
7. ~~G2~~ ✅ (2026-11-09): запуск `synthesize_stream` в фоновом потоке из GUI
   (напрямую app-core, без subprocess), прогресс done/total полосой, Стоп
   (Arc<AtomicBool>), статусы Completed/Cancelled/Failed с путями
   .partial/диагностических журналов, лог чанков, запрет двух заданий,
   закрытие окна с отменой живого задания; CPU/CUDA × WAV/MP3/Opus, отмена,
   повтор, побитовый GUI==CLI паритет подтверждены (real_matrix-тест,
   `cargo test -p shtorm-gui --release --lib real_matrix -- --ignored`).
8. **G3**: пакетный режим (walkdir), перезапуск воркеров каждые N файлов,
   авто-EOS/runtime.json-резолвер (или явное «не поддержано»), отчёт K из N.
9. Портативная сборка (exe + deps/portable + ffmpeg).

## Известные ограничения (для GUI-дизайна)

- Каждый воркер грузит модель заново (старт ~1–3 с); CUDA 2w даёт малый
  выигрыш (+VRAM 1.8 ГБ) — по умолчанию выбирать CPU 2w / CUDA 1w.
- WAV > ~4 ГиБ (≈24 ч аудио @24 kHz) не поддерживается: явная ошибка,
  нужен RF64/разбиение (будущая функция).
- Тесты синков с ffmpeg помечаются SKIP, если ffmpeg недоступен (внешний
  инструмент); смоуки используют портативный ffmpeg явно.