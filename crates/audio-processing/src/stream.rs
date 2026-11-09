//! A5: потоковая запись PCM16 WAV без накопления аудио в памяти.
//!
//! Каждый чанк пишется сразу (hound WavWriter), паузы — блоками нулей
//! (без создания больших временных векторов). Итоговый файл публикуется
//! только после успешной финализации: черновик лежит в
//! `{stem}.{время}.{pid}.partial.{ext}` рядом с итогом (уникальное имя за
//! запуск, никогда не перезаписывается).
//!
//! Политика публикации:
//! - по умолчанию — Win32 `MoveFileW` БЕЗ MOVEFILE_REPLACE_EXISTING: при
//!   занятом имени вызов атомарно отказывает, файл-приёмник не изменяется,
//!   черновик остаётся;
//! - G5 `overwrite=true` — `MoveFileExW(REPLACE_EXISTING|WRITE_THROUGH)`:
//!   замена выполняется только после полной успешной финализации sink.
//!   Старый файл НЕ удаляется заранее; при ошибке замены он остаётся
//!   доступным, а `.partial` сохраняется для диагностики.
//!
//! После успешного переноса `.partial` больше нет (перемещён, не удалён).
//! При отмене/ошибке `.partial` остаётся и путь сообщается вызывающему;
//! пользовательские файлы неприкосновенны (create_new, без рекурсивных
//! удалений, без автоматической чистки старых черновиков).

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
    /// G5: разрешена замена существующего готового файла при finalize.
    overwrite: bool,
}

impl StreamingWav {
    /// Создать: итога быть не должно; черновик `{stem}.partial.wav` — тоже
    /// (create_new).
    pub fn create(final_path: &Path, sample_rate: u32) -> Result<Self> {
        Self::create_with_mode(final_path, sample_rate, false, RIFF_MAX_DATA_BYTES)
    }

    /// G5: создать с явной политикой замены. При `overwrite=true` готовый
    /// файл может существовать; он НЕ трогается до успешного finalize.
    pub fn create_with_overwrite(
        final_path: &Path,
        sample_rate: u32,
        overwrite: bool,
    ) -> Result<Self> {
        Self::create_with_mode(final_path, sample_rate, overwrite, RIFF_MAX_DATA_BYTES)
    }

    pub fn create_with_limit(
        final_path: &Path,
        sample_rate: u32,
        max_data_bytes: u64,
    ) -> Result<Self> {
        Self::create_with_mode(final_path, sample_rate, false, max_data_bytes)
    }

    /// Общий конструктор: `overwrite` разрешает существующий итог только
    /// для аудиофайла назначения; каталог/ссылка в качестве итога отвергаются.
    pub fn create_with_mode(
        final_path: &Path,
        sample_rate: u32,
        overwrite: bool,
        max_data_bytes: u64,
    ) -> Result<Self> {
        if overwrite {
            // Не заменяем каталоги и reparse-точки: это не «готовый
            // аудиофайл», и замена могла бы уйти за пределы выбранного пути.
            if let Ok(md) = std::fs::symlink_metadata(final_path) {
                if md.is_dir() {
                    bail!(
                        "итоговый путь указывает на каталог, замена невозможна: {}",
                        final_path.display()
                    );
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
                    if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        bail!(
                            "итоговый путь — ссылка/точка перенаправления (reparse point): {} — укажите физический файл",
                            final_path.display()
                        );
                    }
                }
                #[cfg(not(windows))]
                if md.file_type().is_symlink() {
                    bail!(
                        "итоговый путь — символическая ссылка: {} — укажите физический файл",
                        final_path.display()
                    );
                }
            }
        } else {
            anyhow::ensure!(
                !final_path.exists(),
                "итоговый WAV уже существует: {} — не перезаписываем",
                final_path.display()
            );
        }
        // уникальное имя за запуск: старые черновики не мешают повторному
        // запуску (и не удаляются автоматически)
        let partial_path = unique_partial_path(final_path, "wav");
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
            overwrite,
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

    /// G5.1: финализация записи БЕЗ публикации — заголовок и закрытие
    /// черновика. Публикация (необратимый шаг) выделена в `FinalizedWav`,
    /// чтобы конвейер мог принять решение об отмене между ними.
    pub fn finish(mut self) -> Result<FinalizedWav> {
        let mut w = self.writer.take().expect("writer ещё открыт");
        w.flush()?;
        w.finalize()?;
        Ok(FinalizedWav {
            partial_path: self.partial_path,
            final_path: self.final_path,
            overwrite: self.overwrite,
        })
    }

    /// Финализация + публикация одним шагом (прежнее поведение; тесты и
    /// совместимые вызовы). Семантика публикации — как у `FinalizedWav::publish`.
    pub fn finalize(self) -> Result<PathBuf> {
        self.finish()?.publish()
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

/// G5.1: записанный и закрытый WAV-черновик, готовый к публикации.
/// Публикация — отдельный необратимый шаг: конвейер выполняет её только
/// после однозначного решения не отменять задание.
pub struct FinalizedWav {
    partial_path: PathBuf,
    final_path: PathBuf,
    overwrite: bool,
}

impl FinalizedWav {
    /// Опубликовать черновик под итоговым именем. По умолчанию — без замены
    /// (MoveFileW; при занятом имени отказ, черновик остаётся). При
    /// `overwrite=true` — атомарная замена MoveFileExW; старый файл не
    /// удалялся заранее и остаётся при любой ошибке замены.
    pub fn publish(self) -> Result<PathBuf> {
        if self.overwrite {
            publish_replace(&self.partial_path, &self.final_path)?;
        } else {
            publish_no_replace(&self.partial_path, &self.final_path)?;
        }
        Ok(self.final_path)
    }

    pub fn partial_path(&self) -> &Path {
        &self.partial_path
    }
}

/// Уникальный путь черновика за запуск:
/// `{stem}.{нс}.{pid}.{счётчик}.partial.{ext}` — наносекунды + атомарный
/// счётчик гарантируют различие последовательных запусков в одном процессе.
/// Создание всё равно эксклюзивное (create_new / ffmpeg -n): теоретическая
/// коллизия даст явную ошибку, но не перезапись.
pub fn unique_partial_path(final_path: &Path, ext: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let stem = final_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    final_path.with_file_name(format!(
        "{stem}.{nanos}.{}.{seq}.partial.{ext}",
        std::process::id()
    ))
}

/// Публикация черновика под итоговым именем БЕЗ замены существующего файла.
/// Windows: `MoveFileW` без MOVEFILE_REPLACE_EXISTING — атомарный отказ при
/// занятом имени (NTFS; семантика Win32 одинакова для exFAT, отдельно не
/// проверялась). Не-Windows: `hard_link` (fail-if-exists) + удаление
/// собственного черновика — authорitative замены не происходит.
#[cfg(windows)]
pub fn publish_no_replace(from: &Path, to: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileW;
    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (w_from, w_to) = (wide(from), wide(to));
    let ok = unsafe { MoveFileW(w_from.as_ptr(), w_to.as_ptr()) };
    if ok == 0 {
        let e = std::io::Error::last_os_error();
        anyhow::bail!(
            "MoveFileW({} -> {}) отказал: {e}; черновик сохранён",
            from.display(),
            to.display()
        );
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn publish_no_replace(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::hard_link(from, to).with_context(|| {
        format!(
            "опубликовать {} -> {} (итог мог появиться извне; черновик сохранён)",
            from.display(),
            to.display()
        )
    })?;
    let _ = std::fs::remove_file(from);
    Ok(())
}

/// G5: публикация с заменой существующего готового файла. Вызывается ТОЛЬКО
/// после успешной финализации sink (Completed). На Windows — `MoveFileExW`
/// с `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`: замена атомарна
/// на NTFS; при ошибке (занятый файл, права, антивирус) назначение остаётся
/// прежним, а черновик — на месте. Схема «удалить старый → переименовать»
/// намеренно НЕ используется.
#[cfg(windows)]
pub fn publish_replace(from: &Path, to: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (w_from, w_to) = (wide(from), wide(to));
    let ok = unsafe {
        MoveFileExW(
            w_from.as_ptr(),
            w_to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        let e = std::io::Error::last_os_error();
        anyhow::bail!(
            "MoveFileExW(REPLACE) {} -> {} отказал: {e}; старый файл не изменён, черновик сохранён",
            from.display(),
            to.display()
        );
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn publish_replace(from: &Path, to: &Path) -> anyhow::Result<()> {
    // rename на Unix атомарно заменяет существующий файл.
    std::fs::rename(from, to)
        .with_context(|| format!("заменить {} -> {}", from.display(), to.display()))
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
        assert!(!unique_partial_path(&final_p, "wav").exists());
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
            !unique_partial_path(&final_p, "wav").exists(),
            "черновик не создан"
        );
    }

    #[test]
    fn unique_partials_and_rerun_after_abort() {
        let d = unique_dir("rerun");
        let final_p = d.join("book.wav");
        // запуск 1: пишем и «абортим»
        let mut w1 = StreamingWav::create(&final_p, 24000).unwrap();
        w1.write_chunk(&[0.1f32; 50], None).unwrap();
        let partial1 = w1.partial_path().to_path_buf();
        drop(w1);
        assert!(partial1.exists(), "черновик 1 сохранён");
        assert!(!final_p.exists());
        // запуск 2 с тем же итоговым именем: старый partial не мешает,
        // имена различаются, ничего не удаляется
        let w2 = StreamingWav::create(&final_p, 24000).unwrap();
        let partial2 = w2.partial_path().to_path_buf();
        assert_ne!(partial1, partial2, "уникальное имя за запуск");
        assert!(partial1.exists(), "старый черновик не тронут");
        drop(w2);
    }

    #[test]
    fn publish_no_replace_refuses_existing_and_keeps_both() {
        let d = unique_dir("publish");
        let src = d.join("draft.bin");
        let dst = d.join("final.bin");
        std::fs::write(&src, b"draft").unwrap();
        std::fs::write(&dst, b"user data").unwrap();
        let err = publish_no_replace(&src, &dst);
        assert!(
            err.is_err(),
            "публикация при занятом имени обязана отказать"
        );
        assert_eq!(
            std::fs::read(&dst).unwrap(),
            b"user data",
            "назначение не тронуто"
        );
        assert_eq!(std::fs::read(&src).unwrap(), b"draft", "черновик сохранён");
        // свободное имя: публикация переносит файл (черновика больше нет)
        let dst2 = d.join("final2.bin");
        publish_no_replace(&src, &dst2).unwrap();
        assert_eq!(std::fs::read(&dst2).unwrap(), b"draft");
        assert!(!src.exists(), "MoveFileW переносит (не копирует)");
    }

    /// G5: overwrite=true заменяет готовый файл только при успешном finalize.
    #[test]
    fn overwrite_true_replaces_old_final_after_successful_finalize() {
        let d = unique_dir("ow_replace");
        let final_p = d.join("book.wav");
        std::fs::write(&final_p, b"old audio").unwrap();
        let mut w = StreamingWav::create_with_overwrite(&final_p, 24000, true).unwrap();
        w.write_chunk(&[0.5f32; 100], None).unwrap();
        let partial = w.partial_path().to_path_buf();
        let published = w.finalize().unwrap();
        assert_eq!(published, final_p);
        assert_ne!(std::fs::read(&final_p).unwrap(), b"old audio");
        assert!(!partial.exists(), "partial перемещён при замене");
        let (_, pcm) = read_wav(&final_p);
        assert_eq!(pcm.len(), 100);
    }

    /// G5: при аборте без finalize старый готовый файл остаётся нетронутым,
    /// черновик сохраняется.
    #[test]
    fn overwrite_abort_keeps_old_file_and_partial() {
        let d = unique_dir("ow_abort");
        let final_p = d.join("book.wav");
        std::fs::write(&final_p, b"old audio").unwrap();
        let mut w = StreamingWav::create_with_overwrite(&final_p, 24000, true).unwrap();
        w.write_chunk(&[0.5f32; 100], None).unwrap();
        let partial = w.partial_path().to_path_buf();
        drop(w);
        assert_eq!(std::fs::read(&final_p).unwrap(), b"old audio");
        assert!(partial.exists(), "черновик сохранён для диагностики");
    }

    /// G5.1: finish закрывает черновик БЕЗ публикации — итог ещё не создан и
    /// старый файл не тронут; publish отдельно заменяет его. Между шагами
    /// конвейер может принять решение об отмене без всяких последствий.
    #[test]
    fn finish_then_publish_split_keeps_old_file_between_steps() {
        let d = unique_dir("ow_split");
        let final_p = d.join("book.wav");
        std::fs::write(&final_p, b"old audio").unwrap();
        let mut w = StreamingWav::create_with_overwrite(&final_p, 24000, true).unwrap();
        w.write_chunk(&[0.5f32; 100], None).unwrap();
        let fin = w.finish().unwrap();
        // черновик закрыт и корректен, итог ещё НЕ опубликован
        assert_eq!(std::fs::read(&final_p).unwrap(), b"old audio");
        let (_, pcm) = read_wav(fin.partial_path());
        assert_eq!(pcm.len(), 100, "черновик дописан до закрытия");
        // публикация — отдельный необратимый шаг
        let published = fin.publish().unwrap();
        assert_eq!(published, final_p);
        assert_ne!(std::fs::read(&final_p).unwrap(), b"old audio");
    }

    /// G5: каталог на месте итога — не «готовый аудиофайл»; замена отвергается.
    #[test]
    fn overwrite_refuses_directory_target() {
        let d = unique_dir("ow_dir");
        let dir = d.join("book.wav");
        std::fs::create_dir(&dir).unwrap();
        let err = StreamingWav::create_with_overwrite(&dir, 24000, true);
        assert!(err.is_err());
        assert!(dir.is_dir());
    }

    /// G5: публикация с заменой — успешная замена и сохранность обоих файлов
    /// при ошибке (схема «удалить старый → переименовать» не используется).
    #[test]
    fn publish_replace_replaces_on_success_and_keeps_old_on_failure() {
        let d = unique_dir("pub_replace");
        let src = d.join("draft.bin");
        let dst = d.join("final.bin");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        publish_replace(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        assert!(!src.exists(), "MoveFileExW переносит, а не копирует");
        // ошибка замены: назначение — родитель-файл, move невозможен;
        // старый файл и черновик остаются на месте
        let src2 = d.join("draft2.bin");
        std::fs::write(&src2, b"draft2").unwrap();
        let blocker = d.join("blocker.bin");
        std::fs::write(&blocker, b"blocker").unwrap();
        let bad_dst = blocker.join("child.bin");
        let err = publish_replace(&src2, &bad_dst);
        assert!(err.is_err());
        assert_eq!(std::fs::read(&src2).unwrap(), b"draft2");
        assert_eq!(std::fs::read(&blocker).unwrap(), b"blocker");
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
