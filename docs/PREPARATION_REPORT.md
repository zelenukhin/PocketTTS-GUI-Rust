# PREPARATION_REPORT — подготовка нативного порта Pocket-TTS GUI (Windows)

Дата: 2026-11-08. Этап: каркас движка + смоук CPU/CUDA. GUI не начинался (по плану).

## 1. Что исследовано

| Объект | Результат |
|---|---|
| Python-приложение (`C:\PocketTTS-GUI\PocketTTS-GUI\`) | Актуальные версии подтверждены: `pocket-tts_gui.py` (117 783 Б, 04.11), `synthesis_worker.py` (40 454 Б, 05.11), `runtime.json` (eos_threshold −1.0 + char_rules), `settings_pocket_tts.json` (модель по умолчанию — Shtorm_PocketTTS_RU_v2_slow, 3 инстанса, seed 42). Git HEAD `82d4543`; неполитая ветка `comma-semantics` (`terminate_bare_chunks`) учтена в FEATURE_PARITY как отдельная фаза. Приложение не изменялось. |
| teratts-android (движок-источник) | Ветка `teratts`, **commit `edb614e81186ea82a27294b34d567f1b4937bb20`** (2026-11-08) — зафиксирован; все вендоренные файлы побайтово сверены с этим SHA. Лицензия репозитория: **GPL-3.0** (см. §5). |
| Компоненты teratts-android | `rust/src/pocket.rs` — полный движок Shtorm ONNX (BPE-токенизатор на чистом Rust, stateful-графы, EOS, анти-повторы); `denoise.rs` — DPDFNet; `tera.rs` — паузы/trim/fade/`plus_to_acute`; `stretch.rs` — WSOLA-темп; `helper.rs`/`lib.rs` — JNI/Supertonic-обвязка (не вендорится). Cargo: `ort 2.0.0-rc.11` (load-dynamic), ndarray 0.17, rustfft, hound. |
| Модели Shtorm-v2 ONNX | `C:\Users\alexa\Downloads\pocket_tts_android\build\onnx\ru-v2-fast`, `ru-v2-slow` (+ ru, ru-v1, ru-xvibe). Инспекция — §3. |
| Windows-среда — §4. | |
| CUDA-путь — см. CUDA_FEASIBILITY.md. | |

## 2. Версии инструментов (фактические)

- Rust: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, default host `x86_64-pc-windows-msvc`, cargo 1.99.1.
- MSVC: Visual Studio 2022 Community (`cl` через vcvars; линковка Rust-бинарников MSVC-тулчейном подтверждена сборкой).
- Windows SDK: 10.0.22621.0.
- Git: 2.56.0.windows.1.
- CMake: **не требуется** (чистый Rust + динамическая onnxruntime.dll; sentencepiece не используется — BPE-токенизатор teratts на чистом Rust).
- NVIDIA: драйвер 581.42, RTX 3060 12 ГБ; CUDA Toolkit 12.3 (и 11.8) установлены, но для запуска не нужны — берутся DLL из torch-venv.
- ONNX Runtime: **1.23.2** (CPU: официальный zip `onnxruntime-win-x64-1.23.2.zip`; GPU: PyPI-колесо `onnxruntime_gpu-1.23.2-cp312-win_amd64.whl`, т.к. GitHub-редирект zip качался нестабильно). Крейт `ort = "=2.0.0-rc.11"` (тот же пин, что в teratts; `ORT_API_VERSION = 23` ⇒ dylib ≥ 1.23).

## 3. Инвентаризация моделей (ru-v2-slow; fast структурно идентичен)

Инспекция `tools/onnx_inspect.py` (onnx 1.23, read-only):

| Граф | Размер | Входы | Выходы | Веса | Квантование |
|---|---|---|---|---|---|
| flow_lm_main.onnx | 302.4 МБ | `sequence` f32[1,seq,32]; `text_embeddings` f32[1,text,1024]; 18 × `state_*` (f32[1,1000,16,64] ×12, i64[1] ×6 — KV-кэш 6 слоёв + счётчики позиций) | `conditioning` f32[1024]; `eos_logit` f32[1]; `out_state_*` (зеркально входам) | 54 × f32 | **нет** |
| flow_lm_flow.onnx | 39.1 МБ | `c` f32[1024], `s`/`t` f32[1,1], `x` f32[1,32] | `flow_dir` f32[1,32] | 68 × f32 | нет |
| mimi_encoder.onnx | 39.3 МБ | `audio` f32[1,1,samples] | `conditioning` f32[1,frames,1024] | 45 × f32 | нет |
| text_conditioner.onnx | 16.4 МБ | `token_ids` i64[1,text_len] | `embeddings` f32[1,text_len,1024] | 1 × f32 (таблица эмбеддингов) | нет |
| mimi_decoder.onnx | 41.4 МБ | `latent` f32[1,frames,32] + `state_*` | `audio_frame` f32[1,1,samples] + `out_state_*` | 47 × f32 | нет |
| tokenizer.model | 317 КБ | SentencePiece BPE (4000 pieces) | — | — | — |

- opset 17, ir_version 8 — поддерживаются ONNX Runtime 1.23 на CPU и CUDA без оговорок.
- **Квантованных операторов нет ни в одном графе** (DynamicQuantizeLinear/QLinear*/MatMulInteger — 0; все инициализаторы f32). Целочисленные тензоры (token_ids i64, счётчики позиций i64) — служебные, квантованием не являются. int8-файлы (`*_int8.onnx`) в проекте лежат рядом, но **загрузчиком игнорируются принудительно** (см. кард pocket.rs: FP32-only, авто-выбор int8 из teratts удалён).
- fast ≠ slow: все 4 весовых графа различаются (это разные чекпоинты, не rename).
- Совместимость: **CPU FP32 — подтверждена генерацией; CUDA FP32 — подтверждена генерацией** (см. BENCHMARKS). Остальные наборы (ru, ru-v1, ru-xvibe) не тестировались — вне текущей задачи.

## 4. Состояние Windows-окружения

Готово к работе полностью: Rust/MSVC/SDK ✓, драйвер NVIDIA ✓, onnxruntime CPU+GPU ✓, cuDNN 9 / CUDA 12 DLL ✓ (из `C:\PocketTTS-GUI\venv\...\torch\lib`: cudart64_12, cublas64_12, cudnn64_9, cufft64_11 — torch 2.9.1+cu128). Ничего системного не менялось; все зависимости — внутри `ShtormTTS-Windows\deps\` и PATH подставляется только дочернему процессу (tools\smoke-cuda.ps1).

Схема распространения «рядом с EXE»: `onnxruntime.dll` (GPU-сборка) + `onnxruntime_providers_cuda.dll` + `onnxruntime_providers_shared.dll` из колеса + `cudart64_12.dll`, `cublas64_12.dll` (+ `cublasLt64_12`), `cufft64_11.dll`, `cudnn64_9.dll` (+ сопутствующие cudnn-*.dll) — положить в каталог EXE; `ORT_DYLIB_PATH` не нужен, если dll лежит рядом и загружается стандартным поиском (load-dynamic сначала ищет по ORT_DYLIB_PATH, затем стандартные пути — проверено: CPU-dll рядом с exe подхватывается и без переменной).

## 5. Лицензии

- teratts-android: **GPL-3.0** (файл LICENSE сохранён как `LICENSE-TERATTS-ANDROID`). Вендоренные `pocket.rs`, `denoise.rs`, `dsp.rs` — производные работы ⇒ весь бинарь ShtormTTS-Windows распространяется под **GPL-3.0-or-later** (workspace уже объявляет `license = "GPL-3.0-or-later"`). Для личного использования ограничений нет; при публикации исходников обязательна ссылка на teratts-android и сохранение лицензии.
- Зависимости: ort (MIT/Apache-2.0), ndarray/rustfft/hound/rand/anyhow и пр. — MIT/Apache-2.0, GPL-совместимы. ONNX Runtime — MIT. Модели Shtorm — CC BY 4.0 (по header pocket.rs; у пользователя локально, не перераспространяются проектом). DPDFNet — Apache-2.0 (Ceva).
- Python-приложение (источник логики GUI) — его лицензия не меняется, код не заимствуется напрямую в Rust (только пере-реализация функций с указанием происхождения в док-комментариях).

## 6. Созданный проект

```
ShtormTTS-Windows/
    Cargo.toml                  # workspace: 4 крейта
    LICENSE-TERATTS-ANDROID     # GPL-3.0 оригинала
    crates/
        shtorm-engine/          # движок: pocket.rs (FP32-only, Device, seed), denoise.rs, dsp.rs
        text-processing/        # to_model_stress_notation (порт Python 1:1)
        audio-processing/       # write_wav16 (hound), silence
        app-core/               # settings (совместимые имена полей GUI), diagnostics (RAM/VRAM),
                                #   bin/smoke.rs, bin/bench.rs
    tools/
        onnx_inspect.py         # инспекция графов (read-only, python-onnx)
        build.ps1, smoke-cpu.ps1, smoke-cuda.ps1
    deps/                       # onnxruntime CPU/GPU (в .gitignore)
    out/                        # результаты смоуков/бенчей
    docs/                       # этот отчёт и др.
```

Юнит-тесты: 18 passed, 0 failed (DSP, BPE-нотация ударений, настройки, vorbis-окно, resample, prepare). Реальные генерации — через ignored-тесты `shtorm-engine` (`SHTORM_DIR`, `ORT_DYLIB_PATH`) и CLI `smoke`.

## 7. Проверки совместимости (по правилу «генерация решает»)

| Проверка | Статус |
|---|---|
| CPU FP32 ru-v2-slow: генерация фразы с ударениями | ✅ passed (smoke, WAV 5.60 с, peak 0.578) |
| Клонирование голоса (локальный WAV 44.1 кГц стерео→моно 24 кГц) | ✅ passed (тот же smoke; голос «Василий Леванов F5-TTS3.wav») |
| temperature/decode_steps как параметры | ✅ приняты CLI и движком (значения по умолчанию 0.3/1; экстремумы не прогонялись — в bench-план) |
| EOS: фраза закрывается, длительность адекватна тексту | ✅ (5.6 с на ~70 символов; анти-повторы не сработали — EOS пришёл до лимита) |
| Детерминизм seed на чанк | ✅ два прогона seed 42 → **побитово идентичные WAV** |
| CUDA FP32 ru-v2-slow: генерация | ✅ passed (WAV 5.60 с; VRAM Δ +2143 МБ — вычисления реально на GPU) |
| Сохранение WAV 16-бит 24 кГц | ✅ |
| Сравнение звучания с Python-версией | ⚠️ не проводилось формально (нужен наушучный аудит); длительности совпадают с ожидаемыми, отличия движков (иной генератор шума vs torch) задокументированы как ожидаемые |

## 8. Обнаруженные особенности/риски

1. **CUDA побеждает CPU незначительно** (RTF 0.56 vs 0.84 на смоуке): покадровые вызовы + KV-стейты гоняются CPU↔GPU каждый кадр. Это ожидаемый потолок «наивного» CUDA; оптимизация (I/O Binding, резидентные стейты) — отдельный этап после измерений (см. CUDA_FEASIBILITY.md).
2. Windows WDDM: `nvidia-smi --query-compute-apps` не показывает память процесса — признак GPU-работы = дельта `--query-gpu=memory.used` (учтено в smoke).
3. GitHub releases через текущую сеть отдаёт битые zip (54 КБ вместо 250 МБ): GPU-сборка берётся из PyPI-колеса (эквивалентна).
4. `mimi_encoder` на CUDA грузится ~2.3 с (компиляция/копирование весов) против 0.25 с на CPU — разовая цена при старте воркера.
5. py-onnx в venv — только для инспекции графов; рантайм-зависимостей Python нет.

## 9. Непроверенные предположения (честный список)

- ru-v1/ru/ru-xvibe на движке не прогонялись.
- Денойз DPDFNet: код вендорен, юнит-тест окна зелёный, но сквозной прогон не делался (модель dpdfnet2_48khz_hr.onnx нужно положить в каталог модели; HF-качалка не реализована намеренно — «без обязательного обращения к HF»).
- `--decode-steps > 1` на качество/скорость не измерялись.
- Повторная генерация (анти-повтор attempt 2) в тестах не срабатывала (EOS закрывал фразу) — кодовой путь выполнен в bench-прогоне частично; проверяется стресс-тестом на 500–1000 чанков (следующий запуск).
- Memory-рост на сотнях чанков: замеряется bench (первый прогон — 100 чанков; 500–1000 — после подтверждения стабильности).

## 10. Дополнение 2026-11-08 (этап проверки): TF32, лицензия, портативность

### TF32 (подробности — CUDA_FEASIBILITY.md)

Дефолт ONNX Runtime CUDA EP = `use_tf32=1` (доказано тройной пробой на
conv-графе mimi_encoder: unset==tf32=1 побитово). Исправлено: билдер всегда
передаёт `use_tf32=0`. Gemm-графы TF32 не меняют. bench_run1 CUDA-цифры по
свёрткам были с TF32; bench_run2 — строгий FP32.

### Лицензия проекта: GPL-3.0-only (уточнено)

В teratts-android нет ни одного «or any later version»-уведомления и SPDX —
только файл LICENSE (полный текст GPL-3.0). GitHub классифицирует репозиторий
как `GPL-3.0` (= only). Производный Rust-проект корректно лицензировать как
**GPL-3.0-only** — workspace переведён с `GPL-3.0-or-later` на `GPL-3.0-only`
(все Cargo.toml).

### Портативность без venv/torch (подтверждено запуском)

Тест: PATH = `C:\Windows\System32;C:\Windows` + `deps\portable\` (иначе чистый
процесс), `ORT_DYLIB_PATH` → локальная GPU-сборка. CUDA-смоук прошёл:
генерация ОК, VRAM Δ +2153 МБ. Существующее Python-окружение не менялось.

Минимальный локальный пакет CUDA-рантайма — `deps\portable\`, 15 DLL, 2.3 ГБ:

| DLL | Размер | Источник | Лицензия |
|---|---|---|---|
| onnxruntime.dll (GPU 1.23.2) | 15.5 МБ | PyPI-колесо onnxruntime-gpu | MIT |
| onnxruntime_providers_cuda.dll | 366 МБ | там же | MIT |
| onnxruntime_providers_shared.dll | 22 КБ | там же | MIT |
| cudart64_12.dll | 0.6 МБ | torch 2.9.1+cu128 (lib) | CUDA Toolkit EULA (NVIDIA) |
| cublas64_12.dll | 114 МБ | там же | CUDA Toolkit EULA |
| cublasLt64_12.dll | 675 МБ | там же | CUDA Toolkit EULA |
| cufft64_11.dll | 276 МБ | там же | CUDA Toolkit EULA |
| cudnn64_9.dll + 7 × cudnn_*_64_9.dll | ~1 ГБ | там же | NVIDIA cuDNN Software License Agreement |

Совместимость: onnxruntime-gpu 1.23.2 требует CUDA 12.x + cuDNN 9.x —
torch-евские DLL 12.8/9.x совместимы (проверено запуском). Убирать
`cufft64_11`/`cudnn_adv64_9`/`cudnn_cnn64_9` нельзя: регистрация CUDA EP
падает **молча** с уходом на CPU (поймано VRAM-контролем смоука, кейс
воспроизведён). Для CPU-режима нужен только `onnxruntime.dll` CPU-сборки
(~15 МБ).

Замечание о «жирности»: 2.3 ГБ — потому что torch поставляет полные
универсальные сборки cuBLAS/cuDNN. Альтернатива — официальные дистрибутивы
CUDA 12.x + cuDNN 9 для Windows (те же лицензии), возможна оптимизация размера
в этапе портативной сборки; функционально уже доказано, что состав достаточен.
