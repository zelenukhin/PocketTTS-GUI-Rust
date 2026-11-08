//! synthesize — A5 CLI: текстовый файл → WAV/MP3/Opus потоком (без накопления
//! аудио в памяти). Пул воркеров A4, синк A5.
//!
//!   set ORT_DYLIB_PATH=...\onnxruntime.dll
//!   synthesize --model-dir <onnx\ru-v2-slow> --voice <v.wav> \
//!     --text-file book.txt --out out\book.mp3 --format mp3 --device cuda --workers 2

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::time::Instant;

use app_core::pipeline::{synthesize_stream, PipelineOptions, RunStatus, StreamOutcome};
use app_core::sink::{make_sink, OutputFormat};
use shtorm_engine::Device;
use text_processing::CharRules;

#[derive(Parser, Debug)]
#[command(about = "Shtorm A5: text file -> streamed WAV/MP3/Opus with teratts pauses")]
struct Args {
    #[arg(long)]
    model_dir: PathBuf,
    #[arg(long)]
    voice: PathBuf,
    #[arg(long)]
    text_file: PathBuf,
    /// Итоговый файл
    #[arg(long)]
    out: PathBuf,
    /// wav | mp3 | opus
    #[arg(long, default_value = "wav")]
    format: String,
    /// Пресет качества (метки как в Python GUI; значения с «-V..»)
    #[arg(long, default_value = "-V2 (Высокое)", allow_hyphen_values = true)]
    mp3_preset: String,
    /// Явный путь к ffmpeg (иначе: рядом с exe → ../ffmpeg → PATH)
    #[arg(long)]
    ffmpeg: Option<PathBuf>,
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
    /// Писать рядом {имя}_log.txt (metadata-only журнал)
    #[arg(long, default_value_t = false)]
    chunk_log: bool,
    /// Параллельные воркеры (каждый со своим экземпляром движка)
    #[arg(long, default_value_t = 1)]
    workers: usize,
}

fn parse_format(s: &str) -> Result<OutputFormat> {
    match s.to_ascii_lowercase().as_str() {
        "wav" => Ok(OutputFormat::Wav),
        "mp3" => Ok(OutputFormat::Mp3),
        "opus" => Ok(OutputFormat::Opus),
        other => bail!("--format: wav|mp3|opus, получено {other}"),
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    if std::env::var("ORT_DYLIB_PATH").is_err() {
        bail!("ORT_DYLIB_PATH не задан (см. tools/smoke-cpu.ps1 / smoke-cuda.ps1)");
    }
    let format = parse_format(&args.format)?;
    // политика безопасности: существующие файлы не трогаем (синк тоже проверит)
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
        "поток: {} | формат {:?} ({}) | чанков-лимит {} | паузы ×{:.2} | абзац {:.2} с | {} | воркеров {}",
        args.text_file.display(),
        format,
        args.mp3_preset,
        args.max_tokens,
        args.pause_scale,
        args.paragraph_pause_s,
        args.device,
        args.workers
    );

    let sink = make_sink(
        &args.out,
        24_000,
        format,
        &args.mp3_preset,
        args.ffmpeg.as_deref(),
    )?;
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let t = Instant::now();
    let outcome = synthesize_stream(
        &text,
        &opts,
        args.workers,
        sink,
        |done, total| println!("  [{done}/{total}] готов"),
        &cancel,
    )?;
    let elapsed = t.elapsed().as_secs_f32();

    match outcome.status {
        RunStatus::Cancelled => {
            println!(
                "ОТМЕНЕНО: обработано {}/{} чанков; итоговый файл НЕ создан; черновик: {}",
                outcome.chunks_done,
                outcome.total_chunks,
                outcome.partial_path.display()
            );
            std::process::exit(2);
        }
        RunStatus::Failed => bail!("синтез не завершён (Failed) — файл не создан"),
        RunStatus::Completed => {}
    }

    print_outcome(&args.out, &outcome, elapsed);

    if args.chunk_log {
        // журнал опционален: сбой не маскирует успешное аудио (независимый статус)
        match write_chunk_log(&chunk_log_path, &outcome, &args) {
            Ok(()) => println!("лог чанков: {}", chunk_log_path.display()),
            Err(e) => println!(
                "⚠ АУДИО УСПЕШНО: {}\n⚠ ЖУРНАЛ ЧАНКОВ НЕ СОЗДАН: {} — {e}",
                args.out.display(),
                chunk_log_path.display()
            ),
        }
    }
    Ok(())
}

fn print_outcome(out: &Path, o: &StreamOutcome, elapsed: f32) {
    let total_s = (o.speech_samples + o.pause_samples) as f64 / o.sample_rate as f64;
    let speech_s = o.speech_samples as f64 / o.sample_rate as f64;
    println!(
        "файл: {} ({:.2} с: речь {:.2} с + паузы {:.2} с; чанков {}; {:.2} с генерации, RTF {:.3})",
        out.display(),
        total_s,
        speech_s,
        total_s - speech_s,
        o.chunks.len(),
        elapsed,
        elapsed as f64 / total_s.max(1e-9)
    );
    println!(
        "пул: воркеров {}, base_seed {}, буфер переупорядочивания <= {} чанков / {} КиБ PCM",
        o.workers,
        o.base_seed,
        o.max_buffered,
        o.max_buffered_bytes / 1024
    );
    println!("денойз: {}", o.denoise_report.summary());
}

/// {out-stem}_log.txt рядом с файлом.
pub fn log_path_for(out: &Path) -> PathBuf {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    out.with_file_name(format!("{stem}_log.txt"))
}

fn write_chunk_log(path: &Path, o: &StreamOutcome, args: &Args) -> Result<()> {
    use std::io::Write;
    let mut w = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("создать {}", path.display()))?;
    writeln!(w, "Лог чанков (A5)")?;
    writeln!(
        w,
        "Устройство: {}; воркеров: {}; base_seed: {}",
        args.device, o.workers, o.base_seed
    )?;
    writeln!(
        w,
        "Формат: {:?} ({}); чанков: {}",
        parse_format(&args.format)?,
        args.mp3_preset,
        o.chunks.len()
    )?;
    writeln!(w, "Статус: {:?}", o.status)?;
    writeln!(w, "Денойз: {}", o.denoise_report.summary())?;
    for c in &o.chunks {
        writeln!(w, "[{}] {}", c.index + 1, c.text)?;
        writeln!(
            w,
            "    -> воркер {}; {:.2} с генерации; речь {:.2} с; пауза {}; денойз {:?}",
            c.worker_id
                .map(|w| (w + 1).to_string())
                .unwrap_or_else(|| "?".into()),
            c.ms.unwrap_or(0) as f64 / 1000.0,
            c.speech_samples as f64 / o.sample_rate as f64,
            c.pause_s
                .map(|p| format!("{p:.2} с"))
                .unwrap_or_else(|| "нет (конец файла)".into()),
            c.denoise
        )?;
    }
    Ok(())
}
