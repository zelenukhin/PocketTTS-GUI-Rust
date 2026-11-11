//! CUDA-P2B.2 — диагностический runner: постоянный пул CUDA-воркеров в одном
//! процессе (вариант A) и постоянные независимые процессы (вариант B).
//!
//! Задача этапа — выяснить, что быстрее для ПАКЕТА из нескольких файлов:
//! держать воркеры/процессы с уже загруженной моделью и передавать им файлы
//! по очереди (модель загружается ОДИН раз на воркер/процесс), или
//! переиспользовать существующий CLI (модель на каждый файл). Здесь
//! реализованы именно «постоянные» режимы; обычный `synthesize.exe` служит
//! эталоном одиночного запуска.
//!
//! Границы эксперимента (сопоставимость A и B):
//! - один файл обрабатывается ОДНИМ воркером целиком (чанки файла не
//!   распределяются между воркерами) — `run_job` с `workers = 1`;
//! - каждый воркер = собственный `Pocket` + собственные ONNX-сессии,
//!   созданные один раз до первого файла и живущие до конца пакета;
//! - математика синтеза, DSP, паузы, seed+index и аудиовывод — те же, что в
//!   `app-core::pipeline` (RealWorker + WavSink), только оркестрация файлов
//!   другая; паритет проверяется побитово по SHA256 WAV;
//! - A и B отличаются только границами процессов: в A очередь файлов —
//!   `AtomicUsize` в памяти, в B — файловые claim-локи (`create_new`) в общем
//!   каталоге, которые захватывает готовый процесс.
//!
//! Инструмент диагностический: не входит в GUI и не меняет обычный алгоритм
//! пакетного режима. Рекурсивных удалений нет; все выходы — только новые
//! уникальные пути (`create_new`), существующие файлы не перезаписываются.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use app_core::pipeline::PipelineOptions;
use app_core::pool::{run_job, ChunkAudio, ChunkWorker, PoolConfig, SeedPolicy, Task};
use app_core::sink::{AudioSink, WavSink};
use app_core::worker::{real_worker_factory, RealWorker};
use shtorm_engine::{Device, EosTail, ModelPrecision, POCKET_RATE};
use text_processing::{
    apply_char_rules, chunk_pause_s, chunk_pipeline, to_model_stress_notation, CharRules,
};

#[derive(Parser, Debug)]
#[command(
    about = "CUDA-P2B.2: постоянный пул воркеров (A) / постоянные процессы (B) на пакете файлов"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Статистика текста: байты, BOM, UTF-8, символы/слова/абзацы и ЧАНКИ
    /// штатным чанкером по каждому абзацу (для разбиения по нагрузке).
    Analyze(AnalyzeArgs),
    /// Разбить текст на N частей по числу ЧАНКОВ, по границам абзацев,
    /// побайтово без потерь; проверить восстановление исходных байтов.
    Split(SplitArgs),
    /// Вариант A: один процесс, N постоянных CUDA-воркеров, общая очередь файлов.
    Pool(PoolArgs),
    /// Вариант B: один процесс, ОДИН воркер, файлы из общей очереди (claim-локи).
    Process(ProcessArgs),
}

#[derive(Args, Debug)]
struct AnalyzeArgs {
    #[arg(long)]
    text_file: PathBuf,
    #[arg(long, default_value_t = 50)]
    max_tokens: u32,
    /// Куда записать JSON с разбивкой по абзацам (по умолчанию — stdout).
    #[arg(long)]
    json_out: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct SplitArgs {
    #[arg(long)]
    text_file: PathBuf,
    #[arg(long, default_value_t = 50)]
    max_tokens: u32,
    #[arg(long, default_value_t = 3)]
    parts: usize,
    /// Новый уникальный каталог для частей (обязан не существовать).
    #[arg(long)]
    out_dir: PathBuf,
    #[arg(long, default_value = "part")]
    prefix: String,
}

/// Параметры синтеза — 1:1 с `synthesize.exe` (P2B.1) и GUI.
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
struct PoolArgs {
    #[command(flatten)]
    synth: SynthArgs,
    /// Входные файлы пакета (каждый обрабатывается одним воркером целиком).
    #[arg(long, num_args = 1.., required = true)]
    files: Vec<PathBuf>,
    /// Постоянных воркеров (каждый со своим Pocket).
    #[arg(long, default_value_t = 1)]
    workers: usize,
    /// Уникальный каталог выходных WAV (обязан не существовать).
    #[arg(long)]
    out_dir: PathBuf,
    /// JSONL-журнал событий процесса.
    #[arg(long)]
    events: PathBuf,
    /// Префикс имён выходных файлов (например a2_w0).
    #[arg(long)]
    tag: String,
    /// После пакета повторить ПЕРВЫЙ файл на том же Pocket (проверка
    /// отсутствия переноса состояния между файлами).
    #[arg(long, default_value_t = false)]
    repeat_first: bool,
}

#[derive(Args, Debug)]
struct ProcessArgs {
    #[command(flatten)]
    synth: SynthArgs,
    #[arg(long, num_args = 1.., required = true)]
    files: Vec<PathBuf>,
    /// Каталог общей очереди: claim_XX.lock (create_new) — «файл мой».
    #[arg(long)]
    queue_dir: PathBuf,
    #[arg(long)]
    out_dir: PathBuf,
    #[arg(long)]
    events: PathBuf,
    #[arg(long)]
    tag: String,
    #[arg(long, default_value_t = false)]
    repeat_first: bool,
}

// ================================ события ================================

/// Потокобезопасный журнал событий (JSON Lines) с монотонным временем
/// процесса и UTC-меткой для межпроцессной склейки.
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

// =============================== текст ================================

/// Строка исходника (для штатного чанкера непустая строка = абзац) с точными
/// байтовыми границами, включая завершитель строки.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LineSpan {
    start: usize,
    end: usize,
}

/// Байтовые границы строк. Завершитель — `\n`, `\r\n` или одиночный `\r`
/// (штатный `chunk_text` нормализует все три к `\n`, поэтому абзацы те же).
fn line_spans(bytes: &[u8]) -> Vec<LineSpan> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let sep_len = match bytes[i] {
            b'\n' => 1,
            b'\r' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    2
                } else {
                    1
                }
            }
            _ => {
                i += 1;
                continue;
            }
        };
        out.push(LineSpan {
            start,
            end: i + sep_len,
        });
        i += sep_len;
        start = i;
    }
    if start < bytes.len() {
        out.push(LineSpan {
            start,
            end: bytes.len(),
        });
    }
    out
}

/// Содержимое абзаца ровно как у `chunk_text` (снят завершитель строки —
/// `\r`, `\n` или `\r\n`).
fn line_content<'a>(bytes: &'a [u8], span: &LineSpan) -> &'a [u8] {
    let mut end = span.end;
    if end > span.start && bytes[end - 1] == b'\n' {
        end -= 1;
    }
    if end > span.start && bytes[end - 1] == b'\r' {
        end -= 1;
    }
    &bytes[span.start..end]
}

/// Число чанков по каждой строке штатным чанкером (пустые строки — 0).
/// Чанкер не склеивает абзацы между собой, поэтому сумма по строкам в точности
/// равна числу чанков всего текста.
fn line_chunk_counts(bytes: &[u8], max_tokens: u32) -> Vec<(LineSpan, usize, String)> {
    line_spans(bytes)
        .into_iter()
        .map(|span| {
            let content = String::from_utf8_lossy(line_content(bytes, &span)).into_owned();
            let chunks = if content.trim().is_empty() {
                0
            } else {
                chunk_pipeline(&content, max_tokens).len()
            };
            (span, chunks, content)
        })
        .collect()
}

/// Индексы строк-границ: после строки `b[k]-1` начинается часть `k+1`.
/// Выбираем ближайшую к равномерной по ЧАНКАМ границу; при равенстве
/// отклонения предпочитаем пустую строку (чистый разрыв абзацев).
fn choose_boundaries(counts: &[usize], parts: usize) -> Result<Vec<usize>> {
    let n = counts.len();
    let total: usize = counts.iter().sum();
    let mut cumulative = Vec::with_capacity(n + 1);
    cumulative.push(0usize);
    for c in counts {
        cumulative.push(cumulative.last().copied().unwrap_or(0) + c);
    }
    let mut bounds: Vec<usize> = Vec::with_capacity(parts.saturating_sub(1));
    for k in 1..parts {
        let target = (total as f64) * (k as f64) / (parts as f64);
        let prev = bounds.last().copied().unwrap_or(0);
        let mut best: Option<(f64, usize)> = None;
        for b in (prev + 1)..=n {
            // оставляем хотя бы по строке на каждую из оставшихся частей
            if n - b < parts - k {
                break;
            }
            let dev = (cumulative[b] as f64 - target).abs();
            // пустая строка перед границей = разрыв абзацев (приоритет при
            // равном отклонении)
            let clean = b > 0 && counts[b - 1] == 0;
            let score = if clean { dev } else { dev + 0.5 };
            match best {
                Some((s, _)) if s <= score => {}
                _ => best = Some((score, b)),
            }
        }
        let Some((_, b)) = best else {
            bail!("не удалось выбрать границу {k} из {parts}");
        };
        bounds.push(b);
    }
    Ok(bounds)
}

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
        Ok(PipelineOptions {
            model_dir: self.model_dir.clone(),
            voice: self.voice.clone(),
            device,
            threads: self.threads,
            precision: ModelPrecision::Fp32,
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

fn out_name(tag: &str, index: usize, path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("file{index}"));
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

// ================================ режим A ================================

fn run_pool(a: &PoolArgs) -> Result<()> {
    anyhow::ensure!(a.workers >= 1, "--workers >= 1");
    // уникальность каталога гарантирует launcher; create_dir_all идемпотентен,
    // а WAV-синк всё равно откажет на существующем файле (create_new)
    std::fs::create_dir_all(&a.out_dir)?;
    let opts = a.synth.to_options()?;
    let base_seed = opts.seed.expect("seed задан");
    let events = Arc::new(Events::create(&a.events)?);
    let names = file_names(&a.files);
    events.emit(
        "process_start",
        serde_json::json!({
            "mode": "pool",
            "workers": a.workers,
            "files": names,
            "tag": a.tag,
            "device": a.synth.device,
            "threads": a.synth.threads,
            "io_binding_env": std::env::var("SHTORM_CUDA_IO_BINDING").ok(),
        }),
    );

    let cancel = Arc::new(AtomicBool::new(false));
    let factory = Arc::new(real_worker_factory(&opts, base_seed));
    let next = Arc::new(AtomicUsize::new(0));
    let repeat_done = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(a.workers);

    for wid in 0..a.workers {
        let factory = Arc::clone(&factory);
        let next = Arc::clone(&next);
        let repeat_done = Arc::clone(&repeat_done);
        let events = Arc::clone(&events);
        let cancel = Arc::clone(&cancel);
        let files = a.files.clone();
        let opts = opts.clone();
        let out_dir = a.out_dir.clone();
        let tag = a.tag.clone();
        let names = names.clone();
        let repeat_first = a.repeat_first;
        handles.push(std::thread::spawn(move || -> Result<()> {
            let t_load = Instant::now();
            events.emit("worker_load_start", serde_json::json!({"worker": wid}));
            let worker = factory(wid, base_seed)?;
            events.emit(
                "worker_ready",
                serde_json::json!({
                    "worker": wid,
                    "load_ms": t_load.elapsed().as_millis() as u64,
                }),
            );
            let slot = Arc::new(Mutex::new(Some(worker)));
            let mut local_claims = 0usize;
            loop {
                let mut i = next.fetch_add(1, Ordering::SeqCst);
                let mut is_repeat = false;
                if i >= files.len() {
                    // после пакета: первый файл ЕЩЁ РАЗ на том же Pocket —
                    // ровно один раз на весь прогон
                    if repeat_first && i == files.len() && !repeat_done.swap(true, Ordering::SeqCst)
                    {
                        i = 0;
                        is_repeat = true;
                    } else {
                        break;
                    }
                }
                let path = &files[i];
                let name = names.get(i).cloned().unwrap_or_default();
                events.emit(
                    "file_claim",
                    serde_json::json!({
                        "worker": wid,
                        "file": i,
                        "name": name,
                        "repeat": is_repeat,
                        // true = файл пошёл на УЖЕ загруженный Pocket
                        "reuse": is_repeat || local_claims > 0,
                    }),
                );
                local_claims += 1;
                let text = read_text(path)?;
                let file_name = if is_repeat {
                    out_name(&format!("{tag}_repeat"), i, path)
                } else {
                    out_name(&tag, i, path)
                };
                let out = out_dir.join(&file_name);
                match process_file(&slot, &name, &text, &out, &opts, base_seed, &cancel) {
                    Ok(r) => events.emit(
                        "file_done",
                        serde_json::json!({
                            "worker": wid,
                            "file": i,
                            "name": r.name,
                            "out": r.out,
                            "repeat": is_repeat,
                            "gen_ms": r.gen_ms as u64,
                            "chunk_ms_sum": r.chunk_ms_sum as u64,
                            "chunks": r.chunks,
                            "speech_s": r.speech_samples as f64 / POCKET_RATE as f64,
                            "pause_s": r.pause_samples as f64 / POCKET_RATE as f64,
                        }),
                    ),
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
            // Pocket освобождается только по завершении ВСЕГО пакета
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
            "mode": "pool",
            "pocket_loads": a.workers,
            "files": names.len(),
            "ok": first_err.is_none(),
        }),
    );
    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(())
}

// ================================ режим B ================================

/// Захват файла из общей очереди: эксклюзивное создание `claim_XX.lock`.
/// `None` — свободных файлов нет, пакет обработан.
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

fn run_process(a: &ProcessArgs) -> Result<()> {
    // Несколько процессов делят один каталог очереди и выхода: create_dir_all
    // идемпотентен, эксклюзивность даёт claim-lock (create_new) и WAV create_new.
    std::fs::create_dir_all(&a.queue_dir)?;
    std::fs::create_dir_all(&a.out_dir)?;
    let opts = a.synth.to_options()?;
    let base_seed = opts.seed.expect("seed задан");
    let events = Events::create(&a.events)?;
    let names = file_names(&a.files);
    events.emit(
        "process_start",
        serde_json::json!({
            "mode": "process",
            "workers": 1,
            "files": names,
            "tag": a.tag,
            "device": a.synth.device,
            "threads": a.synth.threads,
            "io_binding_env": std::env::var("SHTORM_CUDA_IO_BINDING").ok(),
        }),
    );

    let cancel = AtomicBool::new(false);
    let factory = real_worker_factory(&opts, base_seed);
    let t_load = Instant::now();
    events.emit("worker_load_start", serde_json::json!({"worker": 0}));
    let worker = factory(0, base_seed)?;
    events.emit(
        "worker_ready",
        serde_json::json!({"worker": 0, "load_ms": t_load.elapsed().as_millis() as u64}),
    );
    // «сообщает о готовности» — строка в stdout для внешнего диспетчера
    println!("ГОТОВ pid={}", std::process::id());

    let slot = Arc::new(Mutex::new(Some(worker)));
    let mut processed: Vec<usize> = Vec::new();
    let mut repeat_done = false;
    loop {
        let (i, is_repeat) = match claim(&a.queue_dir, a.files.len())? {
            Some(i) => (i, false),
            None if a.repeat_first && !repeat_done => {
                repeat_done = true;
                (0, true)
            }
            None => break,
        };
        if !is_repeat && processed.contains(&i) {
            bail!("файл {i} захвачен дважды — очередь повреждена");
        }
        processed.push(i);
        let path = &a.files[i];
        let name = names.get(i).cloned().unwrap_or_default();
        events.emit(
            "file_claim",
            serde_json::json!({
                "worker": 0,
                "file": i,
                "name": name,
                "repeat": is_repeat,
                "reuse": !is_repeat && processed.len() > 1,
            }),
        );
        let text = read_text(path)?;
        let file_name = if is_repeat {
            out_name(&format!("{}_repeat", a.tag), i, path)
        } else {
            out_name(&a.tag, i, path)
        };
        let out = a.out_dir.join(&file_name);
        let r = process_file(&slot, &name, &text, &out, &opts, base_seed, &cancel)?;
        events.emit(
            "file_done",
            serde_json::json!({
                "worker": 0,
                "file": i,
                "name": r.name,
                "out": r.out,
                "repeat": is_repeat,
                "gen_ms": r.gen_ms as u64,
                "chunk_ms_sum": r.chunk_ms_sum as u64,
                "chunks": r.chunks,
                "speech_s": r.speech_samples as f64 / POCKET_RATE as f64,
                "pause_s": r.pause_samples as f64 / POCKET_RATE as f64,
            }),
        );
    }
    release_pocket(&slot)?;
    events.emit("worker_release", serde_json::json!({"worker": 0}));
    events.emit(
        "process_end",
        serde_json::json!({
            "mode": "process",
            "pocket_loads": 1,
            "files_done": processed.len(),
            "files": names.len(),
            "ok": true,
        }),
    );
    Ok(())
}

// ============================== analyze/split ==============================

fn run_analyze(a: &AnalyzeArgs) -> Result<()> {
    let bytes =
        std::fs::read(&a.text_file).with_context(|| format!("read {}", a.text_file.display()))?;
    let has_bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
    let valid_utf8 = std::str::from_utf8(&bytes).is_ok();
    let text = String::from_utf8_lossy(&bytes);
    let lines = line_chunk_counts(&bytes, a.max_tokens);
    let total_chunks: usize = lines.iter().map(|(_, c, _)| *c).sum();
    let nonempty = lines.iter().filter(|(_, c, _)| *c > 0).count();
    let words = text.split_whitespace().count();
    let chars = text.chars().count();
    let crlf = bytes.windows(2).filter(|w| w == b"\r\n").count();
    let lone_cr = bytes
        .iter()
        .enumerate()
        .filter(|(i, b)| **b == b'\r' && bytes.get(i + 1) != Some(&b'\n'))
        .count();
    let mut per_paragraph = Vec::with_capacity(lines.len());
    for (idx, (span, chunks, content)) in lines.iter().enumerate() {
        per_paragraph.push(serde_json::json!({
            "line": idx + 1,
            "byte_start": span.start,
            "byte_end": span.end,
            "chunks": chunks,
            "chars": content.chars().count(),
            "blank": *chunks == 0,
        }));
    }
    let doc = serde_json::json!({
        "file": a.text_file,
        "bytes": bytes.len(),
        "utf8_bom": has_bom,
        "valid_utf8": valid_utf8,
        "crlf_count": crlf,
        "lone_cr_count": lone_cr,
        "chars": chars,
        "words": words,
        "lines": lines.len(),
        "paragraphs_nonempty": nonempty,
        "max_tokens": a.max_tokens,
        "chunks_total": total_chunks,
        "per_paragraph": per_paragraph,
    });
    match &a.json_out {
        Some(p) => {
            anyhow::ensure!(!p.exists(), "JSON уже существует: {}", p.display());
            std::fs::write(p, serde_json::to_string_pretty(&doc)?)?;
        }
        None => println!("{}", serde_json::to_string_pretty(&doc)?),
    }
    println!(
        "текст: {} байт, BOM {}, UTF-8 {}, CRLF {}, одиночных CR {}, символов {}, слов {}, строк {} (абзацев {}), ЧАНКОВ {} (max_tokens {})",
        bytes.len(),
        if has_bom { "ДА" } else { "нет" },
        if valid_utf8 { "да" } else { "НЕТ" },
        crlf,
        lone_cr,
        chars,
        words,
        lines.len(),
        nonempty,
        total_chunks,
        a.max_tokens
    );
    Ok(())
}

fn run_split(a: &SplitArgs) -> Result<()> {
    anyhow::ensure!(a.parts >= 2, "--parts >= 2");
    let bytes =
        std::fs::read(&a.text_file).with_context(|| format!("read {}", a.text_file.display()))?;
    let lines = line_chunk_counts(&bytes, a.max_tokens);
    let counts: Vec<usize> = lines.iter().map(|(_, c, _)| *c).collect();
    let total: usize = counts.iter().sum();
    let bounds = choose_boundaries(&counts, a.parts)?;
    anyhow::ensure!(
        !a.out_dir.exists(),
        "каталог уже существует: {}",
        a.out_dir.display()
    );
    std::fs::create_dir_all(&a.out_dir)?;

    let mut cuts: Vec<usize> = vec![0];
    cuts.extend(bounds.iter().map(|b| lines[*b].0.start));
    cuts.push(bytes.len());

    let mut manifest = Vec::new();
    let mut rebuilt: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut written_chunks = 0usize;
    for k in 0..a.parts {
        let (s, e) = (cuts[k], cuts[k + 1]);
        let name = format!("{}_{:02}.txt", a.prefix, k + 1);
        let path = a.out_dir.join(&name);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("создать {}", path.display()))?;
        f.write_all(&bytes[s..e])?;
        drop(f);
        rebuilt.extend_from_slice(&bytes[s..e]);
        // чанки части = сумма по её строкам (чанкер не склеивает абзацы)
        let line_lo = if k == 0 { 0 } else { bounds[k - 1] };
        let line_hi = bounds.get(k).copied().unwrap_or(lines.len());
        let part_chunks: usize = counts[line_lo..line_hi].iter().sum();
        written_chunks += part_chunks;
        manifest.push(serde_json::json!({
            "part": k + 1,
            "name": name,
            "path": path,
            "bytes": e - s,
            "lines": line_hi - line_lo,
            "chunks": part_chunks,
            "first_line": line_lo + 1,
            "last_line": line_hi,
        }));
        println!(
            "{name}: {} байт, строк {}..{} ({}), чанков {}",
            e - s,
            line_lo + 1,
            line_hi,
            line_hi - line_lo,
            part_chunks
        );
    }
    anyhow::ensure!(
        rebuilt == bytes,
        "конкатенация частей НЕ совпала с исходником ({} против {} байт)",
        rebuilt.len(),
        bytes.len()
    );
    anyhow::ensure!(
        written_chunks == total,
        "чанков в частях {written_chunks} != {total} в исходнике"
    );
    let doc = serde_json::json!({
        "source": a.text_file,
        "source_bytes": bytes.len(),
        "max_tokens": a.max_tokens,
        "chunks_total": total,
        "parts": a.parts,
        "concat_equals_source": true,
        "chunks_sum": written_chunks,
        "manifest": manifest,
    });
    let mpath = a.out_dir.join("parts_manifest.json");
    std::fs::write(&mpath, serde_json::to_string_pretty(&doc)?)?;
    println!(
        "ПРОВЕРКА: конкатенация == исходник побайтово ({} байт); чанков {} = {}",
        bytes.len(),
        written_chunks,
        total
    );
    Ok(())
}

fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let res = match &cli.cmd {
        Cmd::Analyze(a) => run_analyze(a),
        Cmd::Split(a) => run_split(a),
        Cmd::Pool(a) => run_pool(a),
        Cmd::Process(a) => run_process(a),
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

    #[test]
    fn line_spans_cover_bytes_exactly() {
        let src = b"ab\n\ncd\r\nef";
        let spans = line_spans(src);
        assert_eq!(spans.len(), 4);
        assert_eq!((spans[0].start, spans[0].end), (0, 3));
        assert_eq!((spans[1].start, spans[1].end), (3, 4));
        assert_eq!(
            (spans[2].start, spans[2].end),
            (4, 8),
            "CRLF входит целиком"
        );
        assert_eq!((spans[3].start, spans[3].end), (8, 10));
        let mut p = 0;
        for s in &spans {
            assert_eq!(s.start, p);
            p = s.end;
        }
        assert_eq!(p, src.len());
    }

    #[test]
    fn lone_cr_is_a_line_break() {
        let src = b"a\rb\rc";
        let spans = line_spans(src);
        assert_eq!(spans.len(), 3);
        assert_eq!(line_content(src, &spans[0]), b"a");
        assert_eq!(line_content(src, &spans[1]), b"b");
        assert_eq!(line_content(src, &spans[2]), b"c");
    }

    #[test]
    fn line_content_strips_only_line_ending() {
        let src = b"  a  \n\nb\r\nc";
        let spans = line_spans(src);
        assert_eq!(line_content(src, &spans[0]), b"  a  ");
        assert_eq!(line_content(src, &spans[1]), b"");
        assert_eq!(line_content(src, &spans[2]), b"b");
        assert_eq!(line_content(src, &spans[3]), b"c");
    }

    #[test]
    fn boundaries_balance_chunks_and_stay_inside() {
        let counts = vec![1, 1, 0, 1, 1, 0, 1, 1, 0, 1, 1, 0, 1, 1];
        let b = choose_boundaries(&counts, 3).unwrap();
        assert_eq!(b.len(), 2);
        assert!(b[0] > 0 && b[0] < b[1] && b[1] < counts.len());
        let mut cum = vec![0];
        for c in &counts {
            cum.push(cum.last().unwrap() + c);
        }
        assert!((cum[b[0]] as i64 - 3).abs() <= 1, "{}", cum[b[0]]);
        assert!((cum[b[1]] as i64 - 6).abs() <= 1, "{}", cum[b[1]]);
    }

    #[test]
    fn boundaries_prefer_clean_paragraph_breaks() {
        // при РАВНОМ отклонении (по 2 чанка до границы) побеждает разрыв
        // абзацев: b=2 даёт «грязную» границу внутри абзаца, b=3 — чистую
        let counts = vec![1, 1, 0, 1, 1];
        let b = choose_boundaries(&counts, 2).unwrap();
        assert_eq!(b[0], 3, "граница после пустой строки");
    }

    #[test]
    fn eos_tail_parse_none_is_not_zero() {
        assert_eq!(parse_eos_tail("none").unwrap(), EosTail::Default);
        assert_eq!(parse_eos_tail("0").unwrap(), EosTail::Fixed(0));
        assert_ne!(
            parse_eos_tail("none").unwrap(),
            parse_eos_tail("0").unwrap()
        );
        assert!(parse_eos_tail("11").is_err());
    }

    #[test]
    fn chunk_counts_match_full_text_chunking() {
        let text =
            "Первый абзац. Второй предлог, и ещё.\n\nВторой абзац: короче.\nТретий абзац без точки";
        let per_line: usize = line_chunk_counts(text.as_bytes(), 10)
            .iter()
            .map(|(_, c, _)| *c)
            .sum();
        assert_eq!(per_line, chunk_pipeline(text, 10).len());
    }
}
