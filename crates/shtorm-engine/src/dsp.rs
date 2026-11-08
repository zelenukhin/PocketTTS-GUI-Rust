//! Небольшой DSP-набор движка.
//!
//! ПРОИСХОЖДЕНИЕ: teratts-android (GPL-3.0), ветка `teratts`, commit edb614e,
//! `rust/src/tera.rs` (trim_edges, fade_out). Отличие от оригинала: частота
//! дискретизации — параметр (в tera.rs была зашита 44100 теры; движок Shtorm
//! работает на 24000, GUI передаёт sample rate модели).

/// Leaves only the speech with a little room around it: cut by the sound itself —
/// from [lead] s before the first audible 5 ms window to [tail] s after the last
/// (audible: within 45 dB of the peak, so the decay of the last word is kept).
/// A soft start over the first 5 ms: the cut may fall inside a quiet breath.
#[allow(clippy::needless_range_loop)] // индекс = позиция окна RMS
pub fn trim_edges(wav: &[f32], lead_s: f32, tail_s: f32, fs: u32) -> Vec<f32> {
    let hop = ((fs as f32 * 0.005) as usize).max(1); // 5 ms
    let n = wav.len() / hop;
    if n < 2 {
        return wav.to_vec();
    }
    let rms: Vec<f32> = (0..n)
        .map(|i| {
            let w = &wav[i * hop..(i + 1) * hop];
            (w.iter().map(|x| x * x).sum::<f32>() / hop as f32).sqrt()
        })
        .collect();
    let max = rms.iter().cloned().fold(0.0f32, f32::max);
    let thr = max * 10f32.powf(-45.0 / 20.0);
    let (Some(first), Some(last)) = (
        rms.iter().position(|&r| r > thr),
        rms.iter().rposition(|&r| r > thr),
    ) else {
        return wav.to_vec();
    };
    let start = (first * hop).saturating_sub((lead_s * fs as f32) as usize);
    let stop = ((last + 1) * hop + (tail_s * fs as f32) as usize).min(wav.len());
    let mut out = wav[start..stop].to_vec();
    // a soft start: the cut may fall inside a quiet breath
    let f = out.len().min(hop);
    for i in 0..f {
        out[i] *= i as f32 / f as f32;
    }
    out
}

/// Linear fade over the last 10 ms: a cut inside sound does not click.
/// (Формула — как в tera.rs teratts-android: фактор (n-i)/n.)
pub fn fade_out(wav: &mut [f32], fs: u32) {
    let n = wav.len().min((fs as f32 * 0.01) as usize).max(1);
    let len = wav.len();
    for i in 0..n {
        wav[len - n + i] *= (n - i) as f32 / n as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_silence_around_speech() {
        let fs = 24_000u32;
        // 1 с тишины, 1 с тона, 1 с тишины
        let mut wav = vec![0f32; fs as usize];
        wav.extend(
            (0..fs)
                .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / fs as f32).sin()),
        );
        wav.extend(vec![0f32; fs as usize]);
        let out = trim_edges(&wav, 0.02, 0.10, fs);
        // тон начинается на 1.00 с; lead 0.02 → старт ~0.98 с; конец ~2.10 с
        let len = out.len() as f32 / fs as f32;
        assert!((len - 1.12).abs() < 0.05, "len {len}");
    }

    #[test]
    fn fade_ends_near_silence() {
        // как в tera.rs: последний сэмпл остаётся на 1/n от исходного (не строго 0)
        let fs = 24_000u32;
        let mut wav = vec![0.5f32; fs as usize / 2];
        fade_out(&mut wav, fs);
        let n = (fs as f32 * 0.01) as usize;
        assert!(wav.last().unwrap().abs() <= 0.5 / n as f32 + 1e-6);
        assert_eq!(*wav.first().unwrap(), 0.5);
    }
}
