//! synthesize — A3/A4 CLI: текстовый файл → один WAV с паузами.
//! Последовательный конвейер (A3); параллельные воркеры подключает A4.
//!
//!   set ORT_DYLIB_PATH=...\onnxruntime.dll
//!   synthesize --model-dir <onnx\ru-v2-slow> --voice <v.wav> \
//!     --text-file book.txt --out out\book.wav --device cuda \
//!     --char-rules <runtime.json модели>

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::time::Instant;

use app_core::pipeline::{synthesize_parallel, PipelineOptions, PipelineOutcome};
use shtorm_engine::Device;
use text_processing::CharRules;

#[derive(Parser, Debug)]
#[command(about = "Shtorm A3: text file -> one WAV with teratts pauses (FP32, CPU/CUDA)")]
struct Args {
    #[arg(long)]
    model_dir: PathBuf,
    #[arg(long)]
    voice: PathBuf,
    #[arg(long)]
    text_file: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value = "cpu")]
    device: String,
    #[arg(long, default_value_t = 0)]
    device_id: i32,
    #[arg(long, default_value_t = 4)]
    threads: usize,
    #[arg(long, default_value_t = 0.3)]
    temperature: f32,
    #[arg(long, default_value_t = 1)]
    decode_steps: u32,
    #[arg(long, default_value_t = -4.0, allow_negative_numbers = true)]
    eos_threshold: f32,
    #[arg(long, default_value_t = 42, allow_negative_numbers = true)]
    seed: i64,
    #[arg(long, default_value_t = 8.0)]
    brightness_db: f32,
    #[arg(long, default_value_t = false)]
    denoise: bool,
    #[arg(long, default_value_t = 1.0)]
    pause_scale: f32,
    #[arg(long, default_value_t = 0.9)]
    paragraph_pause_s: f32,
    #[arg(long, default_value_t = 50)]
    max_tokens: u32,
    /// runtime.json модели (секция char_rules)
    #[arg(long)]
    char_rules: Option<PathBuf>,
    /// Писать рядом с WAV {имя}_log.txt со списком чанков и пауз
    #[arg(long, default_value_t = false)]
    chunk_log: bool,
    /// Параллельные воркеры (каждый со своим экземпляром движка); 1 — один.
    #[arg(long, default_value_t = 1)]
    workers: usize,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    if std::env::var("ORT_DYLIB_PATH").is_err() {
        bail!("ORT_DYLIB_PATH не задан (см. tools/smoke-cpu.ps1 / smoke-cuda.ps1)");
    }
    // Политика безопасности (SAFETY_AUDIT.md): существующие файлы не трогаем.
    anyhow::ensure!(
        !args.out.exists(),
        "выходной файл уже существует: {} — укажите другое имя",
        args.out.display()
    );
    let chunk_log_path = log_path_for(&args.out);
    if args.chunk_log {
        anyhow::ensure!(
            !chunk_log_path.exists(),
            "лог чанков уже существует: {}",
            chunk_log_path.display()
        );
    }

    let device = match args.device.as_str() {
        "cpu" => Device::Cpu,
        "cuda" => Device::cuda(args.device_id),
        other => bail!("--device cpu|cuda, получено {other}"),
    };

    // char_rules из runtime.json модели (секция char_rules, если есть)
    let char_rules = match &args.char_rules {
        Some(p) => {
            let raw =
                std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
            let v: serde_json::Value = serde_json::from_str(&raw)?;
            match v.get("char_rules") {
                Some(cr) => {
                    let rules: CharRules = serde_json::from_value(cr.clone())
                        .context("секция char_rules не разобрана")?;
                    Some(rules)
                }
                None => {
                    eprintln!(
                        "ВНИМАНИЕ: в {} нет секции char_rules — правила не применяются.",
                        p.display()
                    );
                    None
                }
            }
        }
        None => None,
    };

    let raw_text = std::fs::read(&args.text_file)
        .with_context(|| format!("read {}", args.text_file.display()))?;
    // как Python GUI: чтение UTF-8 с заменой некорректных байтов
    let text = String::from_utf8_lossy(&raw_text).into_owned();

    let opts = PipelineOptions {
        model_dir: args.model_dir.clone(),
        voice: args.voice.clone(),
        device,
        threads: args.threads,
        seed: if args.seed < 0 {
            None
        } else {
            Some(args.seed as u64)
        },
        temperature: args.temperature,
        decode_steps: args.decode_steps as usize,
        eos_threshold: args.eos_threshold,
        brightness_db: args.brightness_db,
        denoise: args.denoise,
        pause_scale: args.pause_scale,
        paragraph_pause_s: args.paragraph_pause_s,
        max_tokens: args.max_tokens,
        char_rules,
    };

    println!(
        "конвейер: {} | чанков-лимит {} ток. | паузы ×{:.2} | абзац {:.2} с | устройство {} | воркеров {}",
        args.text_file.display(),
        args.max_tokens,
        args.pause_scale,
        args.paragraph_pause_s,
        args.device,
        args.workers
    );
    let t = Instant::now();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let outcome = synthesize_parallel(
        &text,
        &opts,
        args.workers,
        Some(&args.out),
        |done, total| println!("  [{done}/{total}] готов"),
        &cancel,
    )?;
    let elapsed = t.elapsed().as_secs_f32();

    match outcome.status {
        app_core::pipeline::RunStatus::Cancelled => {
            // A4-фикс: неполный результат не публикуется и не называется успехом
            println!(
                "ОТМЕНЕНО: обработано {}/{} чанков; итоговый файл {} НЕ создан",
                outcome.chunks_done,
                outcome.chunks.len(),
                args.out.display()
            );
            std::process::exit(2);
        }
        app_core::pipeline::RunStatus::Failed => {
            anyhow::bail!("синтез не завершён (Failed) — файл не создан");
        }
        app_core::pipeline::RunStatus::Completed => {}
    }
    let total_s = outcome.total_samples as f32 / outcome.sample_rate as f32;
    let speech_s = outcome.speech_samples as f32 / outcome.sample_rate as f32;
    println!(
        "WAV: {} ({:.2} с: речь {:.2} с + паузы {:.2} с; чанков {}; {:.2} с генерации, RTF {:.3})",
        args.out.display(),
        total_s,
        speech_s,
        total_s - speech_s,
        outcome.chunks.len(),
        elapsed,
        elapsed / total_s.max(1e-6)
    );
    println!(
        "пул: воркеров {}, base_seed {}, буфер переупорядочивания <= {} чанков / {} КиБ PCM",
        outcome.workers,
        outcome.base_seed,
        outcome.max_buffered,
        outcome.max_buffered_bytes / 1024
    );
    println!("денойз: {}", outcome.denoise_report.summary());
    let outcome = app_core::pipeline::PipelineOutcome {
        sample_rate: outcome.sample_rate,
        total_samples: outcome.total_samples,
        speech_samples: outcome.speech_samples,
        pause_samples: outcome.pause_samples,
        chunks: outcome.chunks.clone(),
        denoise_report: outcome.denoise_report.clone(),
    };

    if args.chunk_log {
        // Независимая диагностика (A4): статус АУДИО уже успешен выше; сбой
        // журнала не отменяет его и не маскируется — отдельное сообщение.
        if let Err(log_err) = write_chunk_log(&chunk_log_path, &outcome) {
            println!(
                "⚠ АУДИО УСПЕШНО: {}\n⚠ ЖУРНАЛ ЧАНКОВ НЕ СОЗДАН: {} — {log_err}",
                args.out.display(),
                chunk_log_path.display()
            );
        } else {
            println!("лог чанков: {}", chunk_log_path.display());
        }
    }
    Ok(())
}

/// {out-stem}_log.txt рядом с WAV.
pub fn log_path_for(out: &Path) -> PathBuf {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    out.with_file_name(format!("{stem}_log.txt"))
}

fn write_chunk_log(path: &Path, outcome: &PipelineOutcome) -> Result<()> {
    use std::io::Write;
    let mut w = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("создать {}", path.display()))?;
    writeln!(w, "Лог чанков (A3/A4)")?;
    writeln!(w, "Чанков: {}", outcome.chunks.len())?;
    writeln!(w, "Денойз: {}", outcome.denoise_report.summary())?;
    for c in &outcome.chunks {
        writeln!(w, "[{}] {}", c.index + 1, c.text)?;
        let speech_s = c.speech_samples as f32 / outcome.sample_rate as f32;
        match c.pause_s {
            Some(p) => writeln!(w, "    -> пауза {p:.2} с; речь {speech_s:.2} с")?,
            None => writeln!(w, "    -> конец файла (без паузы); речь {speech_s:.2} с")?,
        }
    }
    Ok(())
}
