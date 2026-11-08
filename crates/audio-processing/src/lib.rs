//! Аудио-хелперы уровня приложения (WAV 16-bit, тишина, конкатенация).
//!
//! write_wav16 — порт `write_wav_file()` из synthesis_worker.py
//! (clip [-1..1] -> PCM16) на крейт hound.

use anyhow::Result;
use std::path::Path;

/// Записывает float-PCM [-1..1] в 16-битный моно WAV.
pub fn write_wav16(path: &Path, sample_rate: u32, audio: &[f32]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in audio {
        let v = s.clamp(-1.0, 1.0);
        w.write_sample((v * 32767.0) as i16)?;
    }
    w.finalize()?;
    Ok(())
}

/// Кадры тишины заданной длительности.
pub fn silence(seconds: f32, sample_rate: u32) -> Vec<f32> {
    let n = (seconds.max(0.0) * sample_rate as f32) as usize;
    vec![0.0; n]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_readable_wav() {
        let dir = std::env::temp_dir().join("shtorm_audio_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        let x = vec![0.0f32, 0.5, -0.5, 1.5, -1.5]; // выход за [-1..1] клипается
        write_wav16(&p, 24000, &x).unwrap();
        let mut r = hound::WavReader::open(&p).unwrap();
        assert_eq!(r.spec().sample_rate, 24000);
        assert_eq!(r.spec().channels, 1);
        let got: Vec<i16> = r.samples().map(|s| s.unwrap()).collect();
        assert_eq!(got, vec![0, 16383, -16383, 32767, -32767]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn silence_length() {
        assert_eq!(silence(0.5, 24000).len(), 12000);
        assert_eq!(silence(-1.0, 24000).len(), 0);
    }
}
