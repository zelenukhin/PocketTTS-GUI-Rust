//! CUDA-P2B.3 — диагностический runner гибридного параллелизма:
//! N независимых процессов × W CUDA-воркеров в каждом при общей очереди
//! файлов пакета.
//!
//! Одна команда `hybrid` покрывает ВСЮ матрицу этапа — различаются только
//! границы процессов и число воркеров, диспетчер один и тот же:
//!
//! | ID  | launcher | процессов | воркеров/процесс | всего Pocket |
//! |-----|----------|----------|------------------|--------------|
//! | A1  | a1       | 1        | 1                | 1            |
//! | A2  | a2       | 1        | 2                | 2            |
//! | A3  | a3       | 1        | 3                | 3            |
//! | A4  | a4       | 1        | 4                | 4            |
//! | B2  | b2       | 2        | 1                | 2            |
//! | H22 | h22      | 2        | 2                | 4            |
//! | B4  | b4       | 4        | 1                | 4            |
//!
//! Поведение каждого процесса (задание CUDA-P2B.3 §6):
//! 1. инициализирует собственный ORT CUDA EP (через загрузку Pocket);
//! 2. создаёт W независимых Pocket — по одному на поток-воркер;
//! 3. загружает каждую модель РОВНО один раз и держит до конца пакета;
//! 4. сообщает диспетчеру о готовности ВСЕХ своих воркеров
//!    (событие `workers_all_ready`, строка `ГОТОВ` в stdout и флаг-файл
//!    `ready_<pid>.flag` в каталоге очереди; пока все процессы пакета не
//!    готовы или не истечёт гейт-таймаут, файлы не разбираются);
//! 5. разбирает файлы из общей очереди (claim-локи `claim_XX.lock`,
//!    `create_new`) — следующий свободный файл получает ЛЮБОЙ свободный
//!    воркер ЛЮБОГО процесса, статического закрепления файлов нет;
//! 6. один файл целиком обрабатывает один Pocket (`run_job workers=1`),
//!    после файла Pocket переиспользуется для следующего;
//! 7. Pocket освобождается только после исчерпания очереди.
//!
//! Оркестрация позаимствована из `diag_p2b2.rs` (Events/Loaned/
//! `process_file`/`claim`); математика синтеза и DSP не дублируется —
//! используются `RealWorker`/`Pocket`/`WavSink` из app-core, конвейер файла
//! тот же, что в `pipeline::synthesize_stream`. Паритет проверяется
//! побайтово по SHA256 WAV против одиночных запусков CLI.
//!
//! Инструмент диагностический: в GUI не входит, обычный пакетный режим не
//! меняет. Рекурсивных удалений нет; все выходы — только новые уникальные
//! пути (`create_new`), существующие файлы не перезаписываются.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app_core::pipeline::PipelineOptions;
use app_core::pool::{run_job, ChunkAudio, ChunkWorker, PoolConfig, SeedPolicy, Task};
use app_core::sink::{AudioSink, WavSink};
use app_core::worker::{real_worker_factory, RealWorker};
use shtorm_engine::{Device, EosTail, ModelPrecision, POCKET_RATE};
use text_processing::{
    apply_char_rules, chunk_pause_s, chunk_pipeline, to_model_stress_notation, CharRules,
};

/// Гейт межпроцессной готовности: ждать флаги остальных процессов пакета
/// не дольше этого времени (зависший/упавший сосед не блокирует пакет —
/// claim-локи гарантируют, что файлы заберут живые воркеры).
const READY_GATE_TIMEOUT: Duration = Duration::from_secs(180);
/// Период опроса флагов готовности/очереди в гейте.
const READY_GATE_POLL: Duration = Duration::from_millis(10);

#[derive(Parser, Debug)]
#[command(about = "CUDA-P2B.3: гибрид N процессов × W CUDA-воркеров на пакете файлов")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Гибридный режим: процессы × воркеры, общая claim-очередь файлов.
    Hybrid(HybridArgs),
}

/// Параметры синтеза — 1:1 с `diag_p2b2` / `synthesize.exe` (P2B.1) и GUI.
#[derive(Args, Debug, Clone)]
struct SynthArgs {
    #[arg(long)]
    model_dir: PathBuf,
    #[arg(long)]
    voice: PathBuf,
    #[arg(long, default_value = "cuda")]
    device: String,
    #[arg(long, default_value_t = 0)]
    device_id: i32,
    #[arg(long, default_value_t = 4)]
    threads: usize,
    /// CPU-исследование: точность графов (fp32|int8), как в synthesize.exe.
    #[arg(long, default_value = "fp32")]
    precision: String,
    #[arg(long, default_value_t = 0.3)]
    temperature: f32,
    #[arg(long, default_value_t = 1)]
    decode_steps: u32,
    #[arg(long, default_value_t = -4.0, allow_negative_numbers = true)]
    eos_threshold: f32,
    /// 0..=10 или none (штатная политика движка 5/3).
    #[arg(long, default_value = "5")]
    eos_tail_short: String,
    #[arg(long, default_value = "3")]
    eos_tail_long: String,
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
    #[arg(long)]
    char_rules: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct HybridArgs {
    #[command(flatten)]
    synth: SynthArgs,
    /// Входные файлы пакета (каждый обрабатывается одним воркером целиком).
    #[arg(long, num_args = 1.., required = true)]
    files: Vec<PathBuf>,
    /// Постоянных воркеров в ЭТОМ процессе (каждый со своим Pocket).
    #[arg(long, default_value_t = 1)]
    workers: usize,
    /// Сколько всего процессов делят пакет (гейт готовности). 1 = A-режимы.
    #[arg(long, default_value_t = 1)]
    expect_processes: usize,
    /// Каталог общей очереди: claim_XX.lock (create_new) — «файл мой»;
    /// здесь же флаги готовности ready_<pid>.flag.
    #[arg(long)]
    queue_dir: PathBuf,
    /// Уникальный каталог выходных WAV (создаётся, должен не существовать
    /// уникальность гарантирует launcher; WAV-синк откажет на существующем).
    #[arg(long)]
    out_dir: PathBuf,
    /// JSONL-журнал событий процесса.
    #[arg(long)]
    events: PathBuf,
    /// Префикс имён выходных файлов (например h22_p0).
    #[arg(long)]
    tag: String,
}

// ================================ события ================================

/// Потокобезопасный журнал событий (JSON Lines) с монотонным временем
/// процесса и UTC-меткой для межпроцессной склейки. Как в `diag_p2b2`.
struct Events {
    file: Mutex<std::fs::File>,
    t0: Instant,
    pid: u32,
}

impl Events {
    fn create(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("создать журнал событий {}", path.display()))?;
        Ok(Self {
            file: Mutex::new(file),
            t0: Instant::now(),
            pid: std::process::id(),
        })
    }

    fn mono_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    fn emit(&self, ev: &str, fields: serde_json::Value) {
        let mut obj = match fields {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        obj.insert("ev".into(), serde_json::Value::String(ev.to_string()));
        obj.insert("pid".into(), serde_json::Value::from(self.pid));
        obj.insert("mono_ms".into(), serde_json::Value::from(self.mono_ms()));
        obj.insert("utc_ms".into(), serde_json::Value::from(utc_ms()));
        let line = serde_json::Value::Object(obj).to_string();
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

fn utc_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ========================= очередь и гейт готовности =========================

/// Захват файла из общей очереди: эксклюзивное создание `claim_XX.lock`.
/// `None` — свободных файлов нет, пакет обработан. Атомарность `create_new`
/// гарантирует: каждый файл достаётся ровно одному воркеру ровно один раз,
/// независимо от процесса.
fn claim(queue_dir: &Path, count: usize) -> Result<Option<usize>> {
    for i in 0..count {
        let p = queue_dir.join(format!("claim_{i:02}.lock"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "pid={} file={i}", std::process::id());
                return Ok(Some(i));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => bail!("claim {}: {e}", p.display()),
        }
    }
    Ok(None)
}

/// Число флагов готовности процессов в каталоге очереди.
fn count_ready_flags(queue_dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(queue_dir) else {
        return 0;
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("ready_") && n.ends_with(".flag")
        })
        .count()
}

/// Гейт готовности пакета: свой флаг + ожидание флагов остальных процессов.
/// Возвращает (сколько флагов увидели, сколько ждали). По таймауту —
/// продолжаем: claim-локи не дают пакету потерять целостность.
fn ready_gate(queue_dir: &Path, expect: usize) -> (usize, u64) {
    let flag = queue_dir.join(format!("ready_{}.flag", std::process::id()));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&flag)
    {
        let _ = writeln!(f, "pid={} workers_ready", std::process::id());
    }
    let t0 = Instant::now();
    loop {
        let seen = count_ready_flags(queue_dir);
        if seen >= expect {
            return (seen, t0.elapsed().as_millis() as u64);
        }
        if t0.elapsed() > READY_GATE_TIMEOUT {
            return (seen, t0.elapsed().as_millis() as u64);
        }
        std::thread::sleep(READY_GATE_POLL);
    }
}

// ============================ синтез одного файла ============================

/// Воркер, одолженный у постоянного пула на время одного файла: `run_job`
/// создаёт и дропает экземпляры, а Pocket обязан выжить между файлами.
struct Loaned {
    slot: Arc<Mutex<Option<RealWorker>>>,
    inner: Option<RealWorker>,
}

impl ChunkWorker for Loaned {
    fn process(&mut self, task: &Task) -> Result<ChunkAudio> {
        self.inner
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("одолженный воркер пуст"))?
            .process(task)
    }
}

impl Drop for Loaned {
    fn drop(&mut self) {
        if let Some(w) = self.inner.take() {
            if let Ok(mut slot) = self.slot.lock() {
                *slot = Some(w);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct FileResult {
    name: String,
    out: PathBuf,
    chunks: usize,
    speech_samples: u64,
    pause_samples: u64,
    gen_ms: u128,
    chunk_ms_sum: u128,
}

/// Один файл целиком на ОДНОМ постоянном воркере: ровно тот же конвейер, что
/// `pipeline::synthesize_stream` (чанкер → stress-нотация → char_rules →
/// паузы → пул `workers=1` → WavSink → publish), но с внешним Pocket.
/// Как в `diag_p2b2` — воспроизведено дословно, чтобы конвейер файла не
/// отличался от P2B.2 ни на байт (паритет SHA256).
#[allow(clippy::too_many_arguments)]
fn process_file(
    slot: &Arc<Mutex<Option<RealWorker>>>,
    name: &str,
    text: &str,
    out: &Path,
    opts: &PipelineOptions,
    base_seed: u64,
    cancel: &AtomicBool,
) -> Result<FileResult> {
    let chunks = chunk_pipeline(text, opts.max_tokens);
    anyhow::ensure!(
        !chunks.is_empty(),
        "текст пуст — нечего синтезировать: {name}"
    );
    let total = chunks.len();
    let tasks: Vec<Task> = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut model_text = to_model_stress_notation(&chunk.text);
            if let Some(rules) = &opts.char_rules {
                model_text = apply_char_rules(&model_text, rules);
            }
            let pause_s = chunk_pause_s(
                chunk,
                i + 1 == total,
                opts.paragraph_pause_s,
                opts.pause_scale,
            );
            Task {
                index: i as u64,
                model_text,
                pause_s,
            }
        })
        .collect();

    let mut sink: Box<dyn AudioSink> = Box::new(WavSink::create(out, POCKET_RATE)?);
    let slot_for_factory = Arc::clone(slot);
    let factory = move |_wid: usize, _seed: u64| -> Result<Loaned> {
        let worker = slot_for_factory
            .lock()
            .map_err(|_| anyhow::anyhow!("мьютекс воркера отравлен"))?
            .take()
            .ok_or_else(|| anyhow::anyhow!("постоянный воркер уже занят"))?;
        Ok(Loaned {
            slot: Arc::clone(&slot_for_factory),
            inner: Some(worker),
        })
    };

    let t0 = Instant::now();
    let mut written = 0usize;
    let run = run_job(
        &PoolConfig {
            workers: 1,
            queue_cap: 4,
        },
        tasks.clone(),
        factory,
        |i, pcm, pause| {
            sink.write_chunk(i, pcm, pause)?;
            written += 1;
            Ok(())
        },
        cancel,
        SeedPolicy::Fixed(base_seed),
    );
    let gen_ms = t0.elapsed().as_millis();
    let summary = run?;
    anyhow::ensure!(
        !summary.cancelled,
        "файл {name}: отмена — результат не публикуется"
    );
    anyhow::ensure!(
        summary.chunks_done == total && written == total,
        "файл {name}: неполный результат {}/{total}",
        summary.chunks_done
    );
    let published = sink.finish()?.publish()?;

    let speech_samples: u64 = summary.timings.iter().map(|t| t.samples as u64).sum();
    let pause_samples: u64 = tasks
        .iter()
        .map(|t| {
            t.pause_s
                .map(|p| (p.max(0.0) * POCKET_RATE as f32) as u64)
                .unwrap_or(0)
        })
        .sum();
    let chunk_ms_sum: u128 = summary.timings.iter().map(|t| t.ms).sum();
    Ok(FileResult {
        name: name.to_string(),
        out: published,
        chunks: total,
        speech_samples,
        pause_samples,
        gen_ms,
        chunk_ms_sum,
    })
}

fn read_text(p: &Path) -> Result<String> {
    let raw = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

fn out_name(tag: &str, path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    format!("{tag}_{stem}.wav")
}

fn file_names(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|p| {
            p.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        })
        .collect()
}

fn release_pocket(slot: &Arc<Mutex<Option<RealWorker>>>) -> Result<()> {
    let mut guard = slot
        .lock()
        .map_err(|_| anyhow::anyhow!("мьютекс воркера отравлен"))?;
    guard.take(); // Pocket уничтожается ЗДЕСЬ — в своём потоке/процессе
    Ok(())
}

// ============================== режим hybrid ==============================

fn parse_eos_tail(s: &str) -> Result<EosTail> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" => Ok(EosTail::Default),
        other => {
            let n: u32 = other
                .parse()
                .map_err(|_| anyhow::anyhow!("хвост EOS: 'none' или число 0..=10, '{other}'"))?;
            if n > 10 {
                bail!("хвост EOS: максимум 10, получено {n}");
            }
            Ok(EosTail::Fixed(n))
        }
    }
}

fn load_char_rules(p: &Option<PathBuf>) -> Result<Option<CharRules>> {
    let Some(p) = p else { return Ok(None) };
    let raw = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    match v.get("char_rules") {
        Some(cr) => Ok(Some(
            serde_json::from_value(cr.clone()).context("секция char_rules не разобрана")?,
        )),
        None => {
            eprintln!(
                "ВНИМАНИЕ: в {} нет секции char_rules — правила не применяются.",
                p.display()
            );
            Ok(None)
        }
    }
}

impl SynthArgs {
    fn to_options(&self) -> Result<PipelineOptions> {
        let device = match self.device.as_str() {
            "cpu" => Device::Cpu,
            "cuda" => Device::cuda(self.device_id),
            other => bail!("--device cpu|cuda, получено {other}"),
        };
        if self.seed < 0 {
            bail!("диагностический runner требует фиксированный seed (>= 0)");
        }
        let precision = match self.precision.as_str() {
            "fp32" => ModelPrecision::Fp32,
            "int8" => ModelPrecision::Int8,
            other => bail!("--precision fp32|int8, получено {other}"),
        };
        Ok(PipelineOptions {
            model_dir: self.model_dir.clone(),
            voice: self.voice.clone(),
            device,
            threads: self.threads,
            precision,
            seed: Some(self.seed as u64),
            temperature: self.temperature,
            decode_steps: self.decode_steps as usize,
            eos_threshold: self.eos_threshold,
            eos_tail_short: parse_eos_tail(&self.eos_tail_short)?,
            eos_tail_long: parse_eos_tail(&self.eos_tail_long)?,
            brightness_db: self.brightness_db,
            denoise: self.denoise,
            pause_scale: self.pause_scale,
            paragraph_pause_s: self.paragraph_pause_s,
            max_tokens: self.max_tokens,
            char_rules: load_char_rules(&self.char_rules)?,
            anti_repeat: true,
        })
    }
}

fn run_hybrid(a: &HybridArgs) -> Result<()> {
    anyhow::ensure!(a.workers >= 1, "--workers >= 1");
    anyhow::ensure!(a.expect_processes >= 1, "--expect-processes >= 1");
    // Несколько процессов делят каталоги очереди и выхода: create_dir_all
    // идемпотентен, эксклюзивность дают claim-локи (create_new) и WAV
    // create_new; уникальность каталогов раунда гарантирует launcher.
    std::fs::create_dir_all(&a.queue_dir)?;
    std::fs::create_dir_all(&a.out_dir)?;
    let opts = a.synth.to_options()?;
    let base_seed = opts.seed.expect("seed задан");
    let events = Arc::new(Events::create(&a.events)?);
    let names = file_names(&a.files);
    events.emit(
        "process_start",
        serde_json::json!({
            "mode": "hybrid",
            "workers": a.workers,
            "expect_processes": a.expect_processes,
            "files": names,
            "tag": a.tag,
            "device": a.synth.device,
            "precision": a.synth.precision,
            "threads": a.synth.threads,
            "io_binding_env": std::env::var("SHTORM_CUDA_IO_BINDING").ok(),
        }),
    );

    let cancel = Arc::new(AtomicBool::new(false));
    let factory = Arc::new(real_worker_factory(&opts, base_seed));
    // Готовность своих воркеров и межпроцессный гейт — спинами с флагом
    // load_failed, а не Barrier: упавшая загрузка у соседа не вешает
    // остальных в вечном ожидании.
    let ready_count = Arc::new(AtomicUsize::new(0));
    let load_failed = Arc::new(AtomicBool::new(false));
    let gate_open = Arc::new(AtomicBool::new(false));
    let files_done = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(a.workers);

    for wid in 0..a.workers {
        let factory = Arc::clone(&factory);
        let ready_count = Arc::clone(&ready_count);
        let load_failed = Arc::clone(&load_failed);
        let gate_open = Arc::clone(&gate_open);
        let files_done = Arc::clone(&files_done);
        let events = Arc::clone(&events);
        let cancel = Arc::clone(&cancel);
        let files = a.files.clone();
        let queue_dir = a.queue_dir.clone();
        let out_dir = a.out_dir.clone();
        let tag = a.tag.clone();
        let names = names.clone();
        let opts = opts.clone();
        let workers_total = a.workers;
        let expect_processes = a.expect_processes;
        handles.push(std::thread::spawn(move || -> Result<()> {
            let t_load = Instant::now();
            events.emit("worker_load_start", serde_json::json!({"worker": wid}));
            let worker = match factory(wid, base_seed) {
                Ok(w) => w,
                Err(e) => {
                    load_failed.store(true, Ordering::SeqCst);
                    events.emit(
                        "worker_load_failed",
                        serde_json::json!({
                            "worker": wid, "error": format!("{e:#}"),
                        }),
                    );
                    return Err(e);
                }
            };
            events.emit(
                "worker_ready",
                serde_json::json!({
                    "worker": wid,
                    "load_ms": t_load.elapsed().as_millis() as u64,
                }),
            );
            // (1) свои воркеры: последний готовый фиксирует момент
            if ready_count.fetch_add(1, Ordering::SeqCst) + 1 == workers_total {
                events.emit(
                    "workers_all_ready",
                    serde_json::json!({"workers": workers_total}),
                );
            }
            while ready_count.load(Ordering::SeqCst) < workers_total
                && !load_failed.load(Ordering::SeqCst)
            {
                std::thread::sleep(READY_GATE_POLL);
            }
            if load_failed.load(Ordering::SeqCst) {
                bail!("загрузка воркера-соседа не удалась — пакет не начинается");
            }
            // (2) все процессы пакета: флаг + ожидание чужих флагов.
            // «Сообщает диспетчеру о готовности» — строка ГОТОВ в stdout
            // для внешнего диспетчера (launcher) и флаг в каталоге очереди.
            if wid == 0 {
                println!("ГОТОВ pid={} workers={workers_total}", std::process::id());
                let (seen, waited_ms) = ready_gate(&queue_dir, expect_processes);
                events.emit(
                    "ready_gate",
                    serde_json::json!({
                        "expect": expect_processes, "seen": seen,
                        "waited_ms": waited_ms,
                        "timeout": seen < expect_processes,
                    }),
                );
                gate_open.store(true, Ordering::SeqCst);
            }
            while !gate_open.load(Ordering::SeqCst) && !load_failed.load(Ordering::SeqCst) {
                std::thread::sleep(READY_GATE_POLL);
            }

            let slot = Arc::new(Mutex::new(Some(worker)));
            let mut local_claims = 0usize;
            loop {
                let Some(i) = claim(&queue_dir, files.len())? else {
                    break; // очередь исчерпана
                };
                let path = &files[i];
                let name = names.get(i).cloned().unwrap_or_default();
                events.emit(
                    "file_claim",
                    serde_json::json!({
                        "worker": wid,
                        "file": i,
                        "name": name,
                        // true = файл пошёл на УЖЕ загруженный Pocket
                        "reuse": local_claims > 0,
                    }),
                );
                local_claims += 1;
                let text = read_text(path)?;
                let out = out_dir.join(out_name(&tag, path));
                match process_file(&slot, &name, &text, &out, &opts, base_seed, &cancel) {
                    Ok(r) => {
                        files_done.fetch_add(1, Ordering::SeqCst);
                        events.emit(
                            "file_done",
                            serde_json::json!({
                                "worker": wid,
                                "file": i,
                                "name": r.name,
                                "out": r.out,
                                "gen_ms": r.gen_ms as u64,
                                "chunk_ms_sum": r.chunk_ms_sum as u64,
                                "chunks": r.chunks,
                                "speech_s": r.speech_samples as f64 / POCKET_RATE as f64,
                                "pause_s": r.pause_samples as f64 / POCKET_RATE as f64,
                            }),
                        );
                    }
                    Err(e) => {
                        events.emit(
                            "file_failed",
                            serde_json::json!({
                                "worker": wid, "file": i, "name": name,
                                "error": format!("{e:#}"),
                            }),
                        );
                        return Err(e);
                    }
                }
            }
            // Pocket освобождается только после исчерпания очереди пакета
            release_pocket(&slot)?;
            events.emit("worker_release", serde_json::json!({"worker": wid}));
            Ok(())
        }));
    }

    let mut first_err: Option<anyhow::Error> = None;
    for (wid, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(_) => {
                if first_err.is_none() {
                    first_err = Some(anyhow::anyhow!("поток воркера {wid} паниковал"));
                }
            }
        }
    }
    events.emit(
        "process_end",
        serde_json::json!({
            "mode": "hybrid",
            "pocket_loads": a.workers,
            "files_done": files_done.load(Ordering::SeqCst),
            "files": names.len(),
            "ok": first_err.is_none(),
        }),
    );
    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let res = match &cli.cmd {
        Cmd::Hybrid(a) => run_hybrid(a),
    };
    match res {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ОШИБКА: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Уникальная песочница в %TEMP% (файлы НЕ удаляются — политика проекта).
    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shtorm_p2b3_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn claim_locks_are_exclusive_and_exhaustive() {
        let dir = sandbox("claim");
        // 3 файла: первые три захвата дают 0,1,2 в порядке разбора,
        // четвёртый — None (очередь исчерпана)
        assert_eq!(claim(&dir, 3).unwrap(), Some(0));
        assert_eq!(claim(&dir, 3).unwrap(), Some(1));
        assert_eq!(claim(&dir, 3).unwrap(), Some(2));
        assert_eq!(claim(&dir, 3).unwrap(), None);
    }

    #[test]
    fn ready_flags_counted_only_by_pattern() {
        let dir = sandbox("gate");
        assert_eq!(count_ready_flags(&dir), 0);
        std::fs::write(dir.join("claim_00.lock"), b"").unwrap();
        assert_eq!(
            count_ready_flags(&dir),
            0,
            "claim-локи не считаются флагами готовности"
        );
        std::fs::write(dir.join("ready_111.flag"), b"").unwrap();
        std::fs::write(dir.join("ready_222.flag"), b"").unwrap();
        std::fs::write(dir.join("ready_flags.txt"), b"").unwrap();
        assert_eq!(count_ready_flags(&dir), 2, "считаются только ready_*.flag");
    }

    #[test]
    fn ready_gate_passes_when_expect_met() {
        let dir = sandbox("gatepass");
        std::fs::write(dir.join("ready_1.flag"), b"").unwrap();
        std::fs::write(dir.join("ready_2.flag"), b"").unwrap();
        // свой флаг станет третьим; expect=3 должен пройти без таймаута
        let (seen, waited_ms) = ready_gate(&dir, 3);
        assert_eq!(seen, 3);
        assert!(waited_ms < 5_000, "гейт не должен ждать при готовых флагах");
    }

    #[test]
    fn out_names_are_unique_per_tag_and_file() {
        let a = out_name("h22_p0", Path::new("out\\part_01.txt"));
        let b = out_name("h22_p1", Path::new("out\\part_01.txt"));
        let c = out_name("h22_p0", Path::new("out\\part_02.txt"));
        assert_eq!(a, "h22_p0_part_01.wav");
        assert_ne!(a, b, "разные процессы — разные имена на один файл");
        assert_ne!(a, c, "разные файлы — разные имена");
    }
}
