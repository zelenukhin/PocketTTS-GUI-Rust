//! G3: безопасный пакетный режим.
//!
//! - снимок входных файлов ДО генерации (стабильный порядок);
//! - рекурсивный обход `.txt/.md/.log` без следования символическим ссылкам;
//! - выходная папка исключается из обхода, если лежит внутри исходной;
//! - относительная структура подкаталогов сохраняется в папке результата;
//! - выходные пути проверяются на выход за пределы каталога назначения;
//! - существующие результаты не перезаписываются (Skipped);
//! - ошибка одного файла не прерывает остальные (изоляция), отмена — прерывает.
//!
//! Модуль не знает про ONNX: синтез одного файла инжектируется замыканием,
//! поэтому вся логика планирования/отчёта тестируется на моках.

use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Расширения, которые обрабатываются в пакетном режиме (как Python GUI).
pub const BATCH_EXTENSIONS: &[&str] = &["txt", "md", "log"];

/// Один входной файл и его путь относительно корня исходного каталога.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchFile {
    pub input: PathBuf,
    pub relative: PathBuf,
}

/// Снимок пакетного задания: список файлов зафиксирован до начала генерации.
#[derive(Debug, Clone)]
pub struct BatchPlan {
    pub source_root: PathBuf,
    pub out_root: PathBuf,
    pub files: Vec<BatchFile>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Completed,
    Failed,
    Skipped,
    Cancelled,
}

impl FileStatus {
    pub fn label(&self) -> &'static str {
        match self {
            FileStatus::Completed => "готово",
            FileStatus::Failed => "ошибка",
            FileStatus::Skipped => "пропущено",
            FileStatus::Cancelled => "отменено",
        }
    }
}

/// Итог по одному файлу: статус, причина и все пути для диагностики.
#[derive(Debug, Clone)]
pub struct FileReport {
    pub input: PathBuf,
    pub relative: PathBuf,
    pub output: Option<PathBuf>,
    pub status: FileStatus,
    pub reason: Option<String>,
    pub final_path: Option<PathBuf>,
    pub partial_path: Option<PathBuf>,
    pub diag_path: Option<PathBuf>,
    pub chunk_log_path: Option<PathBuf>,
    pub chunks_done: usize,
    pub chunks_total: usize,
}

impl FileReport {
    pub fn new(file: &BatchFile, output: Option<PathBuf>) -> Self {
        Self {
            input: file.input.clone(),
            relative: file.relative.clone(),
            output,
            status: FileStatus::Skipped,
            reason: None,
            final_path: None,
            partial_path: None,
            diag_path: None,
            chunk_log_path: None,
            chunks_done: 0,
            chunks_total: 0,
        }
    }

    pub fn skipped(file: &BatchFile, output: Option<PathBuf>, reason: impl Into<String>) -> Self {
        let mut r = Self::new(file, output);
        r.reason = Some(reason.into());
        r
    }

    pub fn failed(file: &BatchFile, output: Option<PathBuf>, reason: impl Into<String>) -> Self {
        let mut r = Self::new(file, output);
        r.status = FileStatus::Failed;
        r.reason = Some(reason.into());
        r
    }

    /// Строка отчёта: статус, вход → выход, причина, диагностические пути.
    pub fn line(&self) -> String {
        let out = self
            .output
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".into());
        let mut s = format!(
            "[{}] {} → {}",
            self.status.label(),
            self.input.display(),
            out
        );
        if let Some(r) = &self.reason {
            s.push_str(&format!(" ({r})"));
        }
        if let Some(p) = &self.partial_path {
            s.push_str(&format!("; черновик: {}", p.display()));
        }
        if let Some(p) = &self.diag_path {
            s.push_str(&format!("; диагностика: {}", p.display()));
        }
        if let Some(p) = &self.chunk_log_path {
            s.push_str(&format!("; лог чанков: {}", p.display()));
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchStatus {
    Completed,
    Failed,
    Cancelled,
}

/// Итоговый отчёт пакета: все файлы + агрегированные счётчики.
#[derive(Debug, Clone)]
pub struct BatchReport {
    pub files: Vec<FileReport>,
}

impl BatchReport {
    pub fn total(&self) -> usize {
        self.files.len()
    }

    pub fn count(&self, status: FileStatus) -> usize {
        self.files.iter().filter(|f| f.status == status).count()
    }

    pub fn completed(&self) -> usize {
        self.count(FileStatus::Completed)
    }

    pub fn failed(&self) -> usize {
        self.count(FileStatus::Failed)
    }

    pub fn skipped(&self) -> usize {
        self.count(FileStatus::Skipped)
    }

    pub fn cancelled(&self) -> bool {
        self.files.iter().any(|f| f.status == FileStatus::Cancelled)
    }

    pub fn status(&self) -> BatchStatus {
        if self.cancelled() {
            BatchStatus::Cancelled
        } else if self.completed() == 0 && self.failed() > 0 {
            BatchStatus::Failed
        } else {
            BatchStatus::Completed
        }
    }

    pub fn summary(&self) -> String {
        let mut s = format!(
            "ПАКЕТ: готово {} | ошибок {} | пропущено {} | всего {}",
            self.completed(),
            self.failed(),
            self.skipped(),
            self.total()
        );
        if self.cancelled() {
            s.push_str(" | ОТМЕНЕНО");
        }
        s
    }

    pub fn detail_lines(&self) -> Vec<String> {
        self.files.iter().map(FileReport::line).collect()
    }
}

/// События пакетного прогона (UI: файл K/N и чанки текущего файла).
#[derive(Debug, Clone)]
pub enum BatchEvent {
    Planned {
        total: usize,
        warnings: Vec<String>,
    },
    FileStarted {
        index: usize,
        total: usize,
        input: PathBuf,
        output: PathBuf,
    },
    FileProgress {
        index: usize,
        total: usize,
        done: usize,
        chunks_total: usize,
    },
    FileFinished {
        index: usize,
        total: usize,
        report: Box<FileReport>,
    },
    Log(String),
}

/// Абсолютный лексически нормализованный путь (файловая система не нужна —
/// выходная папка может ещё не существовать).
pub fn absolute_lexical(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    };
    lexical_normalize(&abs)
}

fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn norm_str(p: &Path) -> String {
    let s = absolute_lexical(p).to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

/// `child` равен `root` или лежит внутри него (без учёта регистра на Windows).
pub fn is_under(child: &Path, root: &Path) -> bool {
    let c = norm_str(child);
    let r = norm_str(root);
    if c == r {
        return true;
    }
    let mut prefix = r.clone();
    if !prefix.ends_with('\\') {
        prefix.push('\\');
    }
    c.starts_with(&prefix)
}

fn same_path(a: &Path, b: &Path) -> bool {
    norm_str(a) == norm_str(b)
}

fn has_batch_extension(name: &std::ffi::OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| BATCH_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// Построить план: один файл или рекурсивный обход каталога.
///
/// Список фиксируется здесь и больше не перечитывается; порядок стабилен
/// (относительный путь без учёта регистра, затем точный).
pub fn plan_batch(source: &Path, out_root: &Path) -> Result<BatchPlan> {
    let meta = std::fs::symlink_metadata(source)
        .with_context(|| format!("источник не найден: {}", source.display()))?;
    if meta.file_type().is_symlink() {
        bail!(
            "источник — символическая ссылка (не поддерживается): {}",
            source.display()
        );
    }
    let mut warnings = Vec::new();
    let (source_root, mut files) = if meta.is_file() {
        if !has_batch_extension(source.file_name().unwrap_or_default()) {
            bail!("выбранный файл не .txt/.md/.log: {}", source.display());
        }
        let root = source
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let relative = source
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("input"));
        (
            root,
            vec![BatchFile {
                input: source.to_path_buf(),
                relative,
            }],
        )
    } else if meta.is_dir() {
        let root = source.to_path_buf();
        let root_abs = absolute_lexical(&root);
        let out_abs = absolute_lexical(out_root);
        if same_path(&out_abs, &root_abs) {
            bail!(
                "выходная папка совпадает с исходной — выберите другую папку результата: {}",
                out_root.display()
            );
        }
        // Строгое исключение выходной папки нужно ТОЛЬКО когда она внутри
        // источника. Если выход — предок источника, исключать нельзя.
        let exclude_out = is_under(&out_abs, &root_abs);
        (
            root,
            collect_files(source, &out_abs, exclude_out, &mut warnings)?,
        )
    } else {
        bail!("источник не файл и не каталог: {}", source.display());
    };

    files.sort_by(|a, b| {
        let ka = a.relative.to_string_lossy().to_ascii_lowercase();
        let kb = b.relative.to_string_lossy().to_ascii_lowercase();
        ka.cmp(&kb).then_with(|| a.relative.cmp(&b.relative))
    });

    Ok(BatchPlan {
        source_root,
        out_root: out_root.to_path_buf(),
        files,
        warnings,
    })
}

fn collect_files(
    root: &Path,
    out_abs: &Path,
    exclude_out: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<BatchFile>> {
    let mut files = Vec::new();
    // DFS без рекурсии; каталоги-ссылки не раскрываются.
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(root.to_path_buf(), PathBuf::new())];
    while let Some((dir, rel)) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                warnings.push(format!("каталог не прочитан {}: {e}", dir.display()));
                continue;
            }
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warnings.push(format!(
                        "элемент каталога {} не прочитан: {e}",
                        dir.display()
                    ));
                    continue;
                }
            };
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(e) => {
                    warnings.push(format!("тип {} не определён: {e}", entry.path().display()));
                    continue;
                }
            };
            if ft.is_symlink() {
                warnings.push(format!(
                    "символическая ссылка пропущена: {}",
                    entry.path().display()
                ));
                continue;
            }
            let child = entry.path();
            let child_rel = rel.join(entry.file_name());
            if ft.is_dir() {
                if exclude_out && is_under(&child, out_abs) {
                    continue;
                }
                stack.push((child, child_rel));
            } else if ft.is_file() && has_batch_extension(&entry.file_name()) {
                files.push(BatchFile {
                    input: child,
                    relative: child_rel,
                });
            }
        }
    }
    Ok(files)
}

/// Безопасный выходной путь: `<out_root>/<относительный каталог>/<stem>.<ext>`.
/// Абсолютные пути, `..` и выход за пределы `out_root` — ошибка.
pub fn output_path_for(out_root: &Path, relative: &Path, ext: &str) -> Result<PathBuf> {
    if relative.is_absolute() {
        bail!(
            "относительный путь не должен быть абсолютным: {}",
            relative.display()
        );
    }
    for c in relative.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!(
                "недопустимый компонент в относительном пути: {}",
                relative.display()
            ),
        }
    }
    let stem = relative
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "output".into());
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let out = out_root.join(parent).join(format!("{stem}.{ext}"));
    let out_abs = absolute_lexical(&out);
    if !is_under(&out_abs, out_root) || same_path(&out_abs, &absolute_lexical(out_root)) {
        bail!(
            "выходной путь выходит за пределы каталога назначения: {}",
            out.display()
        );
    }
    Ok(out)
}

/// Прогнать план. `synth` инжектируется (в GUI — реальный `execute_job`,
/// в тестах — мок). Ошибки одного файла изолированы; отмена не запускает
/// следующий файл и помечает оставшиеся как Skipped.
pub fn run_batch<F>(
    plan: &BatchPlan,
    extension: &str,
    cancel: &AtomicBool,
    emit: &dyn Fn(BatchEvent),
    mut synth: F,
) -> BatchReport
where
    F: FnMut(&BatchFile, &Path, &AtomicBool, &mut dyn FnMut(usize, usize)) -> FileReport,
{
    let total = plan.files.len();
    emit(BatchEvent::Planned {
        total,
        warnings: plan.warnings.clone(),
    });
    for w in &plan.warnings {
        emit(BatchEvent::Log(format!("ВНИМАНИЕ: {w}")));
    }

    let mut reports = Vec::with_capacity(total);
    let mut stopped = false;
    for (idx, file) in plan.files.iter().enumerate() {
        if stopped || cancel.load(Ordering::SeqCst) {
            let out = output_path_for(&plan.out_root, &file.relative, extension).ok();
            let r = FileReport::skipped(file, out, "отменено до запуска");
            emit(BatchEvent::FileFinished {
                index: idx + 1,
                total,
                report: Box::new(r.clone()),
            });
            reports.push(r);
            continue;
        }
        let out = match output_path_for(&plan.out_root, &file.relative, extension) {
            Ok(p) => p,
            Err(e) => {
                let r = FileReport::failed(file, None, format!("{e:#}"));
                emit(BatchEvent::FileFinished {
                    index: idx + 1,
                    total,
                    report: Box::new(r.clone()),
                });
                reports.push(r);
                continue;
            }
        };
        emit(BatchEvent::FileStarted {
            index: idx + 1,
            total,
            input: file.input.clone(),
            output: out.clone(),
        });
        if out.exists() {
            let r = FileReport::skipped(
                file,
                Some(out),
                "результат уже существует — не перезаписываем",
            );
            emit(BatchEvent::FileFinished {
                index: idx + 1,
                total,
                report: Box::new(r.clone()),
            });
            reports.push(r);
            continue;
        }
        let mut progress = |done: usize, chunks_total: usize| {
            emit(BatchEvent::FileProgress {
                index: idx + 1,
                total,
                done,
                chunks_total,
            });
        };
        let report = synth(file, &out, cancel, &mut progress);
        let was_cancel = report.status == FileStatus::Cancelled;
        emit(BatchEvent::FileFinished {
            index: idx + 1,
            total,
            report: Box::new(report.clone()),
        });
        reports.push(report);
        if was_cancel || cancel.load(Ordering::SeqCst) {
            stopped = true;
        }
    }

    BatchReport { files: reports }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "shtorm_batch_{}_{}_{}",
            std::process::id(),
            tag,
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn mock_ok(
        file: &BatchFile,
        out: &Path,
        _cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, usize),
    ) -> FileReport {
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(out, b"mock-audio").unwrap();
        progress(1, 1);
        let mut r = FileReport::new(file, Some(out.to_path_buf()));
        r.status = FileStatus::Completed;
        r.final_path = Some(out.to_path_buf());
        r.chunks_done = 1;
        r.chunks_total = 1;
        r
    }

    #[test]
    fn plan_nested_dirs_stable_order_and_extensions() {
        let d = tmpdir("nested");
        let src = d.join("src");
        write(&src.join("b.md"), b"b");
        write(&src.join("a.txt"), b"a");
        write(&src.join("sub").join("c.log"), b"c");
        write(&src.join("sub").join("deep").join("d.txt"), b"d");
        write(&src.join("skip.bin"), b"x");
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        let rels: Vec<String> = plan
            .files
            .iter()
            .map(|f| f.relative.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(rels, vec!["a.txt", "b.md", "sub/c.log", "sub/deep/d.txt"]);
        assert_eq!(plan.files.len(), 4);
    }

    #[test]
    fn same_names_in_different_subdirs_are_distinct() {
        let d = tmpdir("same");
        let src = d.join("src");
        write(&src.join("x").join("a.txt"), b"x");
        write(&src.join("y").join("a.txt"), b"y");
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        let out_x = output_path_for(&plan.out_root, &plan.files[0].relative, "wav").unwrap();
        let out_y = output_path_for(&plan.out_root, &plan.files[1].relative, "wav").unwrap();
        assert_ne!(out_x, out_y);
        assert!(out_x.ends_with(Path::new("x").join("a.wav")));
        assert!(out_y.ends_with(Path::new("y").join("a.wav")));
    }

    #[test]
    fn output_dir_inside_source_is_excluded() {
        let d = tmpdir("inside");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        let out = src.join("out");
        // старый результат/лог в выходной папке не должен попасть в снимок
        write(&out.join("old.txt"), b"old");
        write(&out.join("nested").join("old.md"), b"old");
        let plan = plan_batch(&src, &out).unwrap();
        let rels: Vec<String> = plan
            .files
            .iter()
            .map(|f| f.relative.to_string_lossy().to_string())
            .collect();
        assert_eq!(rels, vec!["a.txt"], "{rels:?}");
    }

    #[test]
    fn output_equal_source_is_error() {
        let d = tmpdir("equal");
        write(&d.join("src").join("a.txt"), b"a");
        let err = plan_batch(&d.join("src"), &d.join("src")).unwrap_err();
        assert!(format!("{err:#}").contains("совпадает"), "{err:#}");
    }

    #[test]
    fn output_ancestor_of_source_does_not_exclude_everything() {
        let d = tmpdir("ancestor");
        let src = d.join("src");
        write(&src.join("sub").join("a.txt"), b"a");
        let plan = plan_batch(&src, &d).unwrap();
        assert_eq!(plan.files.len(), 1);
    }

    #[test]
    fn output_path_rejects_escape() {
        let out = Path::new("C:/out");
        assert!(output_path_for(out, Path::new("../evil.txt"), "wav").is_err());
        assert!(output_path_for(out, Path::new("sub/../../evil.txt"), "wav").is_err());
        assert!(output_path_for(out, Path::new("sub/a.txt"), "wav").is_ok());
    }

    #[test]
    fn existing_result_is_skipped_without_calling_synth() {
        let d = tmpdir("exists");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        write(&src.join("b.txt"), b"b");
        let out = d.join("out");
        write(&out.join("a.wav"), b"already");
        let plan = plan_batch(&src, &out).unwrap();
        let cancel = AtomicBool::new(false);
        let mut called = 0usize;
        let report = run_batch(&plan, "wav", &cancel, &|_| {}, |file, o, c, p| {
            called += 1;
            mock_ok(file, o, c, p)
        });
        assert_eq!(called, 1, "существующий результат не синтезируется");
        assert_eq!(report.skipped(), 1);
        assert_eq!(report.completed(), 1);
        assert!(report.files.iter().any(|f| f.status == FileStatus::Skipped
            && f.reason.as_deref().unwrap().contains("существует")));
    }

    #[test]
    fn one_failure_does_not_stop_others() {
        let d = tmpdir("isolation");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        write(&src.join("bad.txt"), b"bad");
        write(&src.join("c.txt"), b"c");
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        let cancel = AtomicBool::new(false);
        let report = run_batch(&plan, "wav", &cancel, &|_| {}, |file, o, c, p| {
            if file.input.ends_with("bad.txt") {
                FileReport::failed(file, Some(o.to_path_buf()), "мок-сбой")
            } else {
                mock_ok(file, o, c, p)
            }
        });
        assert_eq!(report.completed(), 2);
        assert_eq!(report.failed(), 1);
        assert_eq!(report.skipped(), 0);
        assert_eq!(report.status(), BatchStatus::Completed);
        let bad = report
            .files
            .iter()
            .find(|f| f.input.ends_with("bad.txt"))
            .unwrap();
        assert_eq!(bad.status, FileStatus::Failed);
        assert!(bad.reason.as_deref().unwrap().contains("мок-сбой"));
    }

    #[test]
    fn cancel_stops_current_and_skips_remaining() {
        let d = tmpdir("cancel");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        write(&src.join("b.txt"), b"b");
        write(&src.join("c.txt"), b"c");
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        let cancel = AtomicBool::new(false);
        let report = run_batch(&plan, "wav", &cancel, &|_| {}, |file, o, c, p| {
            // отмена во время первого файла: он Cancelled, остальные Skipped
            c.store(true, Ordering::SeqCst);
            let mut r = FileReport::new(file, Some(o.to_path_buf()));
            r.status = FileStatus::Cancelled;
            r.reason = Some("мок-отмена".into());
            p(1, 2);
            r
        });
        assert_eq!(report.total(), 3);
        assert_eq!(report.count(FileStatus::Cancelled), 1);
        assert_eq!(report.skipped(), 2);
        assert_eq!(report.status(), BatchStatus::Cancelled);
        assert!(report
            .files
            .iter()
            .skip(1)
            .all(|f| f.reason.as_deref() == Some("отменено до запуска")));
    }

    #[test]
    fn rerun_after_cancel_processes_remaining() {
        let d = tmpdir("rerun");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        write(&src.join("b.txt"), b"b");
        let out = d.join("out");
        let plan = plan_batch(&src, &out).unwrap();

        // первый прогон: a готов, затем отмена перед b
        let cancel1 = AtomicBool::new(false);
        let r1 = run_batch(&plan, "wav", &cancel1, &|_| {}, |file, o, c, p| {
            if file.input.ends_with("a.txt") {
                let r = mock_ok(file, o, c, p);
                c.store(true, Ordering::SeqCst);
                r
            } else {
                let mut r = FileReport::new(file, Some(o.to_path_buf()));
                r.status = FileStatus::Cancelled;
                r
            }
        });
        assert_eq!(r1.completed(), 1);
        assert_eq!(r1.skipped(), 1);
        assert!(out.join("a.wav").is_file());

        // повторный прогон: a пропущен (результат есть), b доделан
        let cancel2 = AtomicBool::new(false);
        let r2 = run_batch(&plan, "wav", &cancel2, &|_| {}, mock_ok);
        assert_eq!(r2.skipped(), 1);
        assert_eq!(r2.completed(), 1);
        assert!(out.join("b.wav").is_file());
    }

    #[test]
    fn single_file_selection_is_supported() {
        let d = tmpdir("single");
        let f = d.join("book.txt");
        write(&f, b"text");
        let plan = plan_batch(&f, &d.join("out")).unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].relative, PathBuf::from("book.txt"));
        let out = output_path_for(&plan.out_root, &plan.files[0].relative, "wav").unwrap();
        assert!(out.ends_with("book.wav"));
        // не-текстовый файл — ошибка
        let bin = d.join("x.bin");
        write(&bin, b"x");
        assert!(plan_batch(&bin, &d.join("out")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn symlink_is_not_followed() {
        use std::os::windows::fs::symlink_file;
        let d = tmpdir("symlink");
        let src = d.join("src");
        write(&src.join("real.txt"), b"real");
        let link = src.join("link.txt");
        if symlink_file(src.join("real.txt"), &link).is_err() {
            eprintln!("SKIP symlink_is_not_followed: нет прав на создание ссылки");
            return;
        }
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        assert_eq!(plan.files.len(), 1, "ссылка не попала в снимок");
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("символическая ссылка")));
    }
}
