//! bench — измерение скорости и памяти движка Shtorm (CPU/CUDA, FP32).
//!
//! Два режима:
//!  • обычный (--text): одна фраза N раз; строка для BENCHMARKS.md.
//!  • стресс (--text-file): корпус фраз из файла (по предложению в строке,
//!    длинные дополнительно режутся по ", "); фразы циклически; память
//!    (WS/Private/GPU-total) сэмплируется каждые --sample-every чанков в CSV;
//!    после прогона движок дропается и память меряется снова (проверка
//!    освобождения). Готовое аудио НЕ накапливается.
//!
//!   set ORT_DYLIB_PATH=... && bench --model-dir ... --voice ... --chunks 100 --tag cpu-fp32-slow
//!   bench ... --text-file book.txt --chunks 1000 --csv out/stress_cuda_slow.csv --sample-every 10

use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::Instant;

use shtorm_engine::{Device, LoadOptions, Pocket, POCKET_RATE};

#[derive(Parser, Debug)]
#[command(about = "Shtorm engine benchmark/stress: RTF + memory + VRAM")]
struct Args {
    #[arg(long)]
    model_dir: PathBuf,
    #[arg(long)]
    voice: PathBuf,
    /// Одна фраза (обычный режим), конфликтует с --text-file
    #[arg(
        long,
        default_value = "Стр+елки час+ов показ+ывали п+олночь, а стрелк+и заст+ыли на пост+ах."
    )]
    text: String,
    /// Файл корпуса (стресс-режим): предложения/абзацы по строкам
    #[arg(long)]
    text_file: Option<PathBuf>,
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
    #[arg(long, default_value_t = 42)]
    seed: i64,
    /// Прогревочных чанков (не измеряются; память после них = точка отсчёта тренда)
    #[arg(long, default_value_t = 5)]
    warmup: u32,
    /// Измеряемых чанков
    #[arg(long, default_value_t = 200)]
    chunks: u32,
    /// Метка строки в отчёте
    #[arg(long, default_value = "run")]
    tag: String,
    /// CSV-файл сэмплинга памяти (стресс-режим)
    #[arg(long)]
    csv: Option<PathBuf>,
    /// Сэмплировать память каждые N чанков
    #[arg(long, default_value_t = 10)]
    sample_every: u32,
}

fn p95(mut xs: Vec<f32>) -> f32 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[(((xs.len() as f32) * 0.95) as usize).min(xs.len() - 1)]
}

/// Наклон МНК (МБ/чанк) по точкам (i, mb).
fn slope(pts: &[(f64, f64)]) -> f64 {
    let n = pts.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let (sx, sy) = pts.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1));
    let (sxx, sxy) = pts
        .iter()
        .fold((0.0, 0.0), |a, p| (a.0 + p.0 * p.0, a.1 + p.0 * p.1));
    let denom = n * sxx - sx * sx;
    if denom.abs() < 1e-12 {
        0.0
    } else {
        (n * sxy - sx * sy) / denom
    }
}

struct Mem {
    ws: f64,
    priv_mb: f64,
    gpu: f64,
}
fn sample_mem() -> Mem {
    let m = app_core::diagnostics::process_memory().ok();
    Mem {
        ws: m.as_ref().map(|x| x.working_set_mb).unwrap_or(0.0),
        priv_mb: m.as_ref().map(|x| x.private_mb).unwrap_or(0.0),
        gpu: app_core::diagnostics::gpu_total_used_mb().unwrap_or(0.0),
    }
}

/// Корпус: строки файла → предложения; длинные режем по ", "; всё в «+»-нотации
/// конвертируется в модельную (U+0301).
fn load_corpus(path: &PathBuf) -> Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        for sent in split_sentences(line) {
            let s = sent.trim().to_string();
            if s.chars().any(|c| c.is_alphabetic()) {
                out.push(text_processing::to_model_stress_notation(&s));
            }
        }
    }
    anyhow::ensure!(!out.is_empty(), "корпус пуст: {}", path.display());
    Ok(out)
}

fn split_sentences(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in line.chars() {
        cur.push(ch);
        if matches!(ch, '.' | '!' | '?' | '…') {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    // Длинные (для движка комфортнее <=150 симв.) дополнительно режем по ", "
    let mut sized = Vec::new();
    for s in out {
        let n = s.chars().count();
        if n <= 200 {
            sized.push(s);
            continue;
        }
        let mut cur = String::new();
        for part in s.split(", ") {
            let candidate = if cur.is_empty() {
                part.to_string()
            } else {
                format!("{cur}, {part}")
            };
            if candidate.chars().count() > 150 && !cur.is_empty() {
                sized.push(std::mem::take(&mut cur));
                cur = part.to_string();
            } else {
                cur = candidate;
            }
        }
        if !cur.trim().is_empty() {
            sized.push(cur);
        }
    }
    sized
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = Args::parse();
    if std::env::var("ORT_DYLIB_PATH").is_err() {
        bail!("ORT_DYLIB_PATH не задан");
    }
    // Граничные аргументы: % 0 в цикле сэмплинга запрещён, 0 чанков бессмысленен.
    anyhow::ensure!(
        args.chunks >= 1,
        "--chunks должен быть >= 1 (получено {})",
        args.chunks
    );
    let sample_every = args.sample_every.max(1);
    if let Some(csv) = &args.csv {
        // Политика безопасности: не перезаписывать существующие файлы.
        anyhow::ensure!(
            !csv.exists(),
            "CSV уже существует: {} — удалите/переименуйте вручную или укажите другое имя",
            csv.display()
        );
    }
    let device = match args.device.as_str() {
        "cpu" => Device::Cpu,
        "cuda" => Device::cuda(args.device_id),
        other => bail!("--device cpu|cuda, got {other}"),
    };
    let seed = if args.seed < 0 {
        None
    } else {
        Some(args.seed as u64)
    };
    let voice = args.voice.to_string_lossy().to_string();

    // Корпус: стресс-режим (из файла) или одна фраза
    let corpus: Vec<String> = match &args.text_file {
        Some(p) => load_corpus(p)?,
        None => vec![text_processing::to_model_stress_notation(&args.text)],
    };
    let stress = args.text_file.is_some();

    let mem0 = sample_mem();
    let t = Instant::now();
    let mut engine = Pocket::load(
        &args.model_dir,
        &LoadOptions {
            device,
            threads: args.threads,
            seed,
            profile_dir: None,
        },
    )?;
    let load_s = t.elapsed().as_secs_f32();
    let mem_load = sample_mem(); // фактическая память сразу после загрузки модели
    engine.temperature = args.temperature;
    engine.decode_steps = args.decode_steps as usize;

    // Прогрев
    let t = Instant::now();
    for i in 0..args.warmup {
        let phrase = &corpus[(i as usize) % corpus.len()];
        let _ = engine.say_chunk(phrase, &voice, 1_000_000 + i as u64)?;
    }
    let warmup_s = t.elapsed().as_secs_f32();
    let mem_warm = sample_mem();

    // CSV (стресс)
    let mut csv = match (&args.csv, stress) {
        (Some(p), true) => {
            if let Some(parent) = p.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            // Атомарно-эксклюзивно: существующий CSV не перезаписывается
            // (политика SAFETY_AUDIT.md); выше уже есть ensure!(!csv.exists()).
            let mut w = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)?;
            use std::io::Write;
            writeln!(w, "phase,chunk,ws_mb,private_mb,gpu_total_mb")?;
            writeln!(
                w,
                "start,0,{:.1},{:.1},{:.1}",
                mem0.ws, mem0.priv_mb, mem0.gpu
            )?;
            writeln!(
                w,
                "after_load,0,{:.1},{:.1},{:.1}",
                mem_load.ws, mem_load.priv_mb, mem_load.gpu
            )?;
            Some(w)
        }
        _ => None,
    };

    // Измерение
    let mut times = Vec::with_capacity(args.chunks as usize);
    let mut audio_secs = 0.0f32;
    let mut steady_pts_ws: Vec<(f64, f64)> = Vec::new();
    let mut steady_pts_priv: Vec<(f64, f64)> = Vec::new();
    let mut steady_pts_gpu: Vec<(f64, f64)> = Vec::new();
    let t_all = Instant::now();
    for i in 0..args.chunks {
        let phrase = &corpus[(i as usize) % corpus.len()];
        let t = Instant::now();
        let audio = engine.say_chunk(phrase, &voice, i as u64)?;
        times.push(t.elapsed().as_secs_f32());
        audio_secs += audio.len() as f32 / POCKET_RATE as f32;
        drop(audio); // инструмент не копит аудио
        if stress && (i + 1) % sample_every == 0 {
            let m = sample_mem();
            let x = (i + 1) as f64;
            steady_pts_ws.push((x, m.ws));
            steady_pts_priv.push((x, m.priv_mb));
            steady_pts_gpu.push((x, m.gpu));
            if let Some(w) = csv.as_mut() {
                use std::io::Write;
                writeln!(
                    w,
                    "steady,{},{:.1},{:.1},{:.1}",
                    i + 1,
                    m.ws,
                    m.priv_mb,
                    m.gpu
                )?;
            }
            log::info!(
                "chunk {}/{}: WS {:.0} priv {:.0} gpu {:.0}",
                i + 1,
                args.chunks,
                m.ws,
                m.priv_mb,
                m.gpu
            );
        }
    }
    let total_s = t_all.elapsed().as_secs_f32();
    let mem1 = sample_mem();

    // Освобождение: дропаем движок, ждём, меряем
    drop(engine);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let mem_after_drop = sample_mem();

    let mean = times.iter().sum::<f32>() / times.len() as f32;
    let rtf = total_s / audio_secs.max(1e-6);
    let model_name = args
        .model_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    if stress {
        // Тренды: срез после прогрева (первая точка) к концу
        let slope_ws = slope(&steady_pts_ws) * 1000.0; // МБ / 1000 чанков
        let slope_priv = slope(&steady_pts_priv) * 1000.0;
        let slope_gpu = slope(&steady_pts_gpu) * 1000.0;
        println!("stress | {tag} | {model_name} | {dev} | phrases {nph} | load {load:.1}s | warmup {warm:.1}s ({wu} chunks)",
            tag = args.tag, model_name = model_name, dev = args.device, nph = corpus.len(),
            load = load_s, warm = warmup_s, wu = args.warmup);
        println!("stress | chunks {n} | mean {mean:.3}s p95 {p95:.3}s | audio {audio:.1}s in {total:.1}s | RTF {rtf:.3} ({x:.1}x)",
            n = args.chunks, mean = mean, p95 = p95(times), audio = audio_secs, total = total_s, rtf = rtf,
            x = if rtf > 0.0 { 1.0 / rtf } else { 0.0 });
        println!("stress | mem: after-warmup WS {ws0:.0}/priv {pr0:.0}/gpu {g0:.0} MB -> end WS {ws1:.0}/priv {pr1:.0}/gpu {g1:.0} MB",
            ws0 = mem_warm.ws, pr0 = mem_warm.priv_mb, g0 = mem_warm.gpu,
            ws1 = mem1.ws, pr1 = mem1.priv_mb, g1 = mem1.gpu);
        println!("stress | trend (МНК, МБ/1000 чанков): WS {sws:+.1} | private {spr:+.1} | gpu {sgpu:+.1}",
            sws = slope_ws, spr = slope_priv, sgpu = slope_gpu);
        println!("stress | after drop: WS {ws:.0} MB, private {pr:.0} MB, gpu {g:.0} MB (возврат: WS {dws:+.0}, priv {dpr:+.0}, gpu {dg:+.0})",
            ws = mem_after_drop.ws, pr = mem_after_drop.priv_mb, g = mem_after_drop.gpu,
            dws = mem_after_drop.ws - mem1.ws, dpr = mem_after_drop.priv_mb - mem1.priv_mb,
            dg = mem_after_drop.gpu - mem1.gpu);
        if let Some(p) = &args.csv {
            // дописываем финальные строки
            drop(csv);
            use std::io::Write;
            let mut w = std::fs::OpenOptions::new().append(true).open(p)?;
            writeln!(
                w,
                "end,{},{:.1},{:.1},{:.1}",
                args.chunks, mem1.ws, mem1.priv_mb, mem1.gpu
            )?;
            writeln!(
                w,
                "after_drop,{},{:.1},{:.1},{:.1}",
                args.chunks, mem_after_drop.ws, mem_after_drop.priv_mb, mem_after_drop.gpu
            )?;
        }
    } else {
        println!("| {tag} | {model_name} | {dev} | {steps} | {threads} | {load:.1} | {warm:.1} | {mean:.3} | {p95:.3} | {audio:.1} | {total:.1} | {rtf:.3} | {ws:.0} | {priv:.0} | {vram:.0} |",
            tag = args.tag, model_name = model_name,
            dev = args.device, steps = args.decode_steps, threads = args.threads,
            load = load_s, warm = warmup_s, mean = mean, p95 = p95(times.clone()),
            audio = audio_secs, total = total_s, rtf = rtf,
            ws = mem1.ws - mem0.ws,
            priv = mem1.priv_mb - mem0.priv_mb,
            vram = app_core::diagnostics::vram_used_mb(app_core::diagnostics::pid()),
        );
        eprintln!(
            "load {load_s:.1}s, warmup {} in {warmup_s:.1}s, {} chunks: mean {mean:.3}s p95 {:.3}s, audio {audio_secs:.1}s in {total_s:.1}s, RTF {rtf:.3} ({:.1}x), peak WS {:.0} MB",
            args.warmup, args.chunks, p95(times), if rtf > 0.0 { 1.0 / rtf } else { 0.0 },
            mem1.ws,
        );
    }
    Ok(())
}
