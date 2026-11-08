//! A3: последовательный файловый конвейер синтеза (без GUI, без потоков —
//! воркеры в A4).
//!
//! Схема: chunk (A1, comma-semantics) → char_rules (A2) → Shtorm say_chunk
//! (seed+index, EOS/анти-повторы движка не меняются) → brightness →
//! denoise (опционально; при отсутствии модели — явное сообщение, без
//! имитации) → trim/fade → пауза chunk_pause_s (приоритет конца абзаца над
//! soft_break — задокументированное исправление) → сборка итогового аудио.
//! Последний чанк файла — без вставки паузы.

use anyhow::Result;
use std::path::{Path, PathBuf};

use shtorm_engine::denoise;
use shtorm_engine::dsp::{fade_out, trim_edges};
use shtorm_engine::{high_shelf, resample, Device, LoadOptions, Pocket, POCKET_RATE};
use text_processing::{
    apply_char_rules, chunk_pause_s, chunk_pipeline, to_model_stress_notation, CharRules,
};

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
    /// DPDFNet реально применялся.
    pub denoise_used: bool,
    /// DDPFNet был включён, но модель отсутствует (сообщение уже выведено).
    pub denoise_missing: bool,
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

/// Полный прогон: текст → один WAV (если задан out) + метаданные.
///
/// `progress(index, total)` вызывается перед каждым чанком; возврат `false` —
/// немедленная отмена с ошибкой (уже синтезированное НЕ записывается).
pub fn synthesize<F>(
    raw_text: &str,
    opts: &PipelineOptions,
    out: Option<&Path>,
    mut progress: F,
) -> Result<PipelineOutcome>
where
    F: FnMut(usize, usize) -> bool,
{
    // 1. Нарезка (A1) — конвейер ветки comma-semantics.
    let chunks = chunk_pipeline(raw_text, opts.max_tokens);
    anyhow::ensure!(!chunks.is_empty(), "текст пуст — нечего синтезировать");
    let total = chunks.len();

    // 2. Движок.
    let mut engine = Pocket::load(
        &opts.model_dir,
        &LoadOptions {
            device: opts.device.clone(),
            threads: opts.threads,
            seed: opts.seed,
            profile_dir: None,
        },
    )?;
    engine.temperature = opts.temperature;
    engine.decode_steps = opts.decode_steps;
    engine.eos_threshold = opts.eos_threshold;

    // 3. Denoise: включаем только при наличии файла модели (иначе явно
    //    сообщаем — успешную очистку не имитируем).
    let denoise_model = opts.model_dir.join(denoise::MODEL_FILE);
    let (denoise_used, denoise_missing) = if opts.denoise {
        if denoise_model.exists() {
            denoise::set_enabled(true);
            (true, false)
        } else {
            eprintln!(
                "ВНИМАНИЕ: денойз включён, но модель не найдена: {} — синтез без очистки звука.",
                denoise_model.display()
            );
            (false, true)
        }
    } else {
        (false, false)
    };

    let voice = opts.voice.to_string_lossy().to_string();
    let mut parts: Vec<(Vec<f32>, Option<f32>)> = Vec::with_capacity(total);
    let mut outcomes: Vec<ChunkOutcome> = Vec::with_capacity(total);
    let fs = POCKET_RATE;

    for (i, chunk) in chunks.iter().enumerate() {
        if !progress(i, total) {
            anyhow::bail!("отменено пользователем на чанке {}/{}", i + 1, total);
        }
        // текст модели: ударения (как GUI) + правила модели
        let mut model_text = to_model_stress_notation(&chunk.text);
        if let Some(rules) = &opts.char_rules {
            model_text = apply_char_rules(&model_text, rules);
        }
        let audio = engine.say_chunk(&model_text, &voice, i as u64)?;
        outcomes.push(ChunkOutcome {
            index: i,
            text: chunk.text.clone(),
            pause_s: None, // заполним ниже (нужно для assemble и отчёта)
            speech_samples: 0,
        });

        let mut piece = audio;
        if opts.brightness_db > 0.05 {
            high_shelf(&mut piece, opts.brightness_db, 3000.0, fs as f32);
        }
        if denoise_used {
            if let Some(clean) = engine.clean(&piece) {
                let back = resample(&clean, denoise::RATE, fs);
                piece = back[..back.len().min(piece.len())].to_vec();
            }
        }
        // тератт-режим GUI: тишина хвоста EOS срезается по звуку, фейд 10 мс
        let mut piece = trim_edges(&piece, 0.02, 0.10, fs);
        fade_out(&mut piece, fs);

        // пауза: по исходному чанку (не по model_text), приоритет конца абзаца
        let pause = chunk_pause_s(
            chunk,
            i + 1 == total,
            opts.paragraph_pause_s,
            opts.pause_scale,
        );
        let speech_samples = piece.len();
        outcomes[i].pause_s = pause;
        outcomes[i].speech_samples = speech_samples;
        parts.push((piece, pause));
    }

    let assembled = assemble(&parts, fs);
    let speech_samples: usize = parts.iter().map(|(a, _)| a.len()).sum();
    let pause_samples = assembled.len().saturating_sub(speech_samples);

    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        audio_processing::write_wav16(path, fs, &assembled)?;
    }

    Ok(PipelineOutcome {
        sample_rate: fs,
        total_samples: assembled.len(),
        speech_samples,
        pause_samples,
        chunks: outcomes,
        denoise_used,
        denoise_missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
