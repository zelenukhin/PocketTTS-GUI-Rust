//! A5: потоковая запись PCM16 WAV без накопления аудио в памяти.
//!
//! Каждый чанк пишется сразу (hound WavWriter), паузы — блоками нулей
//! (без создания больших временных векторов). Итоговый файл публикуется
//! только после успешной финализации: черновик лежит в `{stem}.partial.wav`
//! рядом с итогом (явно отличимое имя), публикация — `hard_link` (атомарно
//! отказывает, если итоговый путь уже существует), затем best-effort
//! удаление собственного `.partial`. При отмене/ошибке `.partial` остаётся
//! на месте, путь сообщается вызывающему; пользовательские файлы
//! неприкосновенны (create_new, никаких рекурсивных удалений).

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

/// Максимум данных в RIFF (u32 общий размер: 36 байт заголовка + data).
pub const RIFF_MAX_DATA_BYTES: u64 = u32::MAX as u64 - 36;
/// Блок нулевой тишины для пауз (сэмплы): пауза пишется кусками, не вектором.
const SILENCE_BLOCK_SAMPLES: usize = 4096;

/// Потоковый WAV-writer (mono PCM16).
pub struct StreamingWav {
    writer: Option<hound::WavWriter<BufWriter<File>>>,
    final_path: PathBuf,
    partial_path: PathBuf,
    sample_rate: u32,
    data_bytes: u64,
    samples_written: u64,
    max_data_bytes: u64,
}

impl StreamingWav {
    /// Создать: итога быть не должно; черновик `{stem}.partial.wav` — тоже
    /// (create_new). `max_data_bytes` — тестируемый предел RIFF.
    pub fn create(final_path: &Path, sample_rate: u32) -> Result<Self> {
        Self::create_with_limit(final_path, sample_rate, RIFF_MAX_DATA_BYTES)
    }

    pub fn create_with_limit(
        final_path: &Path,
        sample_rate: u32,
        max_data_bytes: u64,
    ) -> Result<Self> {
        anyhow::ensure!(
            !final_path.exists(),
            "итоговый WAV уже существует: {} — не перезаписываем",
            final_path.display()
        );
        let partial_path = partial_path_for(final_path, "wav");
        anyhow::ensure!(
            !partial_path.exists(),
            "черновик уже существует: {} — удалите/переименуйте вручную",
            partial_path.display()
        );
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial_path)
            .with_context(|| format!("создать черновик {}", partial_path.display()))?;
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::new(BufWriter::new(file), spec)
            .with_context(|| format!("WAV-заголовок {}", partial_path.display()))?;
        Ok(Self {
            writer: Some(writer),
            final_path: final_path.to_path_buf(),
            partial_path,
            sample_rate,
            data_bytes: 0,
            samples_written: 0,
            max_data_bytes,
        })
    }

    /// PCM16 конверсия как в GUI: clip [-1..1] * 32767 -> i16.
    pub fn write_chunk(&mut self, pcm: &[f32], pause_s: Option<f32>) -> Result<()> {
        let pause_samples = pause_s
            .map(|p| (p.max(0.0) * self.sample_rate as f32) as u64)
            .unwrap_or(0);
        let add_bytes = (pcm.len() as u64 + pause_samples) * 2;
        if self.data_bytes + add_bytes > self.max_data_bytes {
            bail!(
                "WAV превысит лимит RIFF ({} байт данных): файл не может быть дописан — \
                 нужен RF64 или разбиение на части (функция следующего этапа). \
                 Черновик остаётся: {}",
                self.max_data_bytes,
                self.partial_path.display()
            );
        }
        let w = self.writer.as_mut().expect("writer открыт до finalize");
        for &s in pcm {
            let v = s.clamp(-1.0, 1.0);
            w.write_sample((v * 32767.0) as i16)?;
        }
        // пауза блоками нулей (без большого вектора)
        let mut remaining = pause_samples as usize;
        while remaining > 0 {
            let n = remaining.min(SILENCE_BLOCK_SAMPLES);
            for _ in 0..n {
                w.write_sample(0i16)?;
            }
            remaining -= n;
        }
        self.data_bytes += add_bytes;
        self.samples_written += pcm.len() as u64 + pause_samples;
        Ok(())
    }

    /// Финализация: заголовок, закрытие, атомарная публикация итога.
    pub fn finalize(mut self) -> Result<PathBuf> {
        let mut w = self.writer.take().expect("writer ещё открыт");
        w.flush()?;
        w.finalize()?;
        // публикация: hard_link отказывает, если итог уже создан (атомарно)
        std::fs::hard_link(&self.partial_path, &self.final_path).with_context(|| {
            format!(
                "опубликовать {} -> {} (итог мог появиться извне; черновик сохранён)",
                self.partial_path.display(),
                self.final_path.display()
            )
        })?;
        // наш собственный черновик после успешной публикации — best-effort
        if let Err(e) = std::fs::remove_file(&self.partial_path) {
            eprintln!(
                "примечание: не удалось убрать черновик {} ({e}); итог уже опубликован",
                self.partial_path.display()
            );
        }
        Ok(self.final_path)
    }

    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }
    pub fn data_bytes(&self) -> u64 {
        self.data_bytes
    }
    pub fn partial_path(&self) -> &Path {
        &self.partial_path
    }
}

/// Путь черновика: `{stem}.partial.{ext}` рядом с итогом.
pub fn partial_path_for(final_path: &Path, ext: &str) -> PathBuf {
    let stem = final_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".into());
    final_path.with_file_name(format!("{stem}.partial.{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "shtorm_stream_{}_{}_{}",
            std::process::id(),
            tag,
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn read_wav(p: &Path) -> (u32, Vec<i16>) {
        let mut r = hound::WavReader::open(p).unwrap();
        let sr = r.spec().sample_rate;
        (sr, r.samples().map(|s| s.unwrap()).collect())
    }

    /// Конверсия как в GUI: clip [-1..1] * 32767 -> i16.
    fn to_i16(v: f32) -> i16 {
        (v.clamp(-1.0, 1.0) * 32767.0) as i16
    }

    #[test]
    fn streams_in_order_with_pauses_and_no_trailing_pause() {
        let d = unique_dir("order");
        let final_p = d.join("book.wav");
        let mut w = StreamingWav::create(&final_p, 24000).unwrap();
        // маркеры: чанк k = k+1 сэмплов значения (k как f32)/10
        let m = |k: f32| to_i16(k / 10.0);
        w.write_chunk(&[0.1], Some(0.001)).unwrap(); // 24 сэмпла паузы
        w.write_chunk(&[0.2, 0.2], Some(0.0)).unwrap(); // 0 сэмплов паузы
        w.write_chunk(&[0.3, 0.3, 0.3], None).unwrap(); // последний — без паузы
        let path = w.finalize().unwrap();
        assert_eq!(path, final_p);
        let (sr, pcm) = read_wav(&final_p);
        assert_eq!(sr, 24000);
        assert_eq!(pcm.len(), 1 + 24 + 2 + 3, "речь 6 + пауза 24");
        assert_eq!(&pcm[..1], &[m(1.0)]);
        assert!(pcm[1..25].iter().all(|&v| v == 0), "пауза — нули");
        assert_eq!(&pcm[25..27], &[m(2.0), m(2.0)]);
        assert_eq!(&pcm[27..30], &[m(3.0), m(3.0), m(3.0)]);
        // черновик убран после публикации
        assert!(!partial_path_for(&final_p, "wav").exists());
    }

    #[test]
    fn bit_parity_with_buffered_write_wav16() {
        let d = unique_dir("parity");
        // разнообразный PCM (включая клиппинг) и паузы
        let chunk1: Vec<f32> = (0..3000).map(|i| ((i % 97) as f32 / 48.5) - 1.0).collect();
        let chunk2: Vec<f32> = (0..1500).map(|i| ((i % 13) as f32 / 6.0) - 1.0).collect();
        let pause = 0.25f32;
        // 1) потоковый путь
        let stream_path = d.join("stream.wav");
        let mut w = StreamingWav::create(&stream_path, 24000).unwrap();
        w.write_chunk(&chunk1, Some(pause)).unwrap();
        w.write_chunk(&chunk2, None).unwrap();
        w.finalize().unwrap();
        // 2) «старый» путь: собрать всё и записать write_wav16 (A4-паритет)
        let mut all = chunk1.clone();
        all.extend(crate::silence(pause, 24000));
        all.extend_from_slice(&chunk2);
        let buffered_path = d.join("buffered.wav");
        crate::write_wav16(&buffered_path, 24000, &all).unwrap();
        // побитовое сравнение файлов
        let a = std::fs::read(&stream_path).unwrap();
        let b = std::fs::read(&buffered_path).unwrap();
        assert_eq!(a, b, "потоковый WAV обязан совпасть побитово с A4-записью");
    }

    #[test]
    fn refuses_existing_final_and_keeps_it_intact() {
        let d = unique_dir("exists");
        let final_p = d.join("book.wav");
        std::fs::write(&final_p, b"user data").unwrap();
        let err = StreamingWav::create(&final_p, 24000);
        assert!(err.is_err());
        assert_eq!(std::fs::read(&final_p).unwrap(), b"user data");
        assert!(
            !partial_path_for(&final_p, "wav").exists(),
            "черновик не создан"
        );
    }

    #[test]
    fn refuses_existing_partial() {
        let d = unique_dir("partial");
        let final_p = d.join("book.wav");
        std::fs::write(partial_path_for(&final_p, "wav"), b"old partial").unwrap();
        let err = StreamingWav::create(&final_p, 24000);
        assert!(err.is_err(), "существующий .partial не перезаписывается");
    }

    #[test]
    fn riff_limit_is_enforced_without_silent_overflow() {
        let d = unique_dir("riff");
        let final_p = d.join("big.wav");
        // лимит 1000 байт данных
        let mut w = StreamingWav::create_with_limit(&final_p, 24000, 1000).unwrap();
        w.write_chunk(&[0.1f32; 400], None).unwrap(); // 800 байт
        let err = w.write_chunk(&[0.1f32; 200], None); // ещё 400 → превысит
        assert!(err.is_err(), "превышение RIFF-лимита обязано быть ошибкой");
        let msg = format!("{:#}", err.unwrap_err());
        assert!(msg.contains("RIFF"), "{msg}");
    }

    #[test]
    fn partial_remains_on_abort_without_deletion() {
        let d = unique_dir("abort");
        let final_p = d.join("book.wav");
        let mut w = StreamingWav::create(&final_p, 24000).unwrap();
        w.write_chunk(&[0.5f32; 100], None).unwrap();
        let partial = w.partial_path().to_path_buf();
        drop(w); // аборт: без finalize
        assert!(partial.exists(), "черновик сохранён для диагностики");
        assert!(!final_p.exists(), "итог при аборте не публикуется");
    }
}
