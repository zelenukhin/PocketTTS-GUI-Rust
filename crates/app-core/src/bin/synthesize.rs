//! synthesize — A5 CLI: текстовый файл → WAV/MP3/Opus потоком (без накопления
//! аудио в памяти). Пул воркеров A4, синк A5, отмена Ctrl+C с диагностическим
//! журналом и корректным кодом выхода (без process::exit в середине работы).

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use app_core::pipeline::{
    log_path_for, synthesize_stream, write_chunk_log, write_diag_log, ChunkLogMeta, DiagMeta,
    PipelineOptions, RunStatus, StreamError, StreamOutcome,
};
use app_core::sink::{make_sink, OutputFormat};
use shtorm_engine::{Device, EosTail};
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
    /// Пресет качества (метки как в Python GUI)
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
    /// Хвост EOS для чанков ≤4 слов: число 0..=10 или none (штатная политика
    /// движка 5/3). Значение передаётся движку напрямую, без пересчёта.
    /// Дефолт 5 — как teratts-android и GUI.
    #[arg(long, value_parser = parse_eos_tail, default_value = "5")]
    eos_tail_short: EosTail,
    /// Хвост EOS для чанков ≥5 слов: число 0..=10 или none. Дефолт 3.
    #[arg(long, value_parser = parse_eos_tail, default_value = "3")]
    eos_tail_long: EosTail,
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

/// Разбор значения хвоста EOS: `none` — отдельное состояние (штатная
/// политика движка), число 0..=10 — Fixed. none ≠ 0.
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

fn parse_format(s: &str) -> Result<OutputFormat> {
    match s.to_ascii_lowercase().as_str() {
        "wav" => Ok(OutputFormat::Wav),
        "mp3" => Ok(OutputFormat::Mp3),
        "opus" => Ok(OutputFormat::Opus),
        other => bail!("--format: wav|mp3|opus, получено {other}"),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ОШИБКА: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
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
        eos_tail_short: args.eos_tail_short,
        eos_tail_long: args.eos_tail_long,
        brightness_db: args.brightness_db,
        denoise: args.denoise,
        pause_scale: args.pause_scale,
        paragraph_pause_s: args.paragraph_pause_s,
        max_tokens: args.max_tokens,
        char_rules,
        anti_repeat: true,
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

    // Отмена: Ctrl+C ставит флаг; воркеры дорабатывают текущий чанк, sink
    // останавливается, итоговый файл не публикуется (RunStatus::Cancelled).
    // Результат установки обработчика не игнорируем: повторная установка
    // (тесты) — предупреждение, продолжаем работать через API-флаг.
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let c = Arc::clone(&cancel);
        if let Err(e) = ctrlc::set_handler(move || {
            c.store(true, Ordering::SeqCst);
        }) {
            eprintln!("ВНИМАНИЕ: обработчик Ctrl+C не установлен ({e}); отмена через API/Stop остаётся доступной");
        }
    }
    // Тестовый хук детерминированной отмены (SHTORM_TEST_CANCEL_AFTER_MS):
    // сам выставляет тот же флаг через N мс — сквозная проверка пути отмены
    // без посылки сигналов извне.
    if let Ok(ms) = std::env::var("SHTORM_TEST_CANCEL_AFTER_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            let c = Arc::clone(&cancel);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                c.store(true, Ordering::SeqCst);
            });
        }
    }

    let sink = make_sink(
        &args.out,
        24_000,
        format,
        &args.mp3_preset,
        args.ffmpeg.as_deref(),
    )?;
    let t = Instant::now();
    let result = synthesize_stream(
        &text,
        &opts,
        args.workers,
        sink,
        |done, total| println!("  [{done}/{total}] готов"),
        &cancel,
    );
    let elapsed = t.elapsed().as_secs_f32();

    let outcome = match result {
        Ok(o) => o,
        Err(e) => {
            // типизированный контекст: диагностический журнал рядом с файлом
            if let Some(se) = e.downcast_ref::<StreamError>() {
                let diag = write_diag_log(
                    &args.out,
                    "Failed",
                    &format!("{}\nпричина: ошибка выполнения задания", se.message),
                    &DiagMeta::from_error(se),
                );
                eprintln!("ДИАГНОСТИКА: {se}");
                if let Some(p) = &diag {
                    eprintln!("диагностический журнал: {}", p.display());
                }
            }
            return Err(e);
        }
    };

    if outcome.status == RunStatus::Cancelled {
        let reason = if cancel.load(Ordering::SeqCst) {
            "прервано пользователем (Ctrl+C/SIGINT)"
        } else {
            "отменено"
        };
        // метаданные — из StreamOutcome (прогресс/seed/черновик), не None
        let diag = write_diag_log(
            &args.out,
            "Cancelled",
            &format!(
                "{reason}; черновик сохранён: {}",
                outcome.partial_path.display()
            ),
            &DiagMeta::from_outcome(&outcome),
        );
        println!(
            "ОТМЕНЕНО: обработано {}/{} чанков; итоговый файл {} НЕ создан; черновик: {}",
            outcome.chunks_done,
            outcome.total_chunks,
            args.out.display(),
            outcome.partial_path.display()
        );
        if let Some(p) = &diag {
            println!("диагностический журнал: {}", p.display());
        }
        return Ok(ExitCode::from(2));
    }
    if outcome.status == RunStatus::Failed {
        bail!("синтез не завершён (Failed) — файл не создан");
    }

    print_outcome(&args.out, &outcome, elapsed);

    if args.chunk_log {
        // журнал опционален: сбой не маскирует успешное аудио (независимый статус)
        let meta = ChunkLogMeta {
            device: args.device.clone(),
            format: args.format.clone(),
            preset: args.mp3_preset.clone(),
        };
        match write_chunk_log(&chunk_log_path, &outcome, &meta) {
            Ok(()) => println!("лог чанков: {}", chunk_log_path.display()),
            Err(e) => println!(
                "⚠ АУДИО УСПЕШНО: {}\n⚠ ЖУРНАЛ ЧАНКОВ НЕ СОЗДАН: {} — {e}",
                args.out.display(),
                chunk_log_path.display()
            ),
        }
    }
    Ok(ExitCode::SUCCESS)
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn base_args() -> Vec<String> {
        std::iter::once("synthesize".to_string())
            .chain(
                [
                    "--model-dir",
                    "m",
                    "--voice",
                    "v.wav",
                    "--text-file",
                    "t.txt",
                    "--out",
                    "o.wav",
                ]
                .into_iter()
                .map(String::from),
            )
            .collect()
    }

    /// G4.1: дефолты CLI идентичны GUI — порог −4.0, хвосты Fixed(5)/Fixed(3)
    /// (значения teratts-android, без Python-пересчёта 6/4).
    #[test]
    fn eos_defaults_match_gui() {
        let a = Args::try_parse_from(base_args()).unwrap();
        assert_eq!(a.eos_threshold, -4.0);
        assert_eq!(a.eos_tail_short, EosTail::Fixed(5));
        assert_eq!(a.eos_tail_long, EosTail::Fixed(3));
    }

    /// none — отдельное строковое представление, не числовой ноль;
    /// числа 0..=10 передаются напрямую.
    #[test]
    fn eos_tail_parse_none_is_not_zero() {
        let mut args = base_args();
        args.extend([
            "--eos-tail-short".into(),
            "none".into(),
            "--eos-tail-long".into(),
            "0".into(),
        ]);
        let a = Args::try_parse_from(args).unwrap();
        assert_eq!(a.eos_tail_short, EosTail::Default);
        assert_eq!(a.eos_tail_long, EosTail::Fixed(0));
        assert_ne!(a.eos_tail_short, a.eos_tail_long);

        let mut args = base_args();
        args.extend(["--eos-tail-short".into(), "10".into()]);
        assert_eq!(
            Args::try_parse_from(args).unwrap().eos_tail_short,
            EosTail::Fixed(10)
        );

        // за пределами диапазона и мусор — ошибка разбора
        let mut args = base_args();
        args.extend(["--eos-tail-short".into(), "11".into()]);
        assert!(Args::try_parse_from(args).is_err());
        let mut args = base_args();
        args.extend(["--eos-tail-long".into(), "auto".into()]);
        assert!(Args::try_parse_from(args).is_err());
    }
}
