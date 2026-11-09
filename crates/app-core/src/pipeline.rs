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

use shtorm_engine::{Device, EosTail, POCKET_RATE};
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
    /// Хвост EOS для чанков ≤4 слов (G4.1; передаётся движку напрямую,
    /// без пересчёта — семантика eos_extra teratts-android).
    pub eos_tail_short: EosTail,
    /// Хвост EOS для чанков ≥5 слов (G4.1).
    pub eos_tail_long: EosTail,
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

/// G5.1: фаза выполнения задания. `Generating` — синтез чанков воркерами;
/// `Finalizing` — дожатие энкодера (ffmpeg) / закрытие контейнера БЕЗ
/// публикации; `Publishing` — необратимая публикация (перенос/замена).
/// Точка принятия решения о публикации — между `Generating` и `Finalizing`:
/// отмена, принятая до неё, предотвращает публикацию; после неё конвейер
/// доходит до конца независимо от флага (UI к этому моменту знает фазу и
/// не обещает, что «Стоп» отменит замену).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    Generating,
    Finalizing,
    Publishing,
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
    /// G5.1: сбой случился уже НА ПУБЛИКАЦИИ (кодирование прошло успешно):
    /// старый готовый файл не изменён, черновик цел.
    pub publish_failed: bool,
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
        publish_failed: false,
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

/// `{out-stem}_log.txt` рядом с файлом (как Python GUI `{имя}_log.txt`).
pub fn log_path_for(out: &std::path::Path) -> PathBuf {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    out.with_file_name(format!("{stem}_log.txt"))
}

/// Метаданные прогона для заголовка журнала чанков (CLI и GUI пишут
/// одно и то же).
#[derive(Debug, Clone)]
pub struct ChunkLogMeta {
    /// Устройство как в CLI ("cpu" / "cuda" / "cuda 1").
    pub device: String,
    pub format: String,
    pub preset: String,
}

/// Файл журнала чанков (create_new — не перезаписываем чужие файлы);
/// сбой журнала НЕ маскирует успешное аудио (ошибка возвращается вызывающему,
/// который решает, как её показать).
pub fn write_chunk_log(
    path: &std::path::Path,
    o: &StreamOutcome,
    meta: &ChunkLogMeta,
) -> Result<()> {
    use std::io::Write;
    let mut w = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| anyhow::anyhow!("создать {}: {e}", path.display()))?;
    writeln!(w, "Лог чанков")?;
    writeln!(
        w,
        "Устройство: {}; воркеров: {}; base_seed: {}",
        meta.device, o.workers, o.base_seed
    )?;
    writeln!(
        w,
        "Формат: {} ({}); чанков: {}",
        meta.format,
        meta.preset,
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
/// `phase` уведомляет о фазах (Generating/Finalizing/Publishing) — UI
/// использует это, чтобы честно отражать отмену после точки решения.
pub fn synthesize_stream<F>(
    raw_text: &str,
    opts: &PipelineOptions,
    workers: usize,
    sink: Box<dyn AudioSink>,
    progress: F,
    cancel: &AtomicBool,
    phase: &dyn Fn(RunPhase),
) -> Result<StreamOutcome>
where
    F: FnMut(usize, usize),
{
    synthesize_stream_impl(
        raw_text,
        opts,
        workers,
        sink,
        progress,
        cancel,
        phase,
        |seed| real_worker_factory(opts, seed),
    )
}

/// Реализация с инъекцией фабрики воркеров: публичная обёртка всегда берёт
/// `real_worker_factory`; тесты подставляют мок и проверяют аварийный путь
/// целиком (run_job → StreamError с реальным прогрессом/seed/черновиком).
#[allow(clippy::too_many_arguments)]
fn synthesize_stream_impl<W, Fac, FacW, F>(
    raw_text: &str,
    opts: &PipelineOptions,
    workers: usize,
    sink: Box<dyn AudioSink>,
    mut progress: F,
    cancel: &AtomicBool,
    phase: &dyn Fn(RunPhase),
    factory_of: Fac,
) -> Result<StreamOutcome>
where
    W: crate::pool::ChunkWorker + 'static,
    Fac: FnOnce(u64) -> FacW,
    FacW: Fn(usize, u64) -> anyhow::Result<W> + Send + Sync + 'static,
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
    let factory = factory_of(base_seed);

    phase(RunPhase::Generating);
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
            // черновик осознанно остаётся для диагностики (без удалений);
            // ошибка возвращается ТИПИЗИРОВАННОЙ: прогресс — это выданные в
            // sink чанки (done_ref), не 0 и не оценки
            let se = stream_error_from_run(e, done_ref, total, base_seed, partial_path.clone());
            return Err(anyhow::Error::new(se));
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

    let mut status = if summary.cancelled {
        RunStatus::Cancelled
    } else if summary.chunks_done == total {
        RunStatus::Completed
    } else {
        RunStatus::Failed
    };

    // G5.1: ОДНОЗНАЧНАЯ ТОЧКА ПРИНЯТИЯ РЕШЕНИЯ О ПУБЛИКАЦИИ.
    // Все чанки выданы; отмена, принятая ДО этого момента, предотвращает
    // публикацию и замену готового файла (статус понижается до Cancelled,
    // черновик остаётся). После этого решения конвейер финализирует и
    // публикует результат ДО КОНЦА независимо от флага: публикация
    // необратима, а UI через RunPhase знает, что «Стоп» больше не может
    // её предотвратить (кнопка отключается на фазах Finalizing/Publishing).
    if status == RunStatus::Completed && cancel.load(std::sync::atomic::Ordering::SeqCst) {
        status = RunStatus::Cancelled;
    }

    let (path, final_partial) = match status {
        RunStatus::Completed => {
            phase(RunPhase::Finalizing);
            let fin = sink.finish().map_err(|e| {
                anyhow::Error::new(StreamError {
                    message: format!("не удалось завершить кодирование: {e:#}"),
                    partial: partial_path.clone(),
                    chunks_done: summary.chunks_done,
                    total,
                    base_seed,
                    publish_failed: false,
                })
            })?;
            phase(RunPhase::Publishing);
            let published = fin.publish().map_err(|e| {
                anyhow::Error::new(StreamError {
                    message: format!("ошибка публикации: {e:#}"),
                    partial: partial_path.clone(),
                    chunks_done: summary.chunks_done,
                    total,
                    base_seed,
                    publish_failed: true,
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
                publish_failed: false,
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
    use crate::sink::WavSink;
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

    /// Аварийный путь сквозного конвейера (мок-воркер, настоящий WavSink):
    /// ошибка run_job возвращается как ТИПИЗИРОВАННЫЙ StreamError с реальным
    /// прогрессом (2 выданных чанка, не 0), seed и путём черновика; итоговый
    /// файл не создаётся (регрессия «anyhow без типа и прогресса»).
    #[test]
    fn synthesize_stream_failure_returns_typed_stream_error() {
        let d = std::env::temp_dir().join(format!(
            "shtorm_se2_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        let final_p = d.join("book.wav");
        let opts = PipelineOptions {
            model_dir: d.clone(),
            voice: d.join("voice.wav"),
            device: Device::Cpu,
            threads: 1,
            seed: Some(42),
            temperature: 0.3,
            decode_steps: 1,
            eos_threshold: -4.0,
            eos_tail_short: EosTail::Fixed(5),
            eos_tail_long: EosTail::Fixed(3),
            brightness_db: 0.0,
            denoise: false,
            pause_scale: 1.0,
            paragraph_pause_s: 0.0,
            max_tokens: 10, // короткие чанки — падение на 3-м гарантировано до конца
            char_rules: None,
            anti_repeat: true,
        };
        let text = "П+ервый предл+ог. Вт+орой предл+ог. Тр+етий предл+ог.";
        let sink = Box::new(WavSink::create(&final_p, 24_000).unwrap());
        let partial = sink.partial_path();
        let mut progress_calls = 0usize;
        let cancel = AtomicBool::new(false);
        let err = synthesize_stream_impl(
            text,
            &opts,
            1, // один воркер: выданы чанки 0,1, сбой на 2 — детерминированно
            sink,
            |done, total| {
                assert!(total >= 3);
                assert!(done >= 1 && done <= total);
                progress_calls += 1;
            },
            &cancel,
            &|_| {},
            |_seed| |_wid, _s| Ok(FailingOnThird),
        )
        .unwrap_err();

        let se = err
            .downcast_ref::<StreamError>()
            .expect("ошибка конвейера обязана быть типизированным StreamError");
        assert!(se.message.contains("синтетический сбой"), "{}", se.message);
        assert_eq!(
            se.chunks_done, 2,
            "реальный прогресс: 2 чанка выдано в sink"
        );
        assert!(se.total >= 3, "минимум 3 чанка в тестовом тексте");
        assert_eq!(se.base_seed, 42);
        assert_eq!(se.partial, partial, "путь черновика из синка");
        assert!(se.partial.exists(), "черновик WAV реально создан");
        assert!(!final_p.exists(), "итоговый файл не публикуется");
        assert!(
            format!("{se}").contains("2/"),
            "Display несёт прогресс: {se}"
        );
        assert!(progress_calls >= 2, "прогресс приходил до сбоя");
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

#[cfg(test)]
mod publish_phase_tests {
    //! G5.1: детерминированные тесты конкурентной отмены вокруг точки
    //! принятия решения о публикации. Порядок событий задаётся барьером и
    //! явными флагами — никаких sleep и случайных задержек.

    use super::*;
    use crate::pool::{ChunkWorker, Task};
    use crate::sink::{AudioSink, FinalizedSink};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    struct OkWorker;

    impl ChunkWorker for OkWorker {
        fn process(&mut self, task: &Task) -> anyhow::Result<crate::pool::ChunkAudio> {
            Ok(crate::pool::ChunkAudio {
                speech: vec![0.5f32; 240],
                pause_s: task.pause_s,
                denoise: crate::pool::DenoiseChunkStatus::Off,
            })
        }
    }

    /// Синк с управляемой публикацией: publish() сигнализирует о входе
    /// флагом и ждёт на барьере — тест выставляет cancel строго ВНУТРИ
    /// фазы Publishing.
    struct GatedSink {
        partial: PathBuf,
        final_path: PathBuf,
        publish_entered: Arc<AtomicBool>,
        gate: Arc<Barrier>,
        publish_done: Arc<AtomicBool>,
    }

    impl AudioSink for GatedSink {
        fn write_chunk(
            &mut self,
            _index: usize,
            _pcm: &[f32],
            _pause_s: Option<f32>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn finish(self: Box<Self>) -> anyhow::Result<Box<dyn FinalizedSink>> {
            Ok(self)
        }
        fn partial_path(&self) -> PathBuf {
            self.partial.clone()
        }
    }

    impl FinalizedSink for GatedSink {
        fn publish(self: Box<Self>) -> anyhow::Result<PathBuf> {
            self.publish_entered.store(true, Ordering::SeqCst);
            self.gate.wait(); // публикация удерживается до команды теста
            self.publish_done.store(true, Ordering::SeqCst);
            Ok(self.final_path)
        }
        fn partial_path(&self) -> PathBuf {
            self.partial.clone()
        }
    }

    /// Синк с мгновенной публикацией и флагом вызова: отмена до точки
    /// решения не должна вызвать publish вовсе.
    struct CountingSink {
        partial: PathBuf,
        final_path: PathBuf,
        publish_called: Arc<AtomicBool>,
    }

    impl AudioSink for CountingSink {
        fn write_chunk(
            &mut self,
            _index: usize,
            _pcm: &[f32],
            _pause_s: Option<f32>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn finish(self: Box<Self>) -> anyhow::Result<Box<dyn FinalizedSink>> {
            Ok(self)
        }
        fn partial_path(&self) -> PathBuf {
            self.partial.clone()
        }
    }

    impl FinalizedSink for CountingSink {
        fn publish(self: Box<Self>) -> anyhow::Result<PathBuf> {
            self.publish_called.store(true, Ordering::SeqCst);
            Ok(self.final_path)
        }
        fn partial_path(&self) -> PathBuf {
            self.partial.clone()
        }
    }

    fn opts_for(model_dir: &std::path::Path) -> PipelineOptions {
        PipelineOptions {
            model_dir: model_dir.to_path_buf(),
            voice: model_dir.join("voice.wav"),
            device: Device::Cpu,
            threads: 1,
            seed: Some(42),
            temperature: 0.3,
            decode_steps: 1,
            eos_threshold: -4.0,
            eos_tail_short: EosTail::Fixed(5),
            eos_tail_long: EosTail::Fixed(3),
            brightness_db: 0.0,
            denoise: false,
            pause_scale: 1.0,
            paragraph_pause_s: 0.0,
            max_tokens: 10,
            char_rules: None,
            anti_repeat: true,
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.subsec_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "shtorm_phase_{}_{}_{}",
            std::process::id(),
            tag,
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Ожидание явного флага с дедлайном: зависание превращается в понятный
    /// panic, а не в вечную блокировку теста.
    fn wait_flag(flag: &AtomicBool, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !flag.load(Ordering::SeqCst) {
            if std::time::Instant::now() > deadline {
                panic!("не дождались: {what} (30 с)");
            }
            std::thread::yield_now();
        }
    }

    const TEXT: &str = "П+ервый предл+ог. Вт+орой предл+ог. Тр+етий предл+ог.";

    /// Отмена, пришедшая ПОСЛЕ точки решения (строго внутри Publishing,
    /// что гарантируется барьером), не предотвращает публикацию — и статус
    /// остаётся Completed: задание не сообщает «Отменено» о замене, которая
    /// реально состоялась.
    #[test]
    fn cancel_during_publishing_does_not_prevent_replacement() {
        let d = tmpdir("during");
        let final_p = d.join("book.wav");
        std::fs::write(&final_p, b"old audio").unwrap();
        let entered = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Barrier::new(2));
        let sink = Box::new(GatedSink {
            partial: d.join("book.partial.wav"),
            final_path: final_p.clone(),
            publish_entered: Arc::clone(&entered),
            gate: Arc::clone(&gate),
            publish_done: Arc::clone(&done),
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_t = Arc::clone(&cancel);
        let phases = Arc::new(Mutex::new(Vec::new()));
        let phases_t = Arc::clone(&phases);
        let opts = opts_for(&d);
        let handle = std::thread::spawn(move || {
            synthesize_stream_impl(
                TEXT,
                &opts,
                1,
                sink,
                |_, _| {},
                &cancel_t,
                &move |p| phases_t.lock().unwrap().push(p),
                |_seed| |_wid, _s| Ok(OkWorker),
            )
        });
        // детерминированный порядок: публикация началась → «Стоп» → отпуск
        wait_flag(&entered, "публикация должна начаться");
        cancel.store(true, Ordering::SeqCst);
        gate.wait();
        let out = handle
            .join()
            .expect("поток задания")
            .expect("успешный прогон");
        assert_eq!(
            out.status,
            RunStatus::Completed,
            "отмена после точки решения не понижает статус"
        );
        assert!(out.path.is_some(), "итог опубликован");
        assert!(done.load(Ordering::SeqCst), "publish дошёл до конца");
        assert_eq!(
            phases.lock().unwrap().clone(),
            vec![
                RunPhase::Generating,
                RunPhase::Finalizing,
                RunPhase::Publishing
            ],
            "фазы идут в строгом порядке"
        );
    }

    /// Отмена во время генерации (после первого выданного чанка) — до точки
    /// решения: публикация не вызывается вовсе, статус Cancelled, старый
    /// файл не тронут.
    #[test]
    fn cancel_during_generating_prevents_publish_entirely() {
        let d = tmpdir("before");
        let final_p = d.join("book.wav");
        std::fs::write(&final_p, b"old audio").unwrap();
        let publish_called = Arc::new(AtomicBool::new(false));
        let sink = Box::new(CountingSink {
            partial: d.join("book.partial.wav"),
            final_path: final_p.clone(),
            publish_called: Arc::clone(&publish_called),
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_t = Arc::clone(&cancel);
        let phases = Arc::new(Mutex::new(Vec::new()));
        let phases_t = Arc::clone(&phases);
        let opts = opts_for(&d);
        let out = std::thread::spawn(move || {
            synthesize_stream_impl(
                TEXT,
                &opts,
                1,
                sink,
                |done, _total| {
                    if done >= 1 {
                        // «Стоп» после первого готового чанка — во время
                        // генерации, до точки решения о публикации
                        cancel_t.store(true, Ordering::SeqCst);
                    }
                },
                &cancel,
                &move |p| phases_t.lock().unwrap().push(p),
                |_seed| |_wid, _s| Ok(OkWorker),
            )
        })
        .join()
        .expect("поток задания")
        .expect("отмена — не ошибка");
        assert_eq!(out.status, RunStatus::Cancelled);
        assert!(out.path.is_none(), "итог не опубликован");
        assert!(
            !publish_called.load(Ordering::SeqCst),
            "publish не вызывался вовсе"
        );
        assert_eq!(
            phases.lock().unwrap().clone(),
            vec![RunPhase::Generating],
            "финализация/публикация не начинались"
        );
        assert_eq!(
            std::fs::read(&final_p).unwrap(),
            b"old audio",
            "старый файл не тронут"
        );
        assert!(out.chunks_done >= 1, "отмена после готового чанка");
    }

    /// Успешный прогон без отмены проходит все три фазы и публикует итог.
    #[test]
    fn phases_sequence_generating_finalizing_publishing() {
        let d = tmpdir("sequence");
        let final_p = d.join("book.wav");
        let publish_called = Arc::new(AtomicBool::new(false));
        let sink = Box::new(CountingSink {
            partial: d.join("book.partial.wav"),
            final_path: final_p.clone(),
            publish_called: Arc::clone(&publish_called),
        });
        let cancel = AtomicBool::new(false);
        let phases = Mutex::new(Vec::new());
        let opts = opts_for(&d);
        let out = synthesize_stream_impl(
            TEXT,
            &opts,
            1,
            sink,
            |_, _| {},
            &cancel,
            &|p| phases.lock().unwrap().push(p),
            |_seed| |_wid, _s| Ok(OkWorker),
        )
        .unwrap();
        assert_eq!(out.status, RunStatus::Completed);
        assert!(out.path.is_some());
        assert!(publish_called.load(Ordering::SeqCst));
        assert_eq!(
            phases.lock().unwrap().clone(),
            vec![
                RunPhase::Generating,
                RunPhase::Finalizing,
                RunPhase::Publishing
            ]
        );
    }
}
