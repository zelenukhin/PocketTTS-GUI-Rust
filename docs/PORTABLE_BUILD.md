# G4 — портативная сборка Windows x64

Дата: 2026-11-09. Статус: реализовано и проверено на машине разработки
(Windows 10 x64, NVIDIA RTX 3060). Проверка на чистой Windows VM без
Python/PyTorch/ORT/FFmpeg **не проводилась** — см. раздел «Результаты».

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
```

Это тот же `execute_job`/`execute_batch`, что и в GUI. Они не заменяют
интерактивную проверку окна (кнопки/Стоп/журнал), но позволяют проверить
ресурсы без Cargo.toml и без ручных кликов.

## Результаты проверки (2026-11-09)

Полный комплект: `C:\PocketTTS-GUI-Rust\dist\ShtormTTS-Portable-2026-11-09-g4c`
(54 файла, логический размер 3403.1 MiB; крупные неизменяемые файлы собраны
через `-HardLinkPayload`, поэтому фактический расход диска на этом томе
минимален — при копировании папки на другой диск файлы материализуются).
`manifest.json`: git `b41cebd`, версия 0.1.0, ORT 1.23.2, ffmpeg 8.0.1;
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
  проверяет batch и кодирование; интерактивный клик-проход — G5.

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
- `frames_after_eos`, auto-EOS, CUDA I/O Binding, переиспользование пула
  между файлами не реализованы.
