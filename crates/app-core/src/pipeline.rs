//! A3/A4/A5: конвейер синтеза (пул воркеров A4, потоковый вывод A5).
//!
//! Схема: chunk (A1, comma-semantics) → char_rules (A2) → Shtorm say_chunk
//! (seed+index, EOS/анти-повторы движка не меняются) → brightness →
//! denoise (опционально; при отсутствии модели — явный статус Missing) →
//! trim/fade → пауза chunk_pause_s (приоритет конца абзаца над soft_break) →
//! AudioSink (WAV напрямую / ffmpeg stdin) строго по порядку.
//! Последний чанк файла — без вставки паузы. Полное аудио в памяти НЕ
//! накапливается: буферы ограничены окном воркеров и каналом.

use anyhow::Result;
use std::path::PathBuf;

use shtorm_engine::{Device, POCKET_RATE};
use text_processing::{apply_char_rules, CharRules};

/// Полные параметры прогона (зеркалируют SynthSettings + пути).
#[derive(Debug, Clone)]
pub struct PipelineOptions {
    pub model_dir: PathBuf,
    pub voice: PathBuf,
    pub device: Device,
    pub threads: usize,
    /// Фиксированный seed (None — случайный на запуск).
    pub seed: Option<u64>,
    pub temperature: f32,
    pub decode_steps: usize,
    pub eos_threshold: f32,
    /// дБ high-shelf > 3 кГц; <= 0.05 — выключен.
    pub brightness_db: f32,
    /// Включить DPDFNet; если файла модели нет — прогон без очистки
    /// и с явным сообщением (denoise_used = false).
    pub denoise: bool,
    pub pause_scale: f32,
    pub paragraph_pause_s: f32,
    /// Ползунок «Макс. токенов» GUI (задаёт байтовый лимит чанка).
    pub max_tokens: u32,
    /// char_rules из runtime.json модели (если заданы).
    pub char_rules: Option<CharRules>,
    /// Защита от повторов (двухпопытная схема Pocket). GUI-переключатель
    /// передаётся в движок реально (G1).
    pub anti_repeat: bool,
}

/// Итоговая сводка по одному чанку.
#[derive(Debug, Clone)]
pub struct ChunkOutcome {
    pub index: usize,
    /// Текст чанка после A1 (до char_rules — паузы считаются по нему,
    /// как в Python compute_pauses_for_gluing).
    pub text: String,
    /// Пауза вставки после чанка; None — последний чанк файла.
    pub pause_s: Option<f32>,
    /// Сэмплов речи (после DSP, без паузы).
    pub speech_samples: usize,
}

#[derive(Debug)]
pub struct PipelineOutcome {
    pub sample_rate: u32,
    pub total_samples: usize,
    pub speech_samples: usize,
    pub pause_samples: usize,
    pub chunks: Vec<ChunkOutcome>,
    /// Фактический учёт DPDFNet (см. DenoiseReport).
    pub denoise_report: DenoiseReport,
}

/// Учёт очистки по факту применения (A4): Applied — только успешный
/// инференс; ошибки загрузки/инференса считаются отдельно.
#[derive(Debug, Default, Clone)]
pub struct DenoiseReport {
    pub applied: usize,
    pub off: usize,
    pub missing: usize,
    pub failed: usize,
    pub first_error: Option<String>,
}

/// Денойз в параллельном пути: если очистка запрошена, но файла модели нет,
/// каждый чанк должен получить явный статус Missing (не молчаливый Off).
/// Чистая функция — тестируется без ONNX.
pub fn denoise_precheck(
    requested: bool,
    model_present: bool,
) -> Option<crate::pool::DenoiseChunkStatus> {
    (requested && !model_present).then_some(crate::pool::DenoiseChunkStatus::Missing)
}

/// Решение о включении DPDFNet на экземпляре движка: только если очистка
/// запрошена заданием И файл модели существует. Чистая функция (тестируется
/// без модели); фактическое применение всё равно подтверждается per-chunk
/// CleanOutcome::Applied.
pub fn resolve_denoise(requested: bool, model_file_exists: bool) -> bool {
    requested && model_file_exists
}

impl DenoiseReport {
    /// Человекочитаемая сводка для лога/CLI.
    pub fn summary(&self) -> String {
        match (&self.first_error, self.missing) {
            (Some(e), _) => format!(
                "применён к {} чанк(ам); сбои: {} (первая: {})",
                self.applied, self.failed, e
            ),
            (None, m) if m > 0 => format!("модель отсутствует ({m} чанк(ов) без очистки)"),
            (None, _) if self.applied > 0 => format!("применён к {} чанк(ам)", self.applied),
            (None, _) => "выключен".into(),
        }
    }
}

/// Собрать итоговый трек из `(речь чанка, пауза после)`. Порядок строгий,
/// пауза последнего чанка не вставляется (None игнорируется). Выделено для
/// юнит-тестов порядка/дублирования без движка.
pub fn assemble(parts: &[(Vec<f32>, Option<f32>)], fs: u32) -> Vec<f32> {
    let mut out = Vec::new();
    let last = parts.len().saturating_sub(1);
    for (i, (audio, pause)) in parts.iter().enumerate() {
        out.extend_from_slice(audio);
        // после последнего чанка файла паузы не бывает — даже если пауза
        // ошибочно передана Some (защитный инвариант конвейера)
        if i != last {
            if let Some(p) = pause {
                out.extend(audio_processing::silence(*p, fs));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use text_processing::{chunk_pause_s, chunk_pipeline};

    fn marker_parts() -> Vec<(Vec<f32>, Option<f32>)> {
        // чанк k — k сэмплов со значением k+1 (маркер порядка/потерь/дублей)
        vec![
            (vec![1.0], Some(0.5)),
            (vec![2.0, 2.0], None),
            (vec![3.0], Some(0.0)),
            (vec![4.0, 4.0, 4.0], None), // последний: пауза не вставляется
        ]
    }

    #[test]
    fn assemble_order_no_duplication_no_trailing_pause() {
        let fs = 24000u32;
        let got = assemble(&marker_parts(), fs);
        // ожидаемая раскладка: [1.0] + 12000 нулей + [2.0,2.0] + [3.0] + [4.0,4.0,4.0]
        assert_eq!(got.len(), 7 + 12000, "длина: речь 7 + пауза 0.5с");
        assert_eq!(got[0], 1.0);
        assert!(got[1..1 + 12000].iter().all(|&v| v == 0.0), "пауза — нули");
        assert_eq!(&got[1 + 12000..3 + 12000], &[2.0, 2.0]);
        assert_eq!(got[3 + 12000], 3.0);
        assert_eq!(&got[4 + 12000..7 + 12000], &[4.0, 4.0, 4.0]);
        // маркеры в точном порядке, без дублей/потерь
        let markers: Vec<f32> = got.iter().copied().filter(|&v| v != 0.0).collect();
        assert_eq!(markers, vec![1.0, 2.0, 2.0, 3.0, 4.0, 4.0, 4.0]);
        // после последнего чанка никакой паузы (хватило бы длины)
    }

    #[test]
    fn assemble_last_pause_ignored_even_if_some() {
        // пауза НЕпоследней части вставляется (10 с), последней — игнорируется
        let parts = vec![(vec![1.0, 1.0], Some(10.0)), (vec![2.0], Some(5.0))];
        let got = assemble(&parts, 24000);
        assert_eq!(
            got.len(),
            2 + 240000 + 1,
            "10с пауза первой части без паузы последней"
        );
        assert_eq!(&got[..2], &[1.0, 1.0]);
        assert!(got[2..2 + 240000].iter().all(|&v| v == 0.0));
        assert_eq!(got[2 + 240000], 2.0);
    }

    #[test]
    fn golden_pauses_match_python_with_documented_divergence() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/golden/pauses_golden.json"
        );
        let raw = std::fs::read_to_string(path).expect(
            "golden-файл pauses_golden.json отсутствует: запустите tools/gen_pauses_golden.py",
        );
        let cases: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("golden json");
        let mut checked = 0usize;
        let mut divergent_checked = 0usize;
        for case in &cases {
            let name = case["name"].as_str().unwrap_or("?");
            let text = case["text"].as_str().unwrap();
            let max_tokens = case["max_tokens"].as_u64().unwrap() as u32;
            let scale = case["pause_scale"].as_f64().unwrap() as f32;
            let para = case["paragraph_pause_s"].as_f64().unwrap() as f32;
            let fs = case["fs"].as_u64().unwrap() as u32;

            // чанки нашего конвейера обязаны совпасть с golden-чанками
            let chunks = chunk_pipeline(text, max_tokens);
            let exp: Vec<serde_json::Value> = case["chunks"].as_array().unwrap().clone();
            assert_eq!(chunks.len(), exp.len(), "{name}: число чанков");
            for (i, c) in chunks.iter().enumerate() {
                assert_eq!(
                    c.text,
                    exp[i]["text"].as_str().unwrap(),
                    "{name}[{i}] текст"
                );
                assert_eq!(
                    c.is_last,
                    exp[i]["is_last"].as_bool().unwrap(),
                    "{name}[{i}] is_last"
                );
                assert_eq!(
                    c.soft_break,
                    exp[i]["soft_break"].as_bool().unwrap(),
                    "{name}[{i}] soft"
                );
            }

            // паузы: наш расчёт == rust-эталон; в не-расходящихся узлах он же
            // совпадает с Python (расхождение только задокументированное)
            let div_idx: Vec<usize> = case["divergent"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["i"].as_u64().unwrap() as usize)
                .collect();
            let py: Vec<Option<f64>> = case["python_pauses"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64())
                .collect();
            let ru: Vec<Option<f64>> = case["rust_pauses"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64())
                .collect();
            assert_eq!(chunks.len(), ru.len(), "{name}: размерность пауз");
            for i in 0..chunks.len() {
                let is_last_global = i + 1 == chunks.len();
                let got = chunk_pause_s(&chunks[i], is_last_global, para, scale)
                    .map(|x| (x as f64 * 1e6).round() / 1e6);
                let want = ru[i].map(|x| (x * 1e6).round() / 1e6);
                assert_eq!(got, want, "{name}[{i}]: rust-эталон");
                let py_v = py[i].map(|x| (x * 1e6).round() / 1e6);
                if !div_idx.contains(&i) {
                    assert_eq!(
                        got, py_v,
                        "{name}[{i}]: узел не расходящийся — обязан совпасть с Python"
                    );
                } else {
                    divergent_checked += 1;
                    assert_ne!(
                        got, py_v,
                        "{name}[{i}]: расходящийся узел обязан отличаться (is_last > soft_break)"
                    );
                }
                // сэмплы паузы (как вставляет assemble)
                if let (Some(g), Some(w)) = (got, ru[i]) {
                    let n_g = (g * fs as f64) as usize;
                    let n_w = case["rust_pause_samples"].as_array().unwrap()[i]
                        .as_u64()
                        .unwrap() as usize;
                    assert_eq!(n_g, n_w, "{name}[{i}]: сэмплы паузы");
                    let _ = w;
                }
            }
            checked += 1;
        }
        assert!(checked >= 20, "golden мал: {checked}");
        assert!(
            divergent_checked >= 1,
            "должен быть хоть один расходящийся узел"
        );
        eprintln!("golden pauses: {checked} cases OK (divergent узлов: {divergent_checked})");
    }
}

#[cfg(test)]
mod denoise_tests {
    use super::*;

    /// Решение о включении очистки на экземпляре (A4): чистая функция —
    /// регрессионный тест без настоящей модели DPDFNet.
    #[test]
    fn resolve_denoise_decision() {
        // не запрошено -> всегда выключено, независимо от файла
        assert!(!resolve_denoise(false, true));
        assert!(!resolve_denoise(false, false));
        // запрошено и модель есть -> включаем
        assert!(resolve_denoise(true, true));
        // запрошено, модели нет -> НЕ включаем (прогон без очистки + сообщение)
        assert!(!resolve_denoise(true, false));
    }

    #[test]
    fn denoise_precheck_flags_missing_model() {
        use crate::pool::DenoiseChunkStatus;
        assert_eq!(
            denoise_precheck(true, false),
            Some(DenoiseChunkStatus::Missing)
        );
        assert_eq!(denoise_precheck(true, true), None);
        assert_eq!(denoise_precheck(false, false), None);
        assert_eq!(denoise_precheck(false, true), None);
    }

    #[test]
    fn report_summary_branches() {
        // успех: только применения
        let r = DenoiseReport {
            applied: 3,
            ..Default::default()
        };
        assert_eq!(r.summary(), "применён к 3 чанк(ам)");
        // модель отсутствует
        let r = DenoiseReport {
            missing: 2,
            ..Default::default()
        };
        assert_eq!(r.summary(), "модель отсутствует (2 чанк(ов) без очистки)");
        // сбой: текст первой ошибки приоритетен
        let r = DenoiseReport {
            applied: 1,
            failed: 2,
            first_error: Some("нет DLL".into()),
            ..Default::default()
        };
        assert!(r.summary().contains("сбои: 2") && r.summary().contains("нет DLL"));
        // выключен
        assert_eq!(DenoiseReport::default().summary(), "выключен");
    }
}

// ============================ A5: потоковый конвейер ============================

use crate::pool::{run_job, ChunkTiming, DenoiseChunkStatus, PoolConfig, SeedPolicy, Task};
use crate::sink::AudioSink;
use crate::worker::real_worker_factory;
use std::sync::atomic::AtomicBool;

/// Однозначный статус задания (A4-фикс): неполный результат не публикуется.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Cancelled,
    Failed,
}

/// Ошибка потокового задания с контекстом для диагностики/журнала:
/// сохраняется исходный текст, путь черновика и точный прогресс.
#[derive(Debug)]
pub struct StreamError {
    pub message: String,
    pub partial: PathBuf,
    pub chunks_done: usize,
    pub total: usize,
    pub base_seed: u64,
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (обработано {}/{}, base_seed {}, черновик: {})",
            self.message,
            self.chunks_done,
            self.total,
            self.base_seed,
            self.partial.display()
        )
    }
}
impl std::error::Error for StreamError {}

/// Обёртка ошибки пула в StreamError (выделено для регрессионного теста
/// мок-сбоя без ONNX).
pub fn stream_error_from_run(
    e: anyhow::Error,
    chunks_done: usize,
    total: usize,
    base_seed: u64,
    partial: PathBuf,
) -> StreamError {
    StreamError {
        message: format!("{e:#}"),
        partial,
        chunks_done,
        total,
        base_seed,
    }
}

/// Метаданные диагностического журнала (из StreamError ИЛИ StreamOutcome).
#[derive(Debug, Clone)]
pub struct DiagMeta {
    pub chunks_done: usize,
    pub total: usize,
    pub base_seed: u64,
    pub partial: Option<PathBuf>,
}

impl DiagMeta {
    pub fn from_error(e: &StreamError) -> Self {
        Self {
            chunks_done: e.chunks_done,
            total: e.total,
            base_seed: e.base_seed,
            partial: Some(e.partial.clone()),
        }
    }
    pub fn from_outcome(o: &StreamOutcome) -> Self {
        Self {
            chunks_done: o.chunks_done,
            total: o.total_chunks,
            base_seed: o.base_seed,
            partial: Some(o.partial_path.clone()),
        }
    }
}

/// Диагностический журнал отмены/ошибки: эксклюзивное уникальное имя
/// (`{stem}.{нс}.{pid}.diagnostic_log.txt`, create_new, без удалений).
/// Сбой создания не маскирует основной статус (None + предупреждение).
pub fn write_diag_log(
    out: &std::path::Path,
    status: &str,
    details: &str,
    meta: &DiagMeta,
) -> Option<PathBuf> {
    use std::io::Write;
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = out.with_file_name(format!(
        "{stem}.{nanos}.{}.diagnostic_log.txt",
        std::process::id()
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut w) => {
            let _ = writeln!(w, "Диагностический журнал задания");
            let _ = writeln!(w, "Статус: {status}");
            let _ = writeln!(w, "Детали: {details}");
            let _ = writeln!(
                w,
                "Прогресс: {}/{} чанков; base_seed {}",
                meta.chunks_done, meta.total, meta.base_seed
            );
            if let Some(p) = &meta.partial {
                let _ = writeln!(w, "Черновик: {}", p.display());
            }
            Some(path)
        }
        Err(e) => {
            eprintln!(
                "примечание: не удалось создать диагностический журнал {}: {e}",
                path.display()
            );
            None
        }
    }
}

/// Строка журнала по одному чанку (метаданные, не аудио).
#[derive(Debug, Clone)]
pub struct ChunkLogRow {
    pub index: usize,
    pub text: String,
    pub worker_id: Option<usize>,
    pub ms: Option<u128>,
    pub denoise: DenoiseChunkStatus,
    pub speech_samples: usize,
    pub pause_s: Option<f32>,
}

/// Итог потокового прогона: пути/статус/метрики, без аудио в памяти.
#[derive(Debug)]
pub struct StreamOutcome {
    pub status: RunStatus,
    /// Итоговый файл (только при Completed).
    pub path: Option<PathBuf>,
    /// Черновик (диагностика при отмене/ошибке; при Completed уже убран).
    pub partial_path: PathBuf,
    pub workers: usize,
    pub base_seed: u64,
    pub max_buffered: usize,
    pub max_buffered_bytes: usize,
    pub chunks_done: usize,
    pub total_chunks: usize,
    pub sample_rate: u32,
    pub speech_samples: u64,
    pub pause_samples: u64,
    pub denoise_report: DenoiseReport,
    pub chunks: Vec<ChunkLogRow>,
}

/// Потоковый конвейер A5: чанки пишутся в `sink` строго по порядку, полное
/// аудио в памяти НЕ накапливается (буферы ограничены окном воркеров).
/// Тот же расчёт пауз/seed, что в A3/A4; при отмене итог не публикуется.
pub fn synthesize_stream<F>(
    raw_text: &str,
    opts: &PipelineOptions,
    workers: usize,
    sink: Box<dyn AudioSink>,
    mut progress: F,
    cancel: &AtomicBool,
) -> Result<StreamOutcome>
where
    F: FnMut(usize, usize),
{
    use text_processing::{chunk_pause_s, chunk_pipeline, to_model_stress_notation};

    let chunks = chunk_pipeline(raw_text, opts.max_tokens);
    anyhow::ensure!(!chunks.is_empty(), "текст пуст — нечего синтезировать");
    let total = chunks.len();
    let fs = POCKET_RATE;
    let partial_path = sink.partial_path();

    let base_seed = match opts.seed {
        Some(s) => s,
        None => SeedPolicy::Random.resolve(),
    };

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

    let cfg = PoolConfig {
        workers,
        queue_cap: 4,
    };
    let factory = real_worker_factory(opts, base_seed);

    let mut done_ref = 0usize;
    let mut sink = sink;
    let run = run_job(
        &cfg,
        tasks.clone(),
        factory,
        |i, pcm, pause| {
            sink.write_chunk(i, pcm, pause)?;
            done_ref += 1;
            progress(done_ref, total);
            Ok(())
        },
        cancel,
        SeedPolicy::Fixed(base_seed),
    );

    let summary = match run {
        Ok(s) => s,
        Err(e) => {
            // черновик осознанно остаётся для диагностики (без удалений)
            return Err(anyhow::anyhow!(
                "{e:#}\nчерновик сохранён: {}",
                partial_path.display()
            ));
        }
    };

    // метаданные журнала: текст/пауза из задач, воркер/время/денойз из timings
    let mut by_index: std::collections::HashMap<usize, ChunkTiming> = summary
        .timings
        .iter()
        .cloned()
        .map(|t| (t.index, t))
        .collect();
    let rows: Vec<ChunkLogRow> = chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let t = by_index.remove(&i);
            ChunkLogRow {
                index: i,
                text: c.text.clone(),
                worker_id: t.as_ref().map(|t| t.worker_id),
                ms: t.as_ref().map(|t| t.ms),
                denoise: t
                    .as_ref()
                    .map(|t| t.denoise.clone())
                    .unwrap_or(DenoiseChunkStatus::Off),
                speech_samples: t.as_ref().map(|t| t.samples).unwrap_or(0),
                pause_s: tasks[i].pause_s,
            }
        })
        .collect();
    let speech_samples: u64 = rows.iter().map(|r| r.speech_samples as u64).sum();
    let pause_samples: u64 = rows
        .iter()
        .map(|r| {
            r.pause_s
                .map(|p| (p.max(0.0) * fs as f32) as u64)
                .unwrap_or(0)
        })
        .sum();

    let status = if summary.cancelled {
        RunStatus::Cancelled
    } else if summary.chunks_done == total {
        RunStatus::Completed
    } else {
        RunStatus::Failed
    };

    let (path, final_partial) = match status {
        RunStatus::Completed => {
            let published = sink.finalize().map_err(|e| {
                anyhow::Error::new(StreamError {
                    message: format!("не удалось завершить/опубликовать файл: {e:#}"),
                    partial: partial_path.clone(),
                    chunks_done: summary.chunks_done,
                    total,
                    base_seed,
                })
            })?;
            (Some(published), partial_path)
        }
        RunStatus::Cancelled => {
            // неполный результат не публикуется; черновик остаётся и сообщается
            (None, partial_path)
        }
        RunStatus::Failed => {
            return Err(anyhow::Error::new(StreamError {
                message: "синтез не завершён (без отмены) — файл не создан".into(),
                partial: partial_path.clone(),
                chunks_done: summary.chunks_done,
                total,
                base_seed,
            }));
        }
    };

    Ok(StreamOutcome {
        status,
        path,
        partial_path: final_partial,
        workers: summary.workers,
        base_seed,
        max_buffered: summary.max_buffered,
        max_buffered_bytes: summary.max_buffered_bytes,
        chunks_done: summary.chunks_done,
        total_chunks: total,
        sample_rate: fs,
        speech_samples,
        pause_samples,
        denoise_report: summary.denoise,
        chunks: rows,
    })
}

#[cfg(test)]
mod stream_error_tests {
    use super::*;
    use crate::pool::{run_job, ChunkWorker, PoolConfig, SeedPolicy, Task};
    use std::sync::atomic::AtomicBool;

    struct FailingOnThird;

    impl ChunkWorker for FailingOnThird {
        fn process(&mut self, task: &Task) -> anyhow::Result<crate::pool::ChunkAudio> {
            if task.index == 2 {
                anyhow::bail!("синтетический сбой чанка 3");
            }
            Ok(crate::pool::ChunkAudio {
                speech: vec![task.index as f32],
                pause_s: task.pause_s,
                denoise: crate::pool::DenoiseChunkStatus::Off,
            })
        }
    }

    /// Мок-воркер падает на 3-м чанке: StreamError несёт реальный прогресс,
    /// диагностический журнал содержит ошибку/прогресс/seed/черновик,
    /// готовый файл отсутствует.
    #[test]
    fn mock_failure_produces_diag_log_with_metadata() {
        let d = std::env::temp_dir().join(format!(
            "shtorm_diag_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        let final_p = d.join("book.wav");
        let partial = d.join("book.partial.wav");

        let tasks: Vec<Task> = (0..5)
            .map(|i| Task {
                index: i,
                model_text: format!("т{i}"),
                pause_s: if i == 4 { None } else { Some(0.1) },
            })
            .collect();
        let cancel = AtomicBool::new(false);
        let mut done = 0usize;
        let err = run_job(
            &PoolConfig {
                workers: 1,
                queue_cap: 2,
            },
            tasks,
            |_, _| Ok(FailingOnThird),
            |_, _, _| {
                done += 1;
                Ok(())
            },
            &cancel,
            SeedPolicy::Fixed(42),
        )
        .unwrap_err();

        let se = stream_error_from_run(err, done, 5, 42, partial.clone());
        assert!(se.message.contains("синтетический сбой"), "{}", se.message);
        assert_eq!(se.total, 5);
        assert_eq!(se.base_seed, 42);
        // прогресс реальный: 2 чанка выдано до сбоя на 3-м
        assert_eq!(se.chunks_done, 2, "done_ref на момент сбоя");

        // диагностический журнал: эксклюзивное имя, все метаданные
        let diag = write_diag_log(
            &final_p,
            "Failed",
            &format!("{}\nпричина: ошибка выполнения задания", se.message),
            &DiagMeta::from_error(&se),
        )
        .expect("diag log создаётся");
        let text = std::fs::read_to_string(&diag).unwrap();
        assert!(text.contains("Failed"));
        assert!(text.contains("синтетический сбой"));
        assert!(text.contains("2/5 чанков"), "{text}");
        assert!(text.contains("base_seed 42"));
        assert!(text.contains("book.partial.wav"));
        // готовый файл отсутствует
        assert!(!final_p.exists());
    }
}
