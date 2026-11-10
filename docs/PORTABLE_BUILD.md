# G4 — портативная сборка Windows x64

Дата: 2026-11-09 (G4); обновлено 2026-11-10 (актуальный комплект —
`ShtormTTS-Portable-2026-11-10-g52-final`, см. раздел «Результаты
проверки G5.2» внизу). Статус: реализовано и проверено на машине
разработки (Windows 10 x64, NVIDIA RTX 3060). Проверка на чистой Windows
VM без Python/PyTorch/ORT/FFmpeg **не проводилась** — см. раздел
«Результаты».

## Цель

`ShtormTTS-GUI` запускается из отдельной папки на Windows x64 без
установленного Python, PyTorch, Rust, системного ONNX Runtime и системного
FFmpeg. Все ресурсы (модели, ORT CPU/CUDA, ffmpeg) лежат рядом с exe.

## Структура дистрибутива

```
ShtormTTS-Portable-<date>/
  shtorm-gui.exe
  msvcp140.dll                      # MSVC++ Redistributable runtime
  msvcp140_1.dll
  vcruntime140.dll
  vcruntime140_1.dll
  shtorm_gui_settings.json          # рабочие настройки с ОТНОСИТЕЛЬНЫМИ путями
  shtorm_gui_settings.example.json  # тот же пример для восстановления
  manifest.json                     # версии, git-коммит, размеры и SHA256 файлов
  README-PORTABLE.txt
  THIRD_PARTY_NOTICES.md
  input/
    example.txt                     # короткий русский текст для проверки
  output/                           # результаты по умолчанию
  voice/
    reference.wav                   # голосовой референс (пользовательский)
    README.txt                      # если reference.wav не включён
  models/
    ru-v2-fast/                     # только FP32-файлы, без *_int8.onnx
    ru-v2-slow/
  onnxruntime/
    cpu/
      onnxruntime.dll
      onnxruntime_providers_shared.dll
    cuda/
      onnxruntime.dll
      onnxruntime_providers_cuda.dll
      onnxruntime_providers_shared.dll
      cudart64_12.dll
      cublas64_12.dll
      cublasLt64_12.dll
      cufft64_11.dll
      cudnn*64_9.dll (8 файлов cuDNN)
  ffmpeg/
    ffmpeg.exe                      # GPLv3 full build (gyan.dev)
  docs/
    PORTABLE_BUILD.md               # этот файл
  LICENSES/
    GPL-3.0.txt                     # лицензия проекта
    ONNX-Runtime-LICENSE.txt        # MIT
    ONNX-Runtime-ThirdPartyNotices.txt
    ONNX-Runtime-VERSION.txt
    FFmpeg-GPLv3.txt
    FFmpeg-NOTICE.txt
    NVIDIA-CUDA-NOTICE.txt
    MODEL-NOTICE.txt
    MSVC-RUNTIME-NOTICE.txt
    Rust-Cargo.lock.txt
```

## Сборка

```powershell
powershell -ExecutionPolicy Bypass -File tools\build-portable.ps1 `
  -Destination C:\PocketTTS-GUI-Rust\dist\ShtormTTS-Portable-2026-11-09 `
  -ModelsDir C:\Users\alexa\Downloads\pocket_tts_android\build\onnx `
  -Ffmpeg C:\PocketTTS-GUI\ffmpeg\ffmpeg.exe `
  -VoiceFile "C:\PocketTTS-GUI\examples\Microsoft Brian Speed 1.wav"
```

Параметры:

- `-Destination` — **новый** каталог; существующий каталог отвергается.
  Каталог обязан быть вне исходного репозитория (чистая проверка
  переносимости). Несколько сборок рядом не удаляются и не очищаются.
- `-ModelsDir` — каталог с `ru-v2-fast/ru-v2-slow`; без параметра
  используется `deps\models`.
- `-Ffmpeg` — путь к `ffmpeg.exe`; без параметра ищется
  `deps\ffmpeg\ffmpeg.exe` или `ffmpeg\ffmpeg.exe`.
- `-VoiceFile` — необязательный WAV; копируется как `voice/reference.wav`.
  Без него создаётся `voice/README.txt`, и голос нужно положить вручную.
- `-VcRuntimeDir` — каталог с `msvcp140.dll`, `msvcp140_1.dll`,
  `vcruntime140.dll`, `vcruntime140_1.dll`; по умолчанию
  `%WINDIR%\System32` (эти файлы входят в Microsoft Visual C++
  Redistributable и разрешены к редистрибуции).
- `-HardLinkPayload` — для локальной сборки на почти заполненном диске:
  крупные неизменяемые файлы (модели, CUDA/CPU DLL, ffmpeg) создаются как
  жёсткие ссылки на источник (тот же том) после проверки размера/SHA256.
  Каталог остаётся полноценным; при копировании на другой диск/машину
  Explorer материализует файлы. Без флага — обычное копирование.
- `-SkipModels`, `-SkipCuda`, `-SkipFfmpeg` — урезанные варианты для
  диагностики; полный комплект собирается без этих флагов.

Скрипт:

1. собирает `shtorm-gui.exe` (`cargo build --release --bin shtorm-gui`);
2. создаёт новый каталог и подкаталоги (без удалений);
3. копирует exe, CPU ORT (2 DLL), CUDA ORT (15 DLL), ffmpeg, FP32-модели,
   пример настроек, документацию и лицензии;
4. **для каждого скопированного файла** сверяет размер и SHA256;
5. проверяет обязательные файлы, отсутствие `*_int8.onnx`, размер
   `flow_lm_main.onnx`, запуск `ffmpeg.exe -version`, валидность JSON
   настроек;
6. пишет `manifest.json` с версией проекта, git-коммитом, версиями ORT,
   версией ffmpeg, перечнем файлов с размерами и SHA256.

При сбое частично созданный каталог **не удаляется** (политика проекта:
никаких рекурсивных удалений) — выберите новый путь.

## Поиск ресурсов (единый резолвер)

Приоритет (`crates/gui/src/resources.rs`):

1. **Явный путь пользователя** из настроек. Абсолютный путь используется
   как есть; относительный — от каталога `shtorm-gui.exe`, а не от текущего
   рабочего каталога/ярлыка.
2. **Портативные подкаталоги рядом с exe**: `models/`, `onnxruntime/cpu/`,
   `onnxruntime/cuda/`, `ffmpeg/`.
3. **Каталоги проекта** (`Cargo.toml` + `deps/`) — только режим разработки.
4. **Документированный fallback** — существующий dev-каталог моделей
   `C:\Users\alexa\Downloads\pocket_tts_android\build\onnx`. На чистой машине
   его нет; он не влияет на портативность. В настройках портативной сборки
   абсолютных путей профиля разработчика нет.

Если `onnx_base` задан явно и не существует, приложение сообщает ошибку и
**не** подставляет другой каталог молча. Явный `ort_*_dll` также не
заменяется fallback-ом.

### CPU и CUDA

`onnxruntime/cpu` и `onnxruntime/cuda` разрешаются независимо
(`resolve_ort_dylib_in`/`ort_candidates`). CUDA-каталог добавляется в
**PATH только собственного процесса** (и в `ORT_DYLIB_PATH`), системный PATH
не меняется.

Один процесс может инициализировать только один ORT runtime. После смены
CPU↔CUDA в одном сеансе GUI приложение выдаёт честную ошибку
«перезапустите приложение»; для смены устройства нужен новый процесс.

### FFmpeg

Явный `ffmpeg_path` → `<exe>/ffmpeg.exe` → `<exe>/ffmpeg/ffmpeg.exe` →
`<exe>/../ffmpeg/ffmpeg.exe` → PATH. `ffmpeg` запускается с `-version` для
проверки. WAV пишется напрямую, MP3/Opus — через ffmpeg.

## Настройки по умолчанию

`tools/portable/settings.json` (копируется как `shtorm_gui_settings.json`)
использует только относительные пути:

- `onnx_base`: `models`
- `ort_cpu_dll`: `onnxruntime/cpu/onnxruntime.dll`
- `ort_gpu_dll`: `onnxruntime/cuda/onnxruntime.dll`
- `ffmpeg_path`: `ffmpeg/ffmpeg.exe`
- `out_dir`: `output`
- `text_file`: `input/example.txt`
- `voice_wav`: `voice/reference.wav`

Все они разрешаются от каталога exe. Сохранение настроек не меняет пути на
абсолютные.

## Модели

Модели **FP32-only**: загрузчик всегда использует `flow_lm_main.onnx`;
`*_int8.onnx` не копируются. В пакет входят только необходимые файлы:
`flow_lm_main.onnx`, `flow_lm_flow.onnx`, `mimi_decoder.onnx`,
`mimi_encoder.onnx`, `text_conditioner.onnx`, `tokenizer.model` для fast и
slow.

Лицензия на веса в исходном каталоге не найдена. Поэтому:

- пакет предназначен для **личного/локального** использования владельцем;
- **публично распространять модели нельзя** без явного разрешения автора;
- если разрешения нет — удалите `models/` и положите свои лицензированные
  модели с той же структурой.

## FFmpeg, CUDA/cuDNN, ONNX Runtime

- **FFmpeg 8.0.1-full_build (gyan.dev)**: собрана с `--enable-gpl
  --enable-version3`, распространяется под GPLv3. В комплекте — текст GPLv3
  и notice со ссылками на исходники. Пакет не публикуется в интернет.
- **ONNX Runtime 1.23.2**: MIT; LICENSE и ThirdPartyNotices включены.
- **Microsoft Visual C++ Redistributable runtime** (`msvcp140.dll`,
  `msvcp140_1.dll`, `vcruntime140.dll`, `vcruntime140_1.dll`): разрешённые к
  редистрибуции компоненты Microsoft; включены рядом с exe, чтобы приложение
  запускалось на чистой Windows без отдельно установленного VC++
  Redistributable. Notice — `LICENSES/MSVC-RUNTIME-NOTICE.txt`.
- **CUDA/cuDNN DLL**: проприетарные редистрибутивы NVIDIA; использование и
  передача регулируются NVIDIA CUDA Toolkit EULA и cuDNN SLA. В комплекте —
  `NVIDIA-CUDA-NOTICE.txt` со ссылками. Для работы CUDA нужен совместимый
  GPU и драйвер; CPU-режим эти DLL не загружает.
- **Голосовой референс**: пример `Microsoft Brian Speed 1.wav` не имеет
  подтверждённой лицензии на публичное распространение; в пакет он
  попадает только по явному `-VoiceFile` для локального теста. Замените
  его своим WAV.

Проект в целом — GPL-3.0-only; исходники проекта являются источником
бинарника.

## Диагностические режимы exe

```text
shtorm-gui.exe --portable-check
  Печатает exe-каталог, настройки, разрешённые model/ORT/ffmpeg пути.

shtorm-gui.exe --portable-selftest [--device cpu|cuda] [--format wav|mp3|opus]
  Короткий синтез из портативного каталога; печатает STATUS/SUMMARY/OUTPUT.

shtorm-gui.exe --portable-selftest-batch [--device cpu|cuda]
  Два файла во временном уникальном каталоге, результат в новом
  подкаталоге output; печатает отчёт по файлам.

shtorm-gui.exe --portable-selftest-cancel [--device cpu|cuda]
  Многочанковое задание, отмена после первого готового чанка (Cancelled,
  итог не опубликован, черновик остаётся), затем повтор на том же пути
  (Completed). P2A.1.
```

Это тот же `execute_job`/`execute_batch`, что и в GUI. Они не заменяют
интерактивную проверку окна (кнопки/Стоп/журнал), но позволяют проверить
ресурсы без Cargo.toml и без ручных кликов.

## Результаты проверки (2026-11-09)

Полный комплект: **этот каталог** (см. `manifest.json` рядом; 54 файла,
логический размер 3403.1 MiB; тестовая сборка g4c имеет тот же exe SHA256).
Крупные неизменяемые файлы собраны через `-HardLinkPayload`,
поэтому фактический расход диска на этом томе минимален — при копировании
папки на другой диск файлы материализуются.
`manifest.json`: git `e88d51c`, версия 0.1.0, ORT 1.23.2, ffmpeg 8.0.1;
каждый файл с размером и SHA256. Exe SHA256:
`54268A8231F41DAA105EA65232E8CBABC68E76C928B372EB3A63DBD08DF6C44E`.

- `--portable-check` из `C:\` (вне репозитория, без Cargo.toml): model,
  ORT CPU DLL и ffmpeg разрешены из относительных путей комплекта — OK.
- GUI-окно открывается из папки комплекта; заголовок
  «ShtormTTS — нативный GUI», кириллица отображается. Процесс закрыт после
  теста.
- CPU FP32 WAV: Completed, 6.80 с аудио (2 чанка), RTF 0.932, WAV 326444
  байта, SHA256 `80CB7288…AD3D43B`.
- CUDA FP32 WAV (RTX 3060): Completed, 6.80 с, RTF 0.783, WAV 326444 байта,
  SHA256 `85A26977…D7C61A00`. Отличие CPU/CUDA обусловлено арифметикой
  устройства; внутри одного устройства паритет сохраняется.
- MP3 (`-V2`) и Opus (`opus 64k`): Completed через комплектный
  `ffmpeg/ffmpeg.exe`; файлы с корректными заголовками ID3/OggS.
- Batch: два файла (включая подкаталог), Completed 2/2, 12.6 с; структура
  подкаталогов сохранена, отчёт по файлам.
- **WAV-паритет GUI == CLI**: CPU — 326444 байта, одинаковый SHA256
  `80CB7288…`; CUDA — 326444 байта, одинаковый SHA256 `85A26977…`.
  CLI запускался с `ORT_DYLIB_PATH` на комплектные DLL и теми же
  параметрами (threads 4, CPU workers 2 / CUDA workers 1, seed 42,
  temperature 0.3, eos −4, brightness 8, паузы 1.0/0.9, max_tokens 40).
- **Фактически загруженные DLL** (модули процесса, не VRAM):
  - CPU: только `<dist>\onnxruntime\cpu\onnxruntime.dll`; CUDA DLL не
    загружались.
  - CUDA: `<dist>\onnxruntime\cuda\onnxruntime.dll`,
    `onnxruntime_providers_cuda.dll`, `onnxruntime_providers_shared.dll`,
    `cudart64_12`, `cublas64_12`, `cublasLt64_12`, `cufft64_11`,
    `cudnn64_9`, `cudnn_engines_precompiled64_9`,
    `cudnn_engines_runtime_compiled64_9`, `cudnn_graph64_9`,
    `cudnn_heuristic64_9`, `cudnn_ops64_9`. `cudnn_adv64_9.dll` и
    `cudnn_cnn64_9.dll` входят в стандартный набор cuDNN, но в тестовом
    прогоне не загружались (оставлены для совместимости с другими операми).
  - MSVC++ runtime (`msvcp140.dll`, `msvcp140_1.dll`, `vcruntime140.dll`,
    `vcruntime140_1.dll`) загружался из корня комплекта, а не из System32.
- **Не проверено**: чистая Windows VM без Python/PyTorch/ORT/FFmpeg.
  На машине разработки эти компоненты установлены, поэтому полная
  автономность на чистой системе не подтверждена.
- **Stop/прогресс/журналы**: отмена через `Arc<AtomicBool>` и события
  K/N/чанков проверены мок-тестами и реальным `real_matrix_device`
  (Cancelled после готового чанка, итог не публикуется, `.partial` и
  диагностический журнал остаются). Портативный selftest дополнительно
  проверяет batch и кодирование. G5: явная перезапись проверена юнит-
  тестами и реальным CPU WAV (`real_overwrite_wav`); интерактивный
  клик-проход окна остаётся отдельной проверкой.

## Ограничения

- Проверка на чистой Windows VM без Python/PyTorch/ORT/FFmpeg **не
  выполнялась**; на машине разработки эти компоненты установлены. Честно
  указать это при передаче пакета.
- Один ORT runtime на процесс; CPU↔CUDA — только с перезапуском.
- Модели и голосовой пример не имеют подтверждённой лицензии на публичную
  редистрибуцию.
- Установщик, реестр, ассоциации файлов не создаются (по заданию).
- Размер полного комплекта — единицы гигабайт (CUDA/cuDNN + две FP32-модели
  + ffmpeg).
- ~~`frames_after_eos`, auto-EOS~~ — закрыто в G4.1 иным решением: хвосты
  EOS задаются явно в GUI/CLI/JSON (None/0–10, семантика teratts-android,
  без Python-пересчёта 6/4); «Авто» порога ≡ −4.0; чтение EOS из
  `runtime.json` не реализуется намеренно. CUDA I/O Binding,
  переиспользование пула между файлами не реализованы.

## Результаты проверки G4.1 (2026-11-09)

**Финальный комплект: `ShtormTTS-Portable-2026-11-09-g41-final`** —
54 файла, 3403.1 MiB, `manifest.json` git `824f2a1`, dirty=false; exe
SHA256 `6A23DDE8CA494DB99F1EBC4A481F1FF7D9512640CB3A5B50E4BE520AD9797963`
(тот же exe, что в тестовой сборке `-g41`, на которой выполнены все пункты
ниже; финальный комплект дополнительно перепроверен: portable-check, CPU
selftest с комплектным шаблоном настроек, CUDA selftest). Штатный
`shtorm_gui_settings.json` комплекта — новый формат
(`"eos_threshold": "auto"`, `"eos_tail_short": 5`, `"eos_tail_long": 3`).
Предыдущие комплекты (g4e и более ранние) не изменялись.

G4.1 добавляет группу «EOS» (порог «Авто»≡−4.0/−6…1; хвосты None/0–10 для
чанков ≤4 и ≥5 слов) в GUI, CLI и `shtorm_gui_settings.json`
(см. GUI_PLAN.md / NEXT_STEPS.md). Проверка комплекта
`ShtormTTS-Portable-2026-11-09-g41` (тот же exe):

- `--portable-check` из корня комплекта — OK; GUI-окно запускается и живёт.
- CPU selftest с настройками СТАРОГО формата (`eos_threshold: -4.0` числом,
  без новых ключей): Completed, 6.80 с, 2 чанка — миграция на месте;
  WAV SHA256 `80CB7288…AD3D43B` — побитово равен базовым линиям G4/G2
  (регрессии нет: дефолты −4/5/3 воспроизводят прежнее поведение).
- CPU selftest с новыми ключами (`"auto"`/5/3): тот же байт-в-байт WAV
  `80CB7288…` («Авто» ≡ −4.0 подтверждён).
- CPU selftest с −1/0/0: Completed, длительность изменилась — параметры
  реально проходят из JSON комплекта в движок.
- CUDA selftest (auto/5/3): Completed, RTF 0.762 (GPU), воркер 1.
- Batch selftest CPU: 2/2 готово (подкаталог сохранён).
- Паритет комплект-GUI == CLI (dev `synthesize.exe`, те же параметры):
  CPU 6.80 с, SHA256 `80CB7288…` совпадают.
- Штатный fallback None/None и увеличенный хвост 10/10 проверены
  движковыми/конвейерными smoke в репозитории (None ≡ 5/3 побитово;
  хвост 10 против 3 на длинном чанке — ровно +7 кадров).

## Результаты проверки CUDA-P2A.1 (2026-11-09)

**Финальный комплект: `ShtormTTS-Portable-2026-11-09-p2a1-final`** —
54 файла, 3403.2 MiB, `manifest.json` git `107acf3`, dirty=false; exe
SHA256 `156BD8D90AC8AAA92F05770446CE90A987FBEAFDF6AA53534CC69FA631279BB3`.
Комплект содержит CUDA I/O Binding **по умолчанию** (P2A.1) и новый
диагностический режим `--portable-selftest-cancel`. Тестовый комплект
`ShtormTTS-Portable-2026-11-09-p2a1` (тот же код без cancel-режима, git
`e99c056`, exe `4577C9AA…AFA68333`) сохранён; предыдущий
`…-g41-final` не изменялся.

Проверки финального комплекта (запуск exe из `C:\`, рабочий каталог вне
комплекта):

- `--portable-check`: model/Slow, CPU ORT и ffmpeg разрешены из
  относительных путей комплекта — PORTABLE CHECK OK.
- CPU FP32 WAV selftest: Completed, 6.80 с, 2 чанка, RTF 0.898, воркеров 2;
  SHA256 `80CB7288…AD3D43B` — как в G4/G4.1.
- CUDA FP32 WAV selftest: Completed, 6.80 с, RTF 0.414, воркер 1; в логе
  `CUDA-P2A: io binding active; state[0] on AllocationDevice("Cuda"),
  states 46.9 MiB one-way` — пользователь комплекта действительно получает
  новый путь. SHA256 `85A26977…D7C61A00` — побитово равен G4/G4.1
  (старый и новый CUDA-пути совпадают).
- MP3 (`-V2`) и Opus (`opus 64k`) на CUDA: Completed, RTF 0.403/0.408 через
  комплектный ffmpeg.
- Batch на CUDA: 2/2 готово, 4.8 с, подкаталог сохранён; в логе два
  сообщения об активном I/O Binding (пул создаётся на каждый файл).
- **Stop/Cancel** (`--portable-selftest-cancel --device cuda`): отмена
  после первого из 6 чанков → Cancelled, итоговый файл не создан,
  `.partial` существует; повтор на том же пути → Completed (6 чанков,
  RTF 0.164). CANCEL SELFTEST OK.
- GUI-окно из `C:\`: заголовок «ShtormTTS - нативный GUI», процесс
  корректно закрывается; UI Automation видит все элементы формы.
- **Сохранение настроек** (UI Automation): тумблер «Лог чанков» →
  «Сохранить настройки» → `chunk_log: true` в JSON; обратный тумблер и
  сохранение возвращают значение; набор ключей и значения сохраняются.
  После теста `shtorm_gui_settings.json` восстановлен из шаблона
  (SHA256 `628F06EE…593CB5` = запись manifest), комплект побитово
  соответствует сборке.
- **WAV-паритет комплект-GUI == CLI** (dev `synthesize.exe`, те же
  параметры): CPU `80CB7288…`, CUDA `85A26977…` — совпадают.
- **Зависимости процесса**: CUDA-прогон загружает `onnxruntime.dll`,
  `cublasLt64_12`, `cudnn_*64_9` из `<dist>\onnxruntime\cuda\`,
  `MSVCP140*`/`VCRUNTIME140*` из корня комплекта; из System32 — только
  `nvcuda.dll` (драйвер NVIDIA) и `msvcp_win.dll` (API-set Windows).
  CPU-прогон загружает только `<dist>\onnxruntime\cpu\onnxruntime.dll`.
  Python/Torch/системный ORT/FFmpeg не используются.

## Результаты проверки G5.2 (2026-11-10)

**Финальный комплект: `ShtormTTS-Portable-2026-11-10-g52-final`** — 54
файла, 3403.2 MiB, `manifest.json` git `44014e5`, dirty=false; exe SHA256
`3BE40CA88AAC2F59BE25F6F2B50AB4DAFE7A670EA70765136A2505FAF99BC888`.
Собран `-HardLinkPayload` (крупные файлы — жёсткие ссылки; при копировании
на другой том материализуются). Комплект включает фикс применения темы на
старте (G5.2) и CUDA I/O Binding по умолчанию. Предыдущие комплекты
(g41-final, p2a1, p2a1-final) не изменялись.

Проверки на КОПИИ комплекта `C:\ShtormG52PortTest` (вне репозитория,
запуск с рабочим каталогом `C:\`):

- `--portable-check`: model/Slow, CPU ORT и ffmpeg разрешены из
  относительных путей комплекта — PORTABLE CHECK OK.
- CPU FP32 WAV selftest: Completed, 6.80 с, 2 чанка, RTF 0.968.
- CUDA FP32 WAV selftest: Completed, RTF 0.387; в логе `CUDA-P2A: io
  binding active` — комплект действительно использует новый путь.
- MP3 (`-V2`) и Opus (`opus 64k`) на CUDA: Completed, RTF 0.404,
  комплектный `ffmpeg/ffmpeg.exe`.
- Batch на CUDA: 2/2 готово (подкаталог сохранён).
- Cancel: Cancelled на 1/6 чанков (итог не создан, `.partial` сохранён),
  повтор на том же пути → Completed, RTF 0.167.
- **Зависимости процесса** (модули CUDA-прогона): `onnxruntime.dll`,
  `onnxruntime_providers_cuda/shared.dll`, `cudart64_12`, `cublas(Lt)64_12`,
  `cufft64_11`, `cudnn*64_9` — из `<dist>\onnxruntime\cuda\`;
  `MSVCP140*`/`VCRUNTIME140*` — из корня комплекта; из системы — только
  драйвер `nvcuda.dll` и API-set `msvcp_win.dll`. Python/Torch/системный
  ORT/FFmpeg не используются.
- **GUI из комплекта**: окно открывается с рабочего каталога `C:\`;
  статус «настройки загружены», «✓ параметры валидны»; тема при старте
  тёмная (средняя яркость окна 38.9 — фикс G5.2 подтверждён на комплектном
  exe); «Лог чанков» toggle + «Сохранить настройки» → JSON chunk_log
  true/false; после теста `shtorm_gui_settings.json` восстановлен из
  `shtorm_gui_settings.example.json` (SHA256 равен).
- **Чистая Windows не проверялась** (нет VM): полная автономность на
  системе без Python/Torch/ORT/FFmpeg/VC++ Redistributable не
  подтверждена — на машине разработки эти компоненты установлены.

### Известное расхождение комплекта с исходниками после сборки

`README-PORTABLE.txt` внутри g52-final содержит формулировку «Existing
audio files are never overwritten» — с G5 неточная (перезапись возможна
явным чекбоксом, атомарно и только после успешного синтеза). Исходник
`tools/portable/README.txt` исправлен в коммите документации G5.2;
комплект задним числом не менялся, exe и код в нём точны для `44014e5`.

## Результаты проверки G6.2.3 (2026-11-11)

**Финальный комплект**: `C:\PocketTTS-GUI-Rust\dist\ShtormTTS-Portable-2026-11-11-g623-final`
— git `4579ca9` (dirty=false), 54 файла, 3404.1 MiB, exe SHA256
`602F9DCB729BF51E6230296CC55E3D20F479B7852896F3965826941E9803D54B`.
Промежуточный `...-g623-autosave-no-console` (git `9a74f6c`, exe SHA256
`0BF311BC7FC6155A6E9E0353A403D1B4DCB0888F87C10FE3C7DCBA7CB19EC08F`)
собран до тестового коммита стабилизации и оставлен как есть; финальный
продуктовый код в обоих комплектах одинаков (коммит 4579ca9 меняет только
`#[cfg(test)]`-часть hybrid.rs). `g62-final` и `g622-final` не изменялись.
Структура комплекта прежняя (новых exe нет); `tools/portable/README.txt`
дополнен заметками про автосохранение и перенаправление диагностики.

Проверки (UIA-скрипт `out/g623_portable_acceptance.ps1`, запуск с CWD `C:\`,
изолированно от репозитория; скриншоты — `<kit>\shots_g623\`):

- **P1 `--portable-check`**: код 0, `PORTABLE CHECK OK`; MODEL/ORT/FFMPEG
  разрешены из каталогов комплекта (в выводе — путь `<kit>`), CWD не важен.
- **P2 автосохранение в комплекте**: клик «Тёмная тема» → закрытие окна →
  `shtorm_gui_settings.json` рядом с exe содержит `theme_dark=false`;
  повторный запуск — тема Off (UIA ToggleState); закрытие без правок не
  изменило файл ни на байт (SHA256 снапшота равен). Консольных окон за
  весь сценарий не появилось.
- **P3 реальный CUDA WAV** (`--portable-selftest --device cuda`): Completed,
  OUTPUT создан; за прогон (включая `ffmpeg -version` probe) ни одного
  console-окна.
- **P4 реальный CUDA MP3**: Completed комплектным `ffmpeg/ffmpeg.exe`;
  консольных окон у энкодера нет (CREATE_NO_WINDOW).
- **P5 реальный гибрид 2×2 из комплекта** (4 файла, batch, MaxSpeed):
  статус `ПАКЕТ: готово 4 | ошибок 0 | пропущено 0 | всего 4 | пул:
  4 воркер(ов), Pocket загружен 4 раз(а) | процессов CUDA: 2; 5.9 с`;
  4 WAV на диске; два дочерних процесса наблюдались живыми
  (Win32_Process ParentProcessId), VRAM max ≈ 6.3 ГБ (короткие файлы —
  арены BFC не успевают вырасти; сам факт четырёх Pocket — в отчёте
  пула); консольных окон нет ни у GUI, ни у детей.
- **P6 закрытие при активном 2×2 (B6)**: после запуска двух детей и
  загрузки моделей (VRAM > 4 ГБ) — `CloseMainWindow` → отмена, окно
  закрылось, exit 0; настройки автосохранены (`theme_dark=true`);
  осиротевших `shtorm-gui.exe`/`ffmpeg.exe` нет; новых console-окон нет.

Ограничение: как и ранее, проверка выполнена на машине разработки, чистая
Windows-VM не проверялась.
