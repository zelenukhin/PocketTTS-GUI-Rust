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

use audio_processing::stream::{publish_no_replace, unique_partial_path, StreamingWav};

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
    /// Процесс уже дожат/собран (finalize или abort) — Drop не повторяет.
    finished: bool,
    /// Хвост stderr (диагностика; заполняется при abort/finalize).
    pub stderr_text: Option<String>,
}

/// Сколько байт ХВОСТА stderr держим для диагностики (читаем всегда до EOF —
/// ограничение касается только памяти, не дренирования pipe).
const STDERR_TAIL: usize = 8192;

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
        // уникальное имя за запуск: старые черновики не мешают (не удаляются)
        let partial = unique_partial_path(final_path, ext);
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
        Self::spawn_with_env(program, args, &[], partial, final_path)
    }

    /// Как [spawn], с дополнительными переменными окружения (тесты).
    pub fn spawn_with_env(
        program: &Path,
        args: &[String],
        envs: &[(&str, &str)],
        partial: PathBuf,
        final_path: PathBuf,
    ) -> Result<Self> {
        let mut cmd = Command::new(program);
        for (k, v) in envs {
            cmd.env(k, v);
        }
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
            // Читаем ДО EOF (иначе pipe переполняется и ffmpeg блокируется),
            // храним только ограниченный хвост.
            let mut tail: Vec<u8> = Vec::new();
            let mut buf = [0u8; 8192];
            let mut total: u64 = 0;
            let mut stderr = stderr;
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n as u64;
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > STDERR_TAIL {
                            let cut = tail.len() - STDERR_TAIL;
                            tail.drain(..cut);
                        }
                    }
                    Err(_) => break,
                }
            }
            let mut text = String::from_utf8_lossy(&tail).into_owned();
            if total > STDERR_TAIL as u64 {
                text = format!("[{total} байт stderr; хвост {STDERR_TAIL}] {text}");
            }
            text
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            stderr_thread: Some(stderr_thread),
            partial,
            final_path,
            buf: Vec::new(),
            bytes_fed: 0,
            finished: false,
            stderr_text: None,
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

impl FfmpegSink {
    /// Прервать кодирование: закрыть stdin, дать ffmpeg штатно завершиться
    /// (ограничение ~5 с), при необходимости убить ТОЛЬКО свой дочерний
    /// процесс; всегда wait() и join stderr-потока. Черновик остаётся.
    pub fn abort(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() > deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => {
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        // собрать поток stderr (после закрытия pipe он завершится)
        if let Some(t) = self.stderr_thread.take() {
            self.stderr_text = t.join().ok();
        }
        self.finished = true;
        Ok(())
    }
}

impl Drop for FfmpegSink {
    /// Защита на случай выхода без finalize/abort (отмена, ранний return):
    /// процесс дожидается/убивается, потоки присоединяются; `.partial`
    /// НЕ удаляется.
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.abort();
        }
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
        self.stderr_text = Some(stderr.clone());
        self.finished = true;
        if !status.success() {
            bail!(
                "ffmpeg завершился с ошибкой (код {:?}): {} — черновик сохранён: {}",
                status.code(),
                stderr.trim(),
                self.partial.display()
            );
        }
        // публикация только после успешного кодирования: MoveFileW без
        // замены; при занятом имени — отказ, черновик остаётся
        publish_no_replace(&self.partial, &self.final_path)?;
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
    fn partial_naming_is_unique_and_explicit() {
        let a = unique_partial_path(Path::new("/out/book.mp3"), "mp3");
        let b = unique_partial_path(Path::new("/out/book.mp3"), "mp3");
        assert_ne!(a, b, "каждый запуск — свой черновик");
        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(name.starts_with("book."), "{name}");
            assert!(name.contains(".partial.mp3"), "{name}");
            assert_eq!(p.parent(), Some(Path::new("/out")));
        }
        let o = unique_partial_path(Path::new(r"C:\outook.opus"), "opus");
        assert!(o.to_string_lossy().contains(".partial.opus"));
    }

    /// Из двух процессов: производитель флуда stderr (запускается харнессом
    /// дочернего процесса с env SHTORM_SINK_FLOOD_PRODUCER=1).
    #[test]
    fn stderr_flood_producer() {
        use std::io::Write;
        if std::env::var("SHTORM_SINK_FLOOD_PRODUCER").is_err() {
            return;
        }
        let mut err = std::io::stderr();
        let block = vec![b'A'; 8192];
        for _ in 0..16 {
            // 128 КиБ — заметно больше буфера pipe
            let _ = err.write_all(&block);
        }
        let _ = err.write_all(
            b"FLOOD_END
",
        );
        let _ = err.flush();
    }

    /// Сценарий флуда: FfmpegSink читает stderr ДО EOF — продюсер не должен
    /// заблокироваться; хвост ограничен. Выполняется ТОЛЬКО в дочернем
    /// процессе (env-маркер): зависание здесь не блокирует основной прогон.
    #[test]
    fn stderr_flood_child_entry() {
        if std::env::var("SHTORM_SINK_STDERR_CHILD").is_err() {
            return;
        }
        let exe = std::env::current_exe().expect("current_exe");
        let d = unique_dir("flood");
        let final_p = d.join("out.mp3");
        let partial = unique_partial_path(&final_p, "mp3");
        let args: Vec<String> = vec![
            "--exact".into(),
            "sink::tests::stderr_flood_producer".into(),
            "--test-threads=1".into(),
        ];
        let mut sink = FfmpegSink::spawn_with_env(
            &exe,
            &args,
            &[("SHTORM_SINK_FLOOD_PRODUCER", "1")],
            partial.clone(),
            final_p.clone(),
        )
        .expect("spawn producer");
        // пишем немного PCM (продюсер stdin не читает — укладывается в буфер)
        let _ = sink.write_chunk(0, &[0.01f32; 4800], None);
        // abort: закрыть stdin, дожать, собрать stderr-хвост
        sink.abort().expect("abort");
        let tail = sink.stderr_text.clone().unwrap_or_default();
        assert!(
            tail.contains("FLOOD_END"),
            "хвост должен содержать конец флуда (дренаж до EOF): {tail:.80}"
        );
        let total_shown = tail.len();
        assert!(
            total_shown <= STDERR_TAIL + 64,
            "в памяти только ограниченный хвост ({total_shown} > {} + префикс)",
            STDERR_TAIL
        );
        println!("CHILD_OK stderr_tail={total_shown}");
    }

    /// Внешний таймаут вокруг сценария флуда: если дренаж stderr сломан,
    /// продюсер блокируется на заполненном pipe — дочерний процесс убивается,
    /// тест падает с явной диагностикой (основной прогон не зависает).
    #[test]
    fn stderr_flood_no_deadlock_with_timeout() {
        if std::env::var("SHTORM_SINK_STDERR_CHILD").is_ok() {
            return;
        }
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(exe)
            .args([
                "--exact",
                "sink::tests::stderr_flood_child_entry",
                "--test-threads=1",
            ])
            .env("SHTORM_SINK_STDERR_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => {
                    assert!(status.success(), "дочерний сценарий флуда упал: {status:?}");
                    break;
                }
                None => {
                    if std::time::Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("дочерний сценарий флуда завис — убит по таймауту 60с");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
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
        let partial = unique_partial_path(&final_p, "mp3");
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
    fn drop_without_finalize_reaps_process_and_keeps_partial() {
        let Some(ff) = test_ffmpeg() else {
            eprintln!("SKIP drop_without_finalize: ffmpeg недоступен");
            return;
        };
        let d = unique_dir("drop");
        let final_p = d.join("out.mp3");
        let t0 = std::time::Instant::now();
        {
            let mut sink =
                FfmpegSink::create(&final_p, 24000, OutputFormat::Mp3, "-V2", &ff).unwrap();
            sink.write_chunk(0, &[0.05f32; 9600], None).unwrap();
            // Drop без finalize: abort внутри — процесс дожат/убит по таймауту
        }
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(15),
            "Drop не должен зависать"
        );
        assert!(!final_p.exists(), "итог при Drop не публикуется");
    }

    #[test]
    fn abort_keeps_partial_and_reports_no_final() {
        let Some(ff) = test_ffmpeg() else {
            eprintln!("SKIP abort_keeps_partial: ffmpeg недоступен");
            return;
        };
        let d = unique_dir("abort");
        let final_p = d.join("out.mp3");
        let mut sink = FfmpegSink::create(&final_p, 24000, OutputFormat::Mp3, "-V2", &ff).unwrap();
        sink.write_chunk(0, &[0.05f32; 9600], None).unwrap();
        let partial = sink.partial_path();
        sink.abort().expect("abort без ошибок");
        assert!(!final_p.exists(), "итог при отмене не публикуется");
        // ffmpeg обычно успевает финализировать контейнер — partial сохраняется
        assert!(
            partial.exists(),
            "черновик остаётся для диагностики: {partial:?}"
        );
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
        assert!(!unique_partial_path(&final_p, "mp3").exists());
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
