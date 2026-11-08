//! Аудио-хелперы уровня приложения (WAV 16-bit, тишина, конкатенация).
//!
//! write_wav16 — порт `write_wav_file()` из synthesis_worker.py
//! (clip [-1..1] -> PCM16) на крейт hound, с атомарно-эксклюзивным созданием
//! файла (`OpenOptions::create_new` + `WavWriter::new`): существующий файл
//! НИКОГДА не перезаписывается и не усекается (политика SAFETY_AUDIT.md).

use anyhow::{Context, Result};
use std::path::Path;

/// Записывает float-PCM [-1..1] в 16-битный моно WAV. Файл создаётся
/// эксклюзивно: если путь уже существует — ошибка, содержимое не трогается.
pub fn write_wav16(path: &Path, sample_rate: u32, audio: &[f32]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("создать {} (файл существует?)", path.display()))?;
    let mut w = hound::WavWriter::new(file, spec)?;
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

    /// Уникальная песочница без удалений: имя с pid+наносекундами, файлы
    /// остаются в %TEMP% (политика SAFETY_AUDIT.md).
    fn unique_wav(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "shtorm_wav_{}_{}_{}.wav",
            std::process::id(),
            tag,
            nanos
        ))
    }

    #[test]
    fn writes_readable_wav_unique_name() {
        let p = unique_wav("write");
        let x = vec![0.0f32, 0.5, -0.5, 1.5, -1.5]; // выход за [-1..1] клипается
        write_wav16(&p, 24000, &x).unwrap();
        let mut r = hound::WavReader::open(&p).unwrap();
        assert_eq!(r.spec().sample_rate, 24000);
        assert_eq!(r.spec().channels, 1);
        let got: Vec<i16> = r.samples().map(|s| s.unwrap()).collect();
        assert_eq!(got, vec![0, 16383, -16383, 32767, -32767]);
        // временный wav остаётся в %TEMP%: тесты работают без удалений
    }

    #[test]
    fn refuses_existing_file_without_touching_it() {
        let p = unique_wav("exists");
        // сначала успешная запись, затем повтор в тот же путь — отказ
        write_wav16(&p, 24000, &[0.25]).unwrap();
        let err = write_wav16(&p, 24000, &[0.9]);
        assert!(
            err.is_err(),
            "повторная запись в существующий файл запрещена"
        );
        // содержимое не изменилось (первая версия осталась)
        let mut r = hound::WavReader::open(&p).unwrap();
        let got: Vec<i16> = r.samples().map(|s| s.unwrap()).collect();
        assert_eq!(
            got,
            vec![8191],
            "существующий файл не перезаписан и не усечён"
        );
    }

    #[test]
    fn preexisting_foreign_file_is_refused() {
        // файл, созданный НЕ через write_wav16 (например пустой), тоже неприкосновенен
        let p = unique_wav("foreign");
        std::fs::write(&p, b"keep me").unwrap();
        assert!(write_wav16(&p, 24000, &[0.0]).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"keep me");
    }

    #[test]
    fn silence_length() {
        assert_eq!(silence(0.5, 24000).len(), 12000);
        assert_eq!(silence(-1.0, 24000).len(), 0);
    }
}
