# CUDA_PROFILE_P2A — экспериментальный I/O Binding и GPU-резидентные состояния main

Дата: 2026-11-09. Стенд: RTX 3060 12 ГБ, драйвер 581.42, Windows (WDDM),
ONNX Runtime 1.23.2 (`deps/portable`, DLL идентичен `deps/ort-gpu-wheel`:
SHA256 `062C3FDA…`, FileVersion 1.23.20251021.3.a83fc4d), CUDA EP
`use_tf32=0`, FP32, модель ru-v2-slow. Rust `ort = 2.0.0-rc.11`
(dynamic load). Базовый HEAD до этапа: `374097b` (CUDA-P1).
Бэкап перед изменениями: `<NAS>\Temp2\ShtormTTS-Windows-
full-374097b-20261109-223351.zip`, SHA256
`593A0594B77B4033F2C03B5153D866D46D4559BF8ED84ED8F549508279A2F1D6`,
135/135 blob совпали с HEAD.

## 1. Итог

Экспериментальный путь (`SHTORM_CUDA_IO_BINDING=1`, только CUDA) реализован,
проверен и **не включён по умолчанию**. Он оставляет 18 состояний
`flow_lm_main` на GPU между кадрами: выход кадра k связывается входом кадра
k+1 через `IoBinding` (ping-pong, два набора буферов). На кадр с хоста
уходит только `sequence` (128 Б), в CPU читаются только `conditioning`
(4 КиБ) и `eos_logit` (4 Б).

Задание 13 чанков / 64.01 с речи (+6.00 с пауз), seed 42, temp 0.3,
decode_steps 1, EOS −4/5/3, FP32/TF32=0, 1/2/3 воркера, прогрев + 3 повтора:

| Путь | W | Чистый синтез, с | RTF | main, мс/кадр | Чанк, с | GPU avg/max | VRAM пик, МБ |
|---|---:|---:|---:|---:|---:|---|---:|
| old | 1 | 33.3 (33.2–33.7) | 0.476 | 36.3 | 2.56 | 37/41 | 2715 |
| old | 2 | 30.3 (30.0–30.6) | 0.433 | 54.6 | 4.53 | 39/54 | 4767 |
| old | 3 | 29.7 (29.5–29.8) | 0.424 | 75.7 | 6.58 | 38/64 | 6825 |
| **new** | **1** | **8.15 (8.11–8.15)** | **0.116** | **5.96** | **0.62** | **40/48** | **2714** |
| **new** | **2** | **7.35 (7.32–7.38)** | **0.105** | **10.60** | **1.08** | **40/50** | **4766** |
| **new** | **3** | **7.30 (7.23–7.44)** | **0.104** | **15.49** | **1.57** | **41/83¹** | **6842** |

¹ 3w new: r1 — 54.7/83, r2 — 40.9/55, r3 — 35.7/50; медиана avg 40.9, пик
по трём прогонам 83. «Чистый синтез» = полное время `synthesize_stream` минус
загрузка моделей (Σ max по типам графов из лога). В скобках — размах r1–r3.

- **Ускорение 4.1× на всех конфигурациях** (33.3→8.15 с при 1w), RTF 0.476→0.116.
  Новый 1-воркер быстрее Python/PyTorch в 3 процесса (8.7–9.35 с, RTF
  0.135–0.146 на том же стенде, CUDA_P1).
- **WAV побитово идентичен**: 31 прогон (24 матричных + 6 чистых + пилот)
  даёт SHA256 `5ca845c093ab73b5b101e2eda460da79b520f2ca818db8862878685d3fbd83ff`;
  он же равен эталону CUDA-P1 (`out/cuda_p1/rust_cuda_w1t4_r1/out.wav`).
- **VRAM не вырос**: пики совпадают с точностью до 17 МБ (см. §7).
- Потокобезопасность/детерминизм: один и тот же WAV при 1/2/3 воркерах.
- Оставшийся потолок: с 2 воркеров время почти не падает (9.1→9.7 с wall,
  7.3 с чистых) — теперь это не копии, а GPU-исполнение + shape-узлы +
  синхронизации; см. §8–9.

## 2. Фактический API I/O Binding в `ort 2.0.0-rc.11` (+ ORT 1.23.2)

Изучены исходники зафиксированной версии crate и ORT 1.23.2
(`out/p2a/ort_src/`): `src/io_binding.rs`, `src/session/mod.rs`,
`src/value/impl_tensor/*`, `session/IOBinding.cc`, `framework/utils.cc`,
`session/inference_session.cc`, `include/.../onnxruntime_c_api.h`.

- `Session::create_binding() -> Result<IoBinding>` — `CreateIoBinding`.
  Два binding'а на сессию — независимые наборы связанных входов/выходов.
- `Session::run_binding(&mut self, &IoBinding) -> Result<SessionOutputs>` —
  `RunWithBinding`; возвращает **только связанные** выходы (по имени/индексу).
  `SessionOutputs::remove` отдаёт собственный `DynValue` (Arc-хендл), поэтому
  значение можно хранить между прогонами.
- `IoBinding::bind_input(name, &Value<T>)` — `OrtApi::BindInput`. В ORT 1.23
  копирование выполняется **в момент bind**, не в Run: `IOBinding::BindInput`
  вызывает `utils::CopyOneInputAcrossDevices` → `BatchOrCopyMLValue`; при
  совпадении устройства источника и цели (`source_device == target_device`)
  буфер **алиасится без копирования**. CPU→CUDA — реальная копия
  (async на stream провайдера). Данные, изменённые после bind, не видны —
  поэтому входы перепривязываются каждый прогон.
- `IoBinding::bind_output_to_device(name, &MemoryInfo)` — выход динамической
  формы; ORT сам аллоцирует буфер на устройстве при Run и **переиспользует
  его между прогонами**. Буфер принадлежит binding'у; `GetBoundOutputValues`
  возвращает copy-constructed `OrtValue` (общие данные, refcount), которые
  вызывающий обязан освободить — Rust-обёртка делает это в `Drop`
  (проверено 50 циклами create/run/drop).
- `IoBinding::synchronize_inputs/outputs()` — `SyncProviders` →
  `provider->Sync()` (`cudaStreamSynchronize`); C API: «may be necessary for
  some providers, such as CUDA». В экспериментальном пути вызывается
  `synchronize_outputs()` после каждого Run перед чтением CPU-выходов и
  повторной привязкой состояний.
- `MemoryInfo::new(AllocationDevice::CUDA, device_id, AllocatorType::Device,
  MemoryType::Default)` — обычная память устройства (не pinned).
  `Value::memory_info()` доступен через `DynValue::downcast_ref::<DynTensor>()`;
  использован для доказательства размещения состояний.
- `Tensor::new`/CUDA-аллокатор не понадобились: состояния аллоцирует ORT через
  `bind_output_to_device`, поэтому нет ручных указателей и своего CUDA-контекста.

## 3. Устройство GPU-буферов и схема ping-pong

Модуль: `crates/shtorm-engine/src/io_states.rs` (`pub(crate) struct IoStates`).

- Пары состояние↔выход строятся **по именам**: для каждого входа `state_N`
  ищется выход `out_state_N` (`"out_" + name`); каждый `out_state_*` обязан
  иметь вход `state_*`. Имена/порядок — из фактических метаданных сессии, не из
  позиции в списке. Входы с динамической формой и выходы с чужим типом/формой —
  явная ошибка (`bail!`), без тихого отката.
- Два `IoBinding` (A/B). Прогон k: binding `cur` получает малые входы
  (`sequence`, `text_embeddings`) и состояния прошлого прогона как **входы**;
  его же `bind_output_to_device`-буферы принимают `out_state_*` прогона k.
  Затем `cur ^= 1`. Пока один набор читается, другой пишется — записи в
  читаемый буфер нет.
- `reset()` (перед чанком и перед повтором anti-repeat в `generate()`)
  очищает `states`: следующий прогон привязывает CPU-нули — **одна**
  H2D-копия 46.9 МиБ на чанк (и на повтор), не на кадр.
- `conditioning` и `eos_logit` привязаны к CPU-памяти
  (`bind_output_to_device(..., MemoryInfo CPU)`), читаются сразу и
  возвращаются в порядке графа — `generate()` не менялся по индексам
  (`out[0]` = cond, `out[1][0]` = eos).
- В `Pocket` путь включается только при `Device::Cuda` и
  `SHTORM_CUDA_IO_BINDING=1`; при флаге на CPU — явная ошибка
  «requires a CUDA device». `flow`, `decoder`, `encoder`, `text` — в исходной
  реализации (`States::run` без изменений). CPU-движок не затронут.
- Семантика генерации не менялась: те же входы, тот же порядок прогонов,
  те же seed/EOS/anti-repeat/лимиты.

## 4. Доказательство отсутствия прежнего CPU↔GPU round-trip

Прямых счётчиков `cudaMemcpy` в ORT Chrome-trace нет: IO-копии выполняются вне
графа и в профиль не попадают (`Session`/`Node` события не содержат байтов;
Nsight Systems 2023.3.3 на этой машине не запускается — «Connection to Agent
lost», требуется elevation; CUPTI недоступен). Поэтому доказательство
построено на четырёх независимых измерениях/фактах:

1. **Контролируемый A/B на синтетическом графе с теми же 18 состояниями**
   (`bigstate.onnx`: 12 × [1,1000,16,64] f32 + 6 × [1] i64; тест
   `io_states::tests::bigstate_copy_cost_measured`, CUDA, 20 прогонов после
   прогрева): старая схема (CPU-OrtValue-стейты → feed → out_state_* →
   feed, как `States::run`) — **29.2–31.5 мс/прогон** (два прогона теста);
   I/O Binding — **0.63–0.65 мс/прогон** (46–48×). Граф один и тот же,
   разница — только транспорт (93.8 МиБ round-trip на прогон).
2. **Реальный main**: `main_run` 36.3 → 5.96 мс/кадр (1w). Разница 30.3 мс/кадр
   при 93.8 МиБ/кадр соответствует ~3.1 ГБ/с эффективной скорости синхронных
   копий на этом стенде — ровно то, что убрано. Новое время совпадает с
   оценкой «чистого GPU-исполнения» (P1: 13.9 мс на кадр — см. §10 о
   методике; 5.96 мс — wall без профилировщика, поэтому меньше).
3. **Размещение**: после первого прогона `state[0]` имеет
   `memory_info().allocation_device() == CUDA`; суммарный размер состояний
   46.9 МиБ в одну сторону (`log`: «CUDA-P2A: io binding active; state[0] on
   Cuda, states 46.9 MiB one-way, plain outputs [conditioning, eos_logit] on
   CPU»). Тест `stateful_pingpong_matches_plain_and_reset` (CUDA) проверяет
   это явно.
4. **Структура исполнения не изменилась**: Chrome-trace old/new
   (`out/p2a/traces_old|new`) содержит одинаковые 55 993 события (148 Session,
   55 845 Node; 39 639 CUDA-узлов и 16 206 CPU-узлов на фразу) — те же ядра,
   тот же порядок; WAV побитово одинаков.

Оценка трафика за задание: old — 861 прогон × 93.75 МиБ ≈ **78.8 ГиБ**;
new — 13 × 46.875 МиБ (нули на сброс) + 861 × (128 Б H2D + 4.1 КиБ D2H) ≈
**0.61 ГиБ**, т.е. примерно в 130 раз меньше. Пик VRAM при этом не вырос
(§7): освободившийся staging-объём ORT компенсирует ping-pong буферы.

## 5. Замеры: методика и сырые данные

- Задание и настройки — как CUDA-P1: `out/cuda_p1/test_text.txt`, 13 чанков,
  речь 64.01 с, voice `Василий Леванов F5-TTS3.wav`, seed 42, temp 0.3,
  decode_steps 1, EOS −4/5/3, char_rules из runtime.json, FP32/TF32=0, WAV.
- Команда: `out/p2a/bench.sh <old|new> <w> <tag>` → `out/p2a/runs/<...>/`
  (`stdout.log`, `window.txt`, `gpu.csv`, `out_log.txt`, `out.wav`);
  `SHTORM_PHASE_TIMING=1` для фаз. Монитор GPU/CPU — `out/p2a/monitor.ps1`
  (nvidia-smi + LoadPercentage, поллинг 200 мс; окно «синтез» — от первого
  сэмпла с VRAM ≥ baseline+800 МБ). Сводка: `out/p2a/analyze.py` →
  `out/p2a/summary_p2a.json`.
- Прогрев: 1 прогон каждой конфигурации, затем 3 измеряемых. Дополнительно
  `out/p2a/bench_clean.sh` — по одному прогону без монитора (проверка, что
  монитор не искажает время: отличия ≤1%).
- «Чистый синтез» = время `synthesize_stream` минус загрузка моделей
  (Σ max по типам графов из лог-строк `load <graph>: N ms`). Для new 1w
  загрузка 1.39–1.40 с, old 1w — 1.48–1.50 с; при 3w — 2.1–2.2 с.

Фазовая разбивка (1 воркер, сумма по 13 чанкам):

| Фаза | old | new |
|---|---:|---:|
| main.run на кадр (вкл. 2 префикса на чанк) | 36.3 мс | 5.96 мс |
| prefix (2 прогона × 13 чанков) | 1139 мс | 459 мс |
| flow.run на кадр | 1.58 мс | 1.37 мс |
| decoder на чанк | 79.4 мс | 75.5 мс |
| средний чанк | 2.56 с | 0.62 с |

При 3w new: main 15.49 мс/кадр, flow 4.32, decoder 129.8 мс/чанк, prefix
885 мс — рост очереди на GPU/синхронизациях, не копиях (main_ext <0.5 мс).

## 6. WAV-паритет

Побитовое совпадение на всех 31 прогонах матрицы и обеих длинах
(one-chunk smoke `ac2b77a9…`, 13-chunk `5ca845c0…`). Расхождений нет,
поэтому поиск «первого места расхождения» не потребовался. Проверено также:
повторная генерация после `reset()` (синтетический тест — три прохода
побитово равны обычному пути, включая финальные значения стейтов на CPU),
anti-repeat-повторы (в матрице не срабатывали; при retry `generate()` делает
`reset()` → те же нули, что и обычный путь).

## 7. Влияние на VRAM

Пик VRAM (nvidia-smi, суммарно по GPU, baseline ≈570 МБ):

| W | old | new | Δ |
|---:|---:|---:|---:|
| 1 | 2715 МБ | 2714 МБ | −1 |
| 2 | 4767 МБ | 4766 МБ | −1 |
| 3 | 6825 МБ | 6842 МБ | +17 |

Отдельный точечный smoke: Δ+1456 МБ (old) против Δ+1446 МБ (new) на 1 воркер.
Ping-pong добавляет 2 × 46.875 МиБ = 93.75 МиБ на воркер, но ORT перестаёт
расти под покадровый staging: суммарный пик не изменился в пределах шума
(≤17 МБ). Дефицита VRAM нет; 4-й воркер по-прежнему не нужен.

## 8. Оставшиеся CPU-fallback и синхронизации

- **Состав узлов не изменился** (trace old/new идентичны): 16 206 CPU-EP
  событий на фразу ≈ 222 узла/кадр (Unsqueeze/Gather/Cast/Concat/Slice/
  Reshape и арифметика форм). Это по-прежнему вставляет синхронизации
  CPU↔CUDA между провайдерами.
- **Синхронизации**: old — неявные синхронизации `Session::run` на каждый
  кадр (копии + fetch); new — явный `synchronize_outputs()` на кадр
  (1 вызов, `cudaStreamSynchronize`) + D2H `conditioning`/`eos_logit`.
  Количество CPU↔GPU переходов сокращено, но не устранено.
- `flow` — отдельный `session.run` на кадр (1.37 мс при 1w, 4.32 при 3w);
  `decoder` — после генерации чанка (75.5 мс/чанк при 1w, 129.8 при 3w).
- Масштабирование воркеров: new 1w→SW wall 9.8→9.4→9.8 с, чистый синтез
  8.15→7.35→7.30 с. Выигрыш от 2-го воркера всего ~10%, от 3-го — 0:
  `main` на прогон растёт 5.78→10.28→15.02 мс, GPU avg 40→41%, пик 48→83%.
  Причина — сериализация на общем CUDA-контексте/синхронизациях и CPU-узлах,
  а не PCIe-копии. Это следующий класс оптимизаций (P2B: shape-узлы,
  EP-настройки, возможно CUDA Graphs), в P2A не трогался.

## 9. Решение о целесообразности CUDA-P2B

**Целесообразен, но с другой целью.** Транспортная задача P2A закрыта:
копии устранены, паритет побитовый, VRAM не вырос, выигрыш 4.1×. Дальнейший
потенциал:

1. **P2B-1 (flow)**: перенести `cond` на GPU (сейчас 4 КиБ D2H на кадр) и
   связать `c`/`x`/`s`/`t` → `flow_dir` на устройстве; экономия мала
   (flow 1.37 мс/кадр при 1w), но убирает один D2H и один session.run-копийный
   цикл. Имеет смысл вместе с декодером.
2. **P2B-2 (shape-узлы/синхронизации)**: 222 CPU-узла/кадр и межEP-синхронизации
   — главный оставшийся источник простоя GPU (avg 40%, пик 48–83%). Это
   переэкспорт/оптимизация графа (вне I/O Binding) либо CUDA Graphs; требует
   отдельного задания и переэкспорта не предусмотрено.
3. **P2B-3 (масштабирование воркеров)**: сейчас 3 воркера дают ~тот же wall,
   что 1 (9.4–9.8 с). Проверить `intra_op` потоки, `session.use_env_allocators`,
   `arena_extend_strategy`, общий stream/`use_ep_level_unified_stream`,
   ограничение числа сессий. Ожидаемо даст больше, чем flow-binding.

Пока P2A-путь оставлен **за диагностическим флагом** и по умолчанию выключен.
P2B не начинать без отдельного задания.

## 10. Методическая поправка к CUDA-P1

P1 называл 14.6 мс/кадр из событий `*_kernel_time` «GPU-временем». Это
**не** CUDA device duration: ORT-события `*_kernel_time` — CPU-side
инструментирование вокруг запуска/ожидания ядер, и с профилировщиком они
раздуты. В P2A опираемся на:
- wall-время фаз (`SHTORM_PHASE_TIMING`) без профилировщика;
- контролируемый A/B bigstate (§4.1);
- побитовый WAV-паритет.
Точное GPU-время через Nsight/CUPTI на этой машине получить не удалось
(nsys 2023.3.3: «Connection to Agent lost», требуется elevation; запуск
из MSYS/PowerShell одинаков). Вывод: 14.6 мс не использовать как прогноз
ускорения; фактический выигрыш измерен напрямую.

## 11. Тесты и проверки

`cargo test`: 153 passed / 0 failed; `cargo clippy --all-targets`: 0
предупреждений; `cargo fmt`; release-сборка OK. CPU-движок не изменён
(флаг на CPU — явная ошибка; smoke CPU RTF 0.922, WAV `e554bcc5…`; старый
CUDA-smoke `ac2b77a9…`).

Синтетические тесты (`crates/shtorm-engine/src/io_states.rs`, `#[ignore]`,
модели в `crates/shtorm-engine/tests/models`, генератор
`tools/gen_p2a_models.py`; запуск CPU и CUDA):

```bash
# CPU
ORT_DYLIB_PATH=deps/ort-cpu/onnxruntime-win-x64-1.23.2/lib/onnxruntime.dll \
  cargo test --release -p shtorm-engine --lib io_states -- --ignored --nocapture
# CUDA (полный набор + замер копий)
PATH="deps/portable:$PATH" SHTORM_P2A_DEVICE=cuda \
  ORT_DYLIB_PATH=deps/portable/onnxruntime.dll \
  cargo test --release -p shtorm-engine --lib io_states -- --ignored --nocapture
```

Покрытие: вход/выход состояния и несколько последовательных прогонов
(`stateful_pingpong_matches_plain_and_reset` — побитовое равенство обычному
пути, финальные стейты на CPU; на CUDA — размещение CUDA); zero-size
`text_embeddings` [1,0,8] и пустая `sequence` [1,0,N] (префиксные прогоны);
`reset` ×2 без загрязнения; несовпадение имён (`badnames.onnx`),
динамическая форма состояния (`dynamic.onnx`), несовпадение типа
(`badtype.onnx`); 50 циклов создание/прогон/освобождение (double-release
`GetBoundOutputValues`); стоимость копий (`bigstate.onnx`).

Реальные проверки:
- one-chunk smoke old/new CUDA: WAV `ac2b77a9…` идентичен, `main_run`
  1802→335 мс, RTF чанка 0.613→0.237;
- матрица 13 чанков: 31 WAV побитово (`5ca845c0…`), старый == эталон P1;
- `ep_probe` old/new: идентичные наборы узлов/провайдеров, traces в
  `out/p2a/traces_old|new`;
- CPU: `Pocket` real_model `speaks`/`deterministic` — OK; smoke CPU — OK.

## 12. Файлы

- `crates/shtorm-engine/src/io_states.rs` — новый модуль (IoStates,
  сопоставление состояний, ping-pong, тесты).
- `crates/shtorm-engine/src/pocket.rs` — поле `main_io`, `main_reset`/
  `main_run` (default-путь — прежний `States::run`), инициализация по флагу.
- `crates/shtorm-engine/src/lib.rs` — `mod io_states`.
- `crates/shtorm-engine/examples/graph_meta.rs` — диагностика метаданных
  графов (входы/выходы/типы/формы).
- `crates/shtorm-engine/tests/models/*.onnx` + `tools/gen_p2a_models.py` —
  синтетические фикстуры и их генератор.
- Сырые данные: `out/p2a/` (runs/, summary_p2a.json, traces_old|new,
  graph_meta_cuda.txt, bench*.sh, analyze.py, copies.py, monitor.ps1).

## 13. CUDA-P2A.1 — стабилизация и включение по умолчанию (2026-11-09)

Базовый HEAD перед этапом — `9133450` (docs-коммит P2A). Бэкап:
`\<NAS>\backup\Temp2\ShtormTTS-Windows-full-9133450-20261109-232858.zip`,
SHA256 `8709B433800205DFF49E06E4C19A0533C0F2A1BEC667A8E837C2A40DB5A1F09A`,
144/144 blob совпали с HEAD. Python-проект и системные настройки не менялись.

### 13.1. Аудит владения и времени жизни (`ort` 2.0.0-rc.11 + ORT 1.23.2)

Изучены `src/io_binding.rs`, `src/session/mod.rs`, `src/session/output.rs`,
`src/value/mod.rs`, `src/util/map.rs` crate `ort` и `IOBinding.cc`,
`inference_session.cc`, `onnxruntime_c_api.h` ORT 1.23.2
(`out/p2a/ort_src/`). Результат:

- **Владение сессией**: `IoBinding` хранит `_session: Arc<SharedSessionInner>`;
  `SharedSessionInner::drop` вызывает `ReleaseSession`. Даже если
  `Pocket::main` (поле `Session`) уничтожается раньше `main_io` (порядок
  объявления полей), ORT-сессия живёт до освобождения обоих binding'ов.
  Порядок полей не менялся.
- **Входы**: `bind_input` сохраняет `Arc<ValueInner>` в `held_inputs`
  (MiniMap с заменой по имени). Источники H2D-копий живы до перепривязки;
  данные копируются в момент bind (`CopyOneInputAcrossDevices`), CPU→CUDA —
  реальная копия, CUDA→CUDA — алиас без копии.
- **Выходы**: `GetBoundOutputValues` возвращает независимо выделенные
  refcounted `OrtValue`; `run_binding` оборачивает каждый в `DynValue` с
  `drop=true` и `Arc` сессии, массив освобождается аллокатором ORT.
  `SessionOutputs::remove` возвращает `DynValue::clone_of` (Arc-клон), так
  что хранение состояний между прогонами корректно; двойного освобождения
  нет (нет ручных указателей, в `IoStates` нет `unsafe`).
- **Синхронизация**: после каждого `RunWithBinding` вызывается
  `synchronize_outputs()` (`SyncProviders` → `cudaStreamSynchronize`) до
  чтения CPU-выходов и перепривязки состояний. Асинхронные H2D-копии
  входов упорядочены тем же stream'ом; источники удерживаются binding'ом.
- **Ping-pong**: run k пишет в буферы binding k%2, читает состояния
  прошлого прогона из binding (k−1)%2 — записи в читаемый буфер нет.
- **Ошибки**: `IoStates::run` при любой ошибке (run_binding, sync,
  извлечение/проверка типа-формы) очищает `states`; следующий прогон
  начинается с нулей. Привязки входа переписываются каждым прогоном, поэтому
  «грязный» binding не используется. Отмена задания завершает текущий чанк,
  затем уничтожает воркеров и `Pocket`; ORT при разрушении CUDA EP
  дожидается stream'а.
- Вывод: use-after-free/double-free не обнаружено; изменения порядка полей
  и `unsafe` не потребовались.

### 13.2. Переключатель и включение по умолчанию

`SHTORM_CUDA_IO_BINDING` (читается при `Pocket::load`):

| Значение | CPU | CUDA |
|---|---|---|
| не задана (`Auto`) | прежний путь (не изменён) | **I/O Binding (по умолчанию)** |
| `1` / `on` / `true` | явная ошибка «requires a CUDA device» | I/O Binding |
| `0` / `off` / `false` | прежний путь | принудительно старый путь (логируется) |
| иное | ошибка разбора (опечатка не меняет путь) | ошибка разбора |

Скрытого fallback на CPU при ошибках GPU нет: ошибки инициализации/форм —
явные. GUI-автоматика CUDA по-прежнему 1 воркер (`effective_workers`).

### 13.3. CUDA-тесты жизненного цикла (новые, `#[ignore]`)

`cargo test --release -p shtorm-engine --lib <имя> -- --ignored --nocapture`
с `SHTORM_TEST_CUDA=1`, `SHTORM_DIR=…ru-v2-slow`, `PATH`/`ORT_DYLIB_PATH` из
`deps/portable`:

- `cuda_io_binding_parity_across_chunks_and_retry` — три последовательных
  чанка одним `Pocket`: старый и новый пути побитово равны (3.58 с legacy
  против 1.02 с binding); anti-repeat retry при `eos_threshold=100`
  (EOS не срабатывает, обе попытки выполняются): 232 320 сэмплов у обоих
  путей, без повтора — 163 200 сэмплов (вторая попытка действительно была).
- `cuda_repeated_load_say_drop_releases_vram` — 4 цикла
  load→say→drop: пик 2552–2571 МБ, после drop 646–652 МБ (base 539 МБ),
  роста от цикла к циклу нет.
- `cuda_two_concurrent_pockets_match_single` — два `Pocket` в двух потоках
  дают побитово те же результаты, что один последовательный.

### 13.4. Реальные регрессии

- **13 чанков / 64.01 с речи**, seed 42, temp 0.3, steps 1, EOS −4/5/3,
  FP32/TF32=0, ru-v2-slow (`out/p2a1/runs/`):

  | Режим | W | Wall, с | Генерация, с | RTF | main, мс/кадр | GPU avg/max | VRAM пик, МБ |
  |---|---:|---:|---:|---:|---:|---|---:|
  | по умолчанию (binding) | 1 | 9.32 | 9.05 | 0.129 | 5.65 | 37.7/48 | 2688 |
  | по умолчанию (binding) | 2 | 8.91 | 8.62 | 0.123 | 10.04 | 41.8/48 | 4738 |
  | legacy (`=0`) | 1 | 33.31 | 33.06 | 0.472 | 34.5 | 37.2/46 | 2689 |

  Чистые прогоны без монитора: binding 1w 8.79–8.89 с (RTF 0.126–0.127),
  2w 8.53–8.56 с (0.122); legacy 1w 32.35 с (0.462). Показатели соответствуют
  P2A (8.15 с / RTF 0.116 у чистого синтеза; разница — включённая в CLI
  сводку загрузка моделей и системный шум), регрессии нет.
- **WAV SHA256**: все 8 прогонов матрицы (clean/mon × auto/legacy × 1/2w) —
  `5ca845c093ab73b5b101e2eda460da79b520f2ca818db8862878685d3fbd83ff`,
  побитово равен эталону CUDA-P1/P2A.
- **VRAM после завершения**: 539 МБ (baseline стенда) после всех прогонов.
- **fast/slow smoke** (короткий текст, CUDA): `ru-v2-fast`
  `7a794f8f…` и `ru-v2-slow` `d516b3f7…` — auto == legacy побитово для
  каждой модели.
- **GUI CUDA-матрица** (`real_matrix_device`, binding по умолчанию): WAV и
  Opus Completed; отмена после первого готового чанка — Cancelled, итог не
  опубликован, `.partial` и диагностический журнал есть; повторный запуск
  после отмены — Completed. Отмена приходит, пока воркер уже генерирует
  следующий чанк (пул не прерывает кадр посередине).
- **CPU-матрица**: WAV/MP3 Completed, отмена/повтор, GUI↔CLI паритет
  374444 байта — поведение прежнее (CPU-путь не изменён).
- **Диагностика**: `graph_meta` с `SHTORM_GRAPH_META` создаёт файл
  эксклюзивно (`create_new`); повторный запуск с тем же путём завершается
  ошибкой «файл существует» (os error 80), существующий файл не изменён.

### 13.5. Решение

CUDA I/O Binding включён обычным путём для `Device::Cuda`; старый путь
доступен обратимым диагностическим переключателем
`SHTORM_CUDA_IO_BINDING=0`. CPU-путь не менялся. CUDA=1 остаётся
рекомендованным автоматическим режимом GUI (2 воркера дают ~4% wall при
+2 ГБ VRAM). P2B не начинался.

### 13.6. Портативный комплект P2A.1

`C:\PocketTTS-GUI-Rust\dist\ShtormTTS-Portable-2026-11-09-p2a1-final` —
54 файла, 3403.2 MiB, manifest git `107acf3`, dirty=false, exe SHA256
`156BD8D90AC8AAA92F05770446CE90A987FBEAFDF6AA53534CC69FA631279BB3`.
Тестовый комплект `…-p2a1` (git `e99c056`, без cancel-режима) и предыдущий
`…-g41-final` не изменялись. Проверено из `C:\`: portable-check; CPU WAV
(Completed, RTF 0.898, `80CB7288…`); CUDA WAV (Completed, RTF 0.414, в логе
`io binding active`, `85A26977…`); MP3/Opus (Completed); batch CUDA 2/2;
`--portable-selftest-cancel` (Cancelled 1/6 без итога, `.partial` есть,
повтор Completed 6 чанков); GUI-окно и сохранение настроек через UI
Automation; WAV-паритет комплект-GUI == CLI; DLL процесса — из комплекта
(CPU: только `onnxruntime\cpu`; CUDA: `onnxruntime\cuda` + cuBLAS/cuDNN;
MSVC++ runtime из корня). Детали — `docs/PORTABLE_BUILD.md`.
