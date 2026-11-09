//! smoke — смоук-тест движка Shtorm на Windows (CPU или CUDA, FP32).
//!
//! Загружает модель, кодирует голосовой WAV, синтезирует фразу(ы) с per-chunk
//! seed, применяет яркость/денойз по флагам, пишет один WAV и печатает
//! диагностику: провайдеры, RTF, память процесса, VRAM.
//!
//! Пример (CPU):
//!   set ORT_DYLIB_PATH=deps\ort-cpu\onnxruntime.dll
//!   smoke --model-dir <...>\build\onnx\ru-v2-slow --voice <...>.wav \
//!         --text "Стр+елки час+ов показ+ывали п+олночь." --out out\smoke_cpu.wav
//! Пример (CUDA): см. tools/smoke-cuda.ps1 (dylib GPU + PATH с CUDA/cuDNN DLL).

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::Instant;

use shtorm_engine::{Device, EosTail, LoadOptions, Pocket, POCKET_RATE};

/// Разбор значения хвоста EOS для smoke: `none` или число 0..=10.
fn parse_eos_tail(s: &str) -> Result<EosTail, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" => Ok(EosTail::Default),
        other => {
            let n = other
                .parse::<u32>()
                .map_err(|_| format!("--eos-tail: 'none' или число 0..=10, получено '{other}'"))?;
            if n > 10 {
                Err(format!("--eos-tail: максимум 10, получено {n}"))
            } else {
                Ok(EosTail::Fixed(n))
            }
        }
    }
}

#[derive(Parser, Debug)]
#[command(about = "Shtorm PocketTTS smoke test (FP32, CPU/CUDA)")]
struct Args {
    /// Каталог модели (flow_lm_main.onnx, mimi_*.onnx, tokenizer.model, ...)
    #[arg(long)]
    model_dir: PathBuf,
    /// Голосовой WAV (клонирование)
    #[arg(long)]
    voice: PathBuf,
    /// Текст с ударениями «+» или U+0301 (уже подготовленный, как в GUI)
    #[arg(long)]
    text: String,
    /// Выходной WAV
    #[arg(long, default_value = "smoke_out.wav")]
    out: PathBuf,
    /// cpu | cuda
    #[arg(long, default_value = "cpu")]
    device: String,
    #[arg(long, default_value_t = 0)]
    device_id: i32,
    /// Intra-op потоки ONNX Runtime (CPU)
    #[arg(long, default_value_t = 4)]
    threads: usize,
    #[arg(long, default_value_t = 0.3)]
    temperature: f32,
    #[arg(long, default_value_t = 1)]
    decode_steps: u32,
    /// −1 = случайный на запуск
    #[arg(long, default_value_t = 42)]
    seed: i64,
    #[arg(long, default_value_t = -4.0, allow_negative_numbers = true)]
    eos_threshold: f32,
    /// Хвост EOS для чанков ≤4 слов: 0..=10 или none (штатная политика 5/3)
    #[arg(long, value_parser = parse_eos_tail, default_value = "none")]
    eos_tail_short: EosTail,
    /// Хвост EOS для чанков ≥5 слов: 0..=10 или none
    #[arg(long, value_parser = parse_eos_tail, default_value = "none")]
    eos_tail_long: EosTail,
    /// Яркость, дБ high-shelf > 3 кГц (0 — выкл)
    #[arg(long, default_value_t = 0.0)]
    brightness_db: f32,
    /// Денойз DPDFNet (модель dpdfnet2_48khz_hr.onnx в каталоге модели)
    #[arg(long, default_value_t = false)]
    denoise: bool,
    /// Выключить двухпопытную защиту от повторов (для сравнения)
    #[arg(long, default_value_t = false)]
    no_anti_repeat: bool,
    /// Сколько раз прогнать фразу (разные chunk_index)
    #[arg(long, default_value_t = 1)]
    repeats: u32,
    /// Пауза между повторами, с
    #[arg(long, default_value_t = 0.3)]
    pause_s: f32,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    // Политика безопасности: диагностические инструменты не перезаписывают
    // существующие файлы — только новые (docs/SAFETY_AUDIT.md).
    anyhow::ensure!(
        !args.out.exists(),
        "выходной файл уже существует: {} — удалите/переименуйте вручную или укажите другое имя (--out)",
        args.out.display()
    );
    let dylib = std::env::var("ORT_DYLIB_PATH").map_err(|_| {
        anyhow::anyhow!("ORT_DYLIB_PATH не задан: укажите путь к onnxruntime.dll (см. tools/*.ps1)")
    })?;
    log::info!("ORT_DYLIB_PATH = {dylib}");
    log::info!("model_dir = {}", args.model_dir.display());

    let device = match args.device.as_str() {
        "cpu" => Device::Cpu,
        "cuda" => Device::cuda(args.device_id),
        other => bail!("--device: только cpu|cuda, получено {other}"),
    };
    let seed = if args.seed < 0 {
        None
    } else {
        Some(args.seed as u64)
    };

    let mem0 = app_core::diagnostics::process_memory().ok();
    let vram0 = app_core::diagnostics::gpu_total_used_mb();

    // ort::init читает ORT_DYLIB_PATH; ошибка загрузки dylib — фатальна (без
    // молчаливого фолбэка).
    if !ort::init().with_name("shtorm-smoke").commit() {
        bail!("ort::init() failed: не удалось инициализировать onnxruntime ({dylib})");
    }
    log::info!("onnxruntime dylib loaded OK");

    let t_load = Instant::now();
    let mut engine = Pocket::load(
        &args.model_dir,
        &LoadOptions {
            device: device.clone(),
            threads: args.threads,
            seed,
            profile_dir: None,
        },
    )
    .with_context(|| format!("load model {}", args.model_dir.display()))?;
    let load_s = t_load.elapsed().as_secs_f32();
    println!("model load: {load_s:.2} s");

    engine.temperature = args.temperature;
    engine.eos_threshold = args.eos_threshold;
    engine.eos_tail_short = args.eos_tail_short;
    engine.eos_tail_long = args.eos_tail_long;
    engine.decode_steps = args.decode_steps as usize;
    engine.anti_repeat = !args.no_anti_repeat;
    if args.denoise {
        // A4: настройка принадлежит экземпляру движка (глобаль удалена)
        engine.denoise_on = true;
    }

    // Голос: кодирование промпта (первый say включает его в замер; прогреваем заранее)
    let voice = args.voice.to_string_lossy().to_string();
    let t_voice = Instant::now();
    {
        // Прогрев: кодирование голоса + один полный проход не делаем —
        // кодируем голос отдельным вызовом через короткий чанк? Нет: engine.voice()
        // приватен; время первого say включает encode — так и задокументируем.
    }
    let _ = t_voice;

    // Текст: «+» -> U+0301 (как GUI делает перед подачей в движок)
    let model_text = text_processing::to_model_stress_notation(&args.text);
    println!("text (model notation): {model_text}");

    let mut all = Vec::<f32>::new();
    let mut gen_secs_total = 0.0f32;
    let mut audio_secs_total = 0.0f32;
    for i in 0..args.repeats.max(1) {
        let t = Instant::now();
        let audio = engine
            .say_chunk(&model_text, &voice, i as u64)
            .with_context(|| format!("say_chunk #{}", i))?;
        let gen_s = t.elapsed().as_secs_f32();
        let audio_s = audio.len() as f32 / POCKET_RATE as f32;
        let peak = audio.iter().fold(0f32, |m, v| m.max(v.abs()));
        let rtf = if audio_s > 0.0 {
            gen_s / audio_s
        } else {
            f32::INFINITY
        };
        println!(
            "chunk {i}: {audio_s:.2} s audio in {gen_s:.2} s (RTF {rtf:.3}, {:.1}x realtime), peak {peak:.3}, samples {}, frames {}",
            if rtf > 0.0 { 1.0 / rtf } else { 0.0 },
            audio.len(),
            audio.len() / 1920 // латентных кадров по 80 мс (до trim_edges)
        );
        gen_secs_total += gen_s;
        audio_secs_total += audio_s;

        let mut piece = audio;
        if args.brightness_db > 0.05 {
            shtorm_engine::high_shelf(&mut piece, args.brightness_db, 3000.0, POCKET_RATE as f32);
        }
        if args.denoise {
            match engine.clean(&piece) {
                shtorm_engine::CleanOutcome::Applied(clean) => {
                    let back =
                        shtorm_engine::resample(&clean, shtorm_engine::denoise::RATE, POCKET_RATE);
                    piece = back[..back.len().min(piece.len())].to_vec();
                }
                other => println!("denoise: {other:?} — чанк без очистки"),
            }
        }
        shtorm_engine::dsp::fade_out(&mut piece, POCKET_RATE);
        all.extend_from_slice(&piece);
        if i + 1 < args.repeats {
            all.extend(audio_processing::silence(args.pause_s, POCKET_RATE));
        }
    }

    if device.is_cuda() {
        // Требование: наличие CUDAExecutionProvider не доказывает GPU-вычисления.
        // Успешный CUDA-статус возможен ТОЛЬКО с подтверждением (VRAM-дельта);
        // при недоступной проверке или признаках CPU-fallback — ошибка (exit != 0).
        match (vram0, app_core::diagnostics::gpu_total_used_mb()) {
            (Some(v0), Some(v1)) => {
                println!(
                    "GPU total VRAM: {v0:.0} -> {v1:.0} MB (Δ {:+.0} MB)",
                    v1 - v0
                );
                // Windows WDDM: per-process память часто недоступна; дельта totals —
                // рабочий признак, per-process только дополнительный.
                let own = app_core::diagnostics::vram_used_mb(app_core::diagnostics::pid());
                println!("this process VRAM (nvidia-smi compute-apps): {own:.0} MB");
                if (v1 - v0) < 64.0 && own < 1.0 {
                    bail!(
                        "CUDA выбран, но признаков GPU-вычислений нет (VRAM не выросла:                          {v0:.0}->{v1:.0} MB): вероятен полный CPU fallback. Проверьте dylib                          (GPU-сборка?), PATH (cudart64_12, cublas64_12, cudnn64_9) и DLL-пакет."
                    );
                }
            }
            (None, _) | (_, None) => {
                bail!(
                    "CUDA выбран, но проверка GPU недоступна (nvidia-smi не отвечает):                      успешный CUDA-статус без подтверждения не выдаётся"
                );
            }
        }
    } else if let (Some(v0), Some(v1)) = (vram0, app_core::diagnostics::gpu_total_used_mb()) {
        // CPU-режим: информация без строгих требований
        println!(
            "GPU total VRAM: {v0:.0} -> {v1:.0} MB (Δ {:+.0} MB)",
            v1 - v0
        );
    }
    if let Some(parent) = args.out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    audio_processing::write_wav16(&args.out, POCKET_RATE, &all)
        .with_context(|| format!("write {}", args.out.display()))?;
    println!(
        "WAV: {} ({:.2} s, {} chunks, суммарный RTF {:.3})",
        args.out.display(),
        all.len() as f32 / POCKET_RATE as f32,
        args.repeats,
        gen_secs_total / audio_secs_total.max(1e-6)
    );

    if let Ok(m) = app_core::diagnostics::process_memory() {
        println!(
            "process RAM: WS {:+.0} MB (peak {:.0} MB), private {:+.0} MB",
            m.working_set_mb - mem0.as_ref().map(|x| x.working_set_mb).unwrap_or(0.0),
            m.peak_working_set_mb,
            m.private_mb - mem0.as_ref().map(|x| x.private_mb).unwrap_or(0.0)
        );
    }
    Ok(())
}
