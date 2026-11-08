//! A5: общий потоковый вывод аудио. Чанки принимаются строго по порядку
//! (`write_chunk(index, pcm, pause_s)`), ничего целого в памяти не держится:
//! WAV пишется сразу hound-ом, MP3/Opus — PCM s16le в stdin ffmpeg.
//!
//! Безопасность результатов (SAFETY_AUDIT.md):
//! - итог никогда не перезаписывается (`create_new` для WAV; ffmpeg `-n` +
//!   отдельный `.partial` путь, которого заранее нет);
//! - публикация итога — `hard_link(partial, final)` (атомарно отказывает,
//!   если итог появился), затем best-effort удаление собственного `.partial`;
//! - при отмене/ошибке `.partial` остаётся и его путь сообщается вызывающему;
//! - рекурсивных удалений нет.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use audio_processing::stream::{partial_path_for, StreamingWav};

/// Потоковый приёмник готовых чанков (строго по порядку).
pub trait AudioSink {
    fn write_chunk(&mut self, index: usize, pcm: &[f32], pause_s: Option<f32>) -> Result<()>;
    /// Завершить: дописать контейнер, опубликовать итоговый файл
    /// (потребляет синк — дальнейшие записи невозможны).
    fn finalize(self: Box<Self>) -> Result<PathBuf>;
    /// Черновик (для сообщений при отмене/ошибке).
    fn partial_path(&self) -> PathBuf;
}

/// Формат вывода.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Wav,
    Mp3,
    Opus,
}

impl OutputFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            OutputFormat::Wav => "wav",
            OutputFormat::Mp3 => "mp3",
            OutputFormat::Opus => "opus",
        }
    }
}

/// WAV-синк: обёртка над потоковым writer-ом audio-processing.
pub struct WavSink {
    inner: StreamingWav,
}

impl WavSink {
    pub fn create(final_path: &Path, sample_rate: u32) -> Result<Self> {
        Ok(Self {
            inner: StreamingWav::create(final_path, sample_rate)?,
        })
    }
}

impl AudioSink for WavSink {
    fn write_chunk(&mut self, _index: usize, pcm: &[f32], pause_s: Option<f32>) -> Result<()> {
        self.inner.write_chunk(pcm, pause_s)
    }
    fn finalize(self: Box<Self>) -> Result<PathBuf> {
        self.inner.finalize()
    }
    fn partial_path(&self) -> PathBuf {
        self.inner.partial_path().to_path_buf()
    }
}

/// Аргументы энкодера FFmpeg — 1:1 с Python GUI `_encoder_args`:
/// MP3: -V0..-V4 -> libmp3lame -q:a N; «192k/128k» -> -b:a; Opus: «opus 96k»
/// -> libopus -b:a 96k (не-опус пресет для OPUS — фолбэк 96k, как в GUI).
pub fn encoder_args(format: OutputFormat, preset: &str) -> Vec<String> {
    match format {
        OutputFormat::Wav => Vec::new(),
        OutputFormat::Mp3 => {
            if let Some(q) = preset.strip_prefix("-V") {
                let q = q.split_whitespace().next().unwrap_or("2");
                vec![
                    "-codec:a".into(),
                    "libmp3lame".into(),
                    "-q:a".into(),
                    q.into(),
                ]
            } else if preset.ends_with('k') {
                vec![
                    "-codec:a".into(),
                    "libmp3lame".into(),
                    "-b:a".into(),
                    preset.into(),
                ]
            } else {
                vec![
                    "-codec:a".into(),
                    "libmp3lame".into(),
                    "-q:a".into(),
                    "2".into(),
                ]
            }
        }
        OutputFormat::Opus => {
            let bitrate = if preset.starts_with("opus") {
                preset.split_whitespace().nth(1).unwrap_or("96k")
            } else {
                "96k"
            };
            vec![
                "-codec:a".into(),
                "libopus".into(),
                "-b:a".into(),
                bitrate.into(),
            ]
        }
    }
}

/// Найти ffmpeg: явный путь → рядом с exe → ../ffmpeg (портативная сборка) →
/// PATH. Ошибочный путь — понятная ошибка (без тихого фолбэка).
pub fn resolve_ffmpeg(explicit: Option<&Path>) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        candidates.push(p.to_path_buf());
    } else {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("ffmpeg.exe"));
                if let Some(parent) = dir.parent() {
                    candidates.push(parent.join("ffmpeg").join("ffmpeg.exe"));
                }
            }
        }
        candidates.push(PathBuf::from("ffmpeg"));
    }
    for cand in candidates {
        if cand.components().count() > 1 && !cand.is_file() {
            continue;
        }
        let ok = Command::new(&cand)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Ok(cand);
        }
    }
    bail!(
        "ffmpeg не найден/не запускается: укажите --ffmpeg <путь> или положите ffmpeg.exe рядом с приложением"
    )
}

/// FFmpeg-синк: PCM s16le (mono, sample_rate) -> stdin -> файл.
pub struct FfmpegSink {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr_thread: Option<std::thread::JoinHandle<String>>,
    partial: PathBuf,
    final_path: PathBuf,
    buf: Vec<u8>,
    bytes_fed: u64,
}

/// Сколько байт stderr держим для диагностики.
const STDERR_CAP: u64 = 8192;

impl FfmpegSink {
    pub fn create(
        final_path: &Path,
        sample_rate: u32,
        format: OutputFormat,
        preset: &str,
        ffmpeg: &Path,
    ) -> Result<Self> {
        anyhow::ensure!(
            !final_path.exists(),
            "итоговый файл уже существует: {} — не перезаписываем",
            final_path.display()
        );
        let ext = format.extension();
        let partial = partial_path_for(final_path, ext);
        anyhow::ensure!(
            !partial.exists(),
            "черновик уже существует: {} — удалите/переименуйте вручную",
            partial.display()
        );
        let args = Self::base_args(sample_rate, format, preset, &partial);
        Self::spawn(ffmpeg, &args, partial, final_path.to_path_buf())
    }

    /// Полный набор аргументов (`-n`: никогда не перезаписывать).
    pub fn base_args(
        sample_rate: u32,
        format: OutputFormat,
        preset: &str,
        out: &Path,
    ) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-n".into(),
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "s16le".into(),
            "-ar".into(),
            sample_rate.to_string(),
            "-ac".into(),
            "1".into(),
            "-i".into(),
            "pipe:0".into(),
        ];
        a.extend(encoder_args(format, preset));
        a.push(out.to_string_lossy().into_owned());
        a
    }

    /// Запуск процесса (отдельный конструктор — удобен для тестов с
    /// некорректными аргументами).
    pub fn spawn(
        program: &Path,
        args: &[String],
        partial: PathBuf,
        final_path: PathBuf,
    ) -> Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("запуск ffmpeg: {}", program.display()))?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let stderr_thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let _ = stderr.take(STDERR_CAP).read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            stderr_thread: Some(stderr_thread),
            partial,
            final_path,
            buf: Vec::new(),
            bytes_fed: 0,
        })
    }

    fn feed(&mut self, samples: &[f32]) -> Result<()> {
        self.buf.clear();
        self.buf.reserve(samples.len() * 2);
        for &s in samples {
            let v = s.clamp(-1.0, 1.0);
            self.buf
                .extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
        }
        let stdin = self.stdin.as_mut().expect("stdin открыт до finalize");
        match stdin.write_all(&self.buf) {
            Ok(()) => {
                self.bytes_fed += self.buf.len() as u64;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                let err = self.collect_stderr();
                bail!(
                    "ffmpeg закрыл stdin раньше времени (BrokenPipe после {} байт): {}",
                    self.bytes_fed,
                    err.trim()
                )
            }
            Err(e) => Err(e).context("запись PCM в ffmpeg"),
        }
    }

    fn collect_stderr(&mut self) -> String {
        // закрываем stdin и ждём процесс, чтобы прочитать хвост stderr
        drop(self.stdin.take());
        let status = self.child.wait().ok();
        let text = self
            .stderr_thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default();
        format!("{text} (код {:?})", status.map(|s| s.code()))
    }
}

impl AudioSink for FfmpegSink {
    fn write_chunk(&mut self, _index: usize, pcm: &[f32], pause_s: Option<f32>) -> Result<()> {
        self.feed(pcm)?;
        if let Some(p) = pause_s {
            let n = (p.max(0.0) * 24_000.0) as usize * 2; // байт тишины (s16)
                                                          // блоками, без больших временных векторов
            static ZERO: [u8; 8192] = [0u8; 8192];
            let mut left = n;
            while left > 0 {
                let k = left.min(ZERO.len());
                let stdin = self.stdin.as_mut().expect("stdin открыт");
                if let Err(e) = stdin.write_all(&ZERO[..k]) {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        let err = self.collect_stderr();
                        bail!("ffmpeg закрыл stdin при записи паузы: {}", err.trim());
                    }
                    return Err(e).context("запись паузы в ffmpeg");
                }
                left -= k;
            }
            self.bytes_fed += n as u64;
        }
        Ok(())
    }

    fn finalize(mut self: Box<Self>) -> Result<PathBuf> {
        // корректно закрыть stdin и дождаться энкодера
        drop(self.stdin.take());
        let status = self.child.wait().context("ожидание ffmpeg")?;
        let stderr = self
            .stderr_thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default();
        if !status.success() {
            bail!(
                "ffmpeg завершился с ошибкой (код {:?}): {} — черновик сохранён: {}",
                status.code(),
                stderr.trim(),
                self.partial.display()
            );
        }
        // публикация только после успешного кодирования
        std::fs::hard_link(&self.partial, &self.final_path).with_context(|| {
            format!(
                "опубликовать {} -> {} (итог мог появиться извне; черновик сохранён)",
                self.partial.display(),
                self.final_path.display()
            )
        })?;
        if let Err(e) = std::fs::remove_file(&self.partial) {
            eprintln!(
                "примечание: не удалось убрать черновик {} ({e}); итог опубликован",
                self.partial.display()
            );
        }
        Ok(self.final_path.clone())
    }

    fn partial_path(&self) -> PathBuf {
        self.partial.clone()
    }
}

/// Создать синк по формату.
pub fn make_sink(
    final_path: &Path,
    sample_rate: u32,
    format: OutputFormat,
    preset: &str,
    ffmpeg: Option<&Path>,
) -> Result<Box<dyn AudioSink>> {
    match format {
        OutputFormat::Wav => Ok(Box::new(WavSink::create(final_path, sample_rate)?)),
        OutputFormat::Mp3 | OutputFormat::Opus => {
            let prog = resolve_ffmpeg(ffmpeg)?;
            Ok(Box::new(FfmpegSink::create(
                final_path,
                sample_rate,
                format,
                preset,
                &prog,
            )?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_args_match_python_gui() {
        assert_eq!(
            encoder_args(OutputFormat::Mp3, "-V0 (Максимальное)"),
            vec!["-codec:a", "libmp3lame", "-q:a", "0"]
        );
        assert_eq!(
            encoder_args(OutputFormat::Mp3, "-V4 (Среднее)"),
            vec!["-codec:a", "libmp3lame", "-q:a", "4"]
        );
        assert_eq!(
            encoder_args(OutputFormat::Mp3, "192k"),
            vec!["-codec:a", "libmp3lame", "-b:a", "192k"]
        );
        assert_eq!(
            encoder_args(OutputFormat::Opus, "opus 64k"),
            vec!["-codec:a", "libopus", "-b:a", "64k"]
        );
        // не-опус пресет для OPUS — фолбэк 96k как в GUI
        assert_eq!(
            encoder_args(OutputFormat::Opus, "-V2"),
            vec!["-codec:a", "libopus", "-b:a", "96k"]
        );
        assert_eq!(
            FfmpegSink::base_args(
                24000,
                OutputFormat::Mp3,
                "-V2",
                Path::new("x.mp3.partial.mp3")
            )
            .last()
            .unwrap(),
            "x.mp3.partial.mp3"
        );
    }

    #[test]
    fn partial_naming_is_explicit() {
        assert_eq!(
            partial_path_for(Path::new("/out/book.mp3"), "mp3"),
            PathBuf::from("/out/book.partial.mp3")
        );
        assert_eq!(
            partial_path_for(Path::new(r"C:\out\book.opus"), "opus"),
            PathBuf::from(r"C:\out\book.partial.opus")
        );
    }

    /// ffmpeg для тестов: env SHTORM_FFMPEG или распространённые места; если
    /// нет — SKIP с явным сообщением (внешний инструмент).
    fn test_ffmpeg() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("SHTORM_FFMPEG") {
            let p = PathBuf::from(p);
            return p.is_file().then_some(p);
        }
        resolve_ffmpeg(None).ok()
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "shtorm_ffmpeg_{}_{}_{}",
            std::process::id(),
            tag,
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ffmpeg_nonzero_exit_is_error_and_no_final() {
        let Some(ff) = test_ffmpeg() else {
            eprintln!("SKIP ffmpeg_nonzero_exit: ffmpeg недоступен");
            return;
        };
        let d = unique_dir("bad_args");
        let final_p = d.join("out.mp3");
        let partial = partial_path_for(&final_p, "mp3");
        // заведомо некорректный флаг энкодера
        let mut args = FfmpegSink::base_args(24000, OutputFormat::Mp3, "-V2", &partial);
        args.insert(args.len() - 1, "-bogus_flag".into());
        let mut sink: Box<dyn AudioSink> =
            Box::new(FfmpegSink::spawn(&ff, &args, partial.clone(), final_p.clone()).unwrap());
        // ffmpeg падает сразу; запись упрётся в BrokenPipe или finalize в код != 0
        let _ = sink.write_chunk(0, &[0.1f32; 4800], None);
        let err = sink.finalize().unwrap_err();
        assert!(format!("{err:#}").contains("ffmpeg"), "{err:#}");
        assert!(!final_p.exists(), "итог не публикуется при ошибке ffmpeg");
    }

    #[test]
    fn ffmpeg_success_produces_final_mp3() {
        let Some(ff) = test_ffmpeg() else {
            eprintln!("SKIP ffmpeg_success: ffmpeg недоступен");
            return;
        };
        let d = unique_dir("ok");
        let final_p = d.join("out.mp3");
        let mut sink: Box<dyn AudioSink> =
            Box::new(FfmpegSink::create(&final_p, 24000, OutputFormat::Mp3, "-V2", &ff).unwrap());
        sink.write_chunk(0, &[0.05f32; 9600], Some(0.05)).unwrap();
        sink.write_chunk(1, &[0.10f32; 9600], None).unwrap();
        let path = sink.finalize().unwrap();
        assert_eq!(path, final_p);
        let size = std::fs::metadata(&final_p).unwrap().len();
        assert!(size > 500, "mp3 содержателен: {size} байт");
        assert!(!partial_path_for(&final_p, "mp3").exists());
    }

    #[test]
    fn ffmpeg_refuses_existing_final() {
        let d = unique_dir("exists");
        let final_p = d.join("out.mp3");
        std::fs::write(&final_p, b"keep me").unwrap();
        let Some(ff) = test_ffmpeg() else {
            eprintln!("SKIP ffmpeg_refuses_existing_final: ffmpeg недоступен");
            return;
        };
        let err = FfmpegSink::create(&final_p, 24000, OutputFormat::Mp3, "-V2", &ff);
        assert!(err.is_err());
        assert_eq!(std::fs::read(&final_p).unwrap(), b"keep me");
    }
}
