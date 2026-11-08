# ARCHITECTURE — ShtormTTS-Windows (нативный движок, этап подготовки)

## Слои

```
┌────────────────────────────────────────────────────────────────┐
│ GUI (будущее, eframe/egui) — НЕ начат                          │
├────────────────────────────────────────────────────────────────┤
│ app-core                                                       │
│  • settings.rs   SynthSettings — имена полей = Python GUI       │
│  • diagnostics.rs RAM (GetProcessMemoryInfo), VRAM (nvidia-smi)│
│  • bin/smoke.rs  смоук-CLI: загрузка→голос→чанк(и)→DSP→WAV      │
│  • bin/bench.rs  бенчмарк: warmup→N чанков→RTF/RAM/VRAM-строка  │
│  (след. этап: worker-пул, файловый конвейер, ffmpeg-пайп)       │
├────────────────────────────────────────────────────────────────┤
│ shtorm-engine (ядро; производное teratts-android, GPL-3.0)     │
│  • pocket.rs  Pocket: load(Device,threads,seed) / say_chunk()   │
│              BPE (SentencePiece-прото на Rust), States (KV),    │
│              generate (EOS+flow), анти-повторы, high_shelf,     │
│              resample, read_voice; session_builder(CPU/CUDA)    │
│  • denoise.rs DPDFNet 48 кГц (STFT+ort, всегда CPU)             │
│  • dsp.rs     trim_edges, fade_out (из tera.rs, fs-параметриз.) │
├────────────────────────────────────────────────────────────────┤
│ text-processing  to_model_stress_notation («+»→U+0301, ё, NFC)  │
│ audio-processing write_wav16 (hound), silence                   │
├────────────────────────────────────────────────────────────────┤
│ ort =2.0.0-rc.11 (load-dynamic) → onnxruntime.dll 1.23.2        │
│   CPU EP / CUDA EP (+CPU fallback по узлам)                     │
└────────────────────────────────────────────────────────────────┘
```

## Ключевые решения

1. **ONNX-движок teratts-android вендорен, не переписан.** `pocket.rs` изменён точечно (перечислено в шапке файла): FP32-only (авто-int8 удалён), `Device::{Cpu,Cuda}` в билдере сессий, `seed: Option<u64>` + `say_chunk(text, voice, chunk_index)` = `seed+index` (зеркало `torch.manual_seed(seed+chunk_index)` из synthesis_worker.py), анти-повтор вынесен в флаг `anti_repeat`. Семантика генерации (формула лимита речи 11 симв/с+1с, EOS-хвост 5/3 кадра, flow-интеграция s→t, клип-гард 0.98) — без изменений.
2. **Стейтфул-модель орt-сессий**: KV-кэш трансформера и состояние декодера передаются входами/выходами сессии (`state_*`/`out_state_*`), класс `States` — как в оригинале. Один `Pocket` = 5 сессий + свои `States` (невладеющие Send — инстанс живёт в одном потоке; параллелизм = N инстансов, как N процессов в Python GUI, только дешевле).
3. **Динамический onnxruntime** (`load-dynamic`): один бинарь работает и с CPU-, и с GPU-dll (выбор переменной `ORT_DYLIB_PATH` до `ort::init()`); провайдер задаётся на сессию, а не глобально — в будущем возможны смешанные конфигурации (main на CUDA, декодер на CPU) без пересборки.
4. **Разделение крейтов** повторяет будущий GUI: движку нельзя знать про настройки/файлы; text/audio — крошечные и стабильные, GUI будет звать их напрямую.

## Поток данных (smoke = прототип будущего конвейера)

```
text («+»-нотация) → text_processing::to_model_stress_notation
  → Pocket::say_chunk(model_text, voice_wav, chunk_index)
      ├─ prepare (кавычки/заглавная/точка/паддинг; eos_extra 5/3)
      ├─ BPE encode → text_conditioner → embeddings
      ├─ voice (hound→mono 24k→mimi_encoder, кэш по mtime)
      ├─ generate: main(последовательные шаги, KV-стейты) ×cap кадров
      │    eos_logit > threshold → хвост eos_extra кадров → стоп
      │    поп.1: cap = min(речь-лимит, токен-лимит); без EOS → поп.2 cap = токен-лимит
      ├─ mimi_decoder 15-кадровыми батчами → f32 PCM 24 кГц
  → high_shelf(+N дБ > 3 кГц, клип-гард) → [denoise: 24k→48k→DPDFNet→24k]
  → fade_out → audio_processing::write_wav16
```

## Совместимость путей/Unicode

Все пути — `std::path::PathBuf`, кириллица в путях проверена (голос «Василий Леванов F5-TTS3.wav», модель в `Downloads`). Чтение текста GUI-этапа будет `String::from_utf8_lossy`-политикой Python (`errors="replace"`). Никаких обращений к HuggingFace в рантайме нет и не планируется (модели — локальные каталоги).
