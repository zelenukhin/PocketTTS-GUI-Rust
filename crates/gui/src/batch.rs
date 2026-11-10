//! G3: безопасный пакетный режим.
//!
//! - снимок входных файлов ДО генерации (стабильный порядок);
//! - рекурсивный обход `.txt/.md/.log` без следования символическим ссылкам;
//! - выходная папка исключается из обхода, если лежит внутри исходной;
//! - относительная структура подкаталогов сохраняется в папке результата;
//! - выходные пути проверяются на выход за пределы каталога назначения;
//! - существующие результаты по умолчанию пропускаются (Skipped); G5
//!   `overwrite=true` разрешает замену — синтез идёт, а sink заменяет
//!   готовый файл только после успешного завершения;
//! - конфликты имён между РАЗНЫМИ входами остаются запрещены даже при
//!   overwrite (chapter.txt + chapter.md не перезаписывают друг друга);
//! - ошибка одного файла не прерывает остальные (изоляция), отмена — прерывает.
//!
//! Модуль не знает про ONNX: синтез одного файла инжектируется замыканием,
//! поэтому вся логика планирования/отчёта тестируется на моках.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
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

/// Коллизия выходных имён: несколько входов дают один и тот же результат
/// (`chapter.txt` + `chapter.md` → `chapter.<ext>`), в том числе совпадение
/// без учёта регистра (Windows). `output` — путь без расширения результата.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputConflict {
    pub output: PathBuf,
    pub inputs: Vec<PathBuf>,
}

/// Снимок пакетного задания: список файлов зафиксирован до начала генерации.
#[derive(Debug, Clone)]
pub struct BatchPlan {
    pub source_root: PathBuf,
    pub out_root: PathBuf,
    pub files: Vec<BatchFile>,
    pub warnings: Vec<String>,
    /// Конфликты имён обнаружены ДО генерации; такие файлы не синтезируются.
    pub conflicts: Vec<OutputConflict>,
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
    /// Есть и успешные файлы, и ошибки — полноценным успехом это не считается.
    Partial,
    Failed,
    Cancelled,
    /// Нечего синтезировать: пустой план или все файлы пропущены.
    NoWork,
}

/// Итоговый отчёт пакета: все файлы + агрегированные счётчики.
#[derive(Debug, Clone)]
pub struct BatchReport {
    pub files: Vec<FileReport>,
    /// Конфликты имён, обнаруженные при построении плана.
    pub conflicts: Vec<OutputConflict>,
    /// Пользователь отменил пакет (флаг НЕ выводится из статусов файлов:
    /// отмена может совпасть с ошибкой текущего файла или произойти до старта).
    pub cancelled: bool,
    /// G6.1: постоянный пул — сколько Pocket реально загружено на пакет
    /// (None — последовательный путь, модель создаётся на каждый файл).
    pub pool_loads: Option<usize>,
    /// G6.1: число постоянных воркеров пула (файлов обрабатывается
    /// одновременно столько же).
    pub pool_workers: Option<usize>,
}

impl BatchReport {
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            conflicts: Vec::new(),
            cancelled: false,
            pool_loads: None,
            pool_workers: None,
        }
    }

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

    /// Отмена пакета: явный флаг ИЛИ файл, сообщивший Cancelled. Ошибка
    /// текущего файла сама по себе отмену не отменяет.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled || self.count(FileStatus::Cancelled) > 0
    }

    pub fn status(&self) -> BatchStatus {
        if self.is_cancelled() {
            return BatchStatus::Cancelled;
        }
        if self.total() == 0 {
            return BatchStatus::NoWork;
        }
        if self.completed() == 0 {
            return if self.failed() > 0 {
                BatchStatus::Failed
            } else {
                BatchStatus::NoWork
            };
        }
        if self.failed() > 0 {
            BatchStatus::Partial
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
        if !self.conflicts.is_empty() {
            s.push_str(&format!(" | конфликтов имён: {}", self.conflicts.len()));
        }
        if let (Some(w), Some(l)) = (self.pool_workers, self.pool_loads) {
            s.push_str(&format!(
                " | пул: {w} воркер(ов), Pocket загружен {l} раз(а)"
            ));
        }
        match self.status() {
            BatchStatus::Cancelled => s.push_str(" | ОТМЕНЕНО"),
            BatchStatus::Partial => s.push_str(" | ЧАСТИЧНЫЙ УСПЕХ — есть ошибки"),
            BatchStatus::NoWork => s.push_str(" | НЕЧЕГО ДЕЛАТЬ — синтез не выполнялся"),
            BatchStatus::Failed => s.push_str(" | ОШИБКА — ни один файл не готов"),
            BatchStatus::Completed => {}
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

// ===== физическая проверка путей (G3.1) =====
// Лексической нормализации недостаточно: symlink/junction/reparse point может
// направить запись за пределы выбранного каталога. Политика — отказ, а не
// автоматическое следование по такой точке.

#[cfg(windows)]
pub(crate) fn is_reparse_metadata(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(crate) fn is_reparse_metadata(md: &std::fs::Metadata) -> bool {
    md.file_type().is_symlink()
}

/// Проверить сам компонент, не переходя по ссылке.
fn is_reparse_point(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|md| is_reparse_metadata(&md))
        .unwrap_or(false)
}

/// Отказ, если любой существующий компонент пути — ссылка/точка
/// перенаправления. Проверяются все предки (включая сам путь, если существует).
pub fn ensure_no_reparse_components(path: &Path) -> Result<()> {
    let abs = absolute_lexical(path);
    let mut chain: Vec<&Path> = abs.ancestors().collect();
    chain.reverse();
    for anc in chain {
        if anc.as_os_str().is_empty() {
            continue;
        }
        match std::fs::symlink_metadata(anc) {
            Ok(md) => {
                if is_reparse_metadata(&md) {
                    bail!(
                        "путь проходит через ссылку/точку перенаправления (reparse point): {} — укажите физический каталог",
                        anc.display()
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // дальнейшие компоненты тоже не существуют
            }
            Err(e) => bail!("проверить компонент пути {}: {e}", anc.display()),
        }
    }
    Ok(())
}

/// Физический путь: разрешает существующий префикс (canonicalize), а
/// несуществующий хвост добавляет лексически. Нужен для сравнения путей,
/// скрытых за альтернативными именами/junction.
pub fn physical_path(path: &Path) -> PathBuf {
    let abs = absolute_lexical(path);
    for anc in abs.ancestors() {
        if let Ok(canon) = std::fs::canonicalize(anc) {
            let rest = abs.strip_prefix(anc).unwrap_or_else(|_| Path::new(""));
            return lexical_normalize(&canon.join(rest));
        }
    }
    abs
}

fn physical_components(path: &Path) -> Vec<String> {
    physical_path(path)
        .components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy().to_string();
            if cfg!(windows) {
                s.to_ascii_lowercase()
            } else {
                s
            }
        })
        .collect()
}

/// `child` физически равен `root` или лежит внутри него (component-wise,
/// без опоры только на строковый starts_with лексического пути).
pub fn physically_under(child: &Path, root: &Path) -> bool {
    let c = physical_components(child);
    let r = physical_components(root);
    !r.is_empty() && c.len() >= r.len() && c[..r.len()] == r[..]
}

pub fn physically_same(a: &Path, b: &Path) -> bool {
    physical_components(a) == physical_components(b)
}

fn has_batch_extension(name: &std::ffi::OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| BATCH_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// Путь результата без расширения: `<out_root>/<относительный каталог>/<stem>`.
/// Нужен для обнаружения конфликтов имён ДО генерации (расширение одинаково
/// для всех файлов текущего запуска).
fn output_stem_path(out_root: &Path, relative: &Path) -> PathBuf {
    let stem = relative
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "output".into());
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    out_root.join(parent).join(stem)
}

fn find_conflicts(out_root: &Path, files: &[BatchFile]) -> Vec<OutputConflict> {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, f) in files.iter().enumerate() {
        // norm_str приводит к абсолютному виду и нижнему регистру на Windows —
        // `chapter.txt` и `CHAPTER.md` попадают в одну группу.
        let key = norm_str(&output_stem_path(out_root, &f.relative));
        groups.entry(key).or_default().push(i);
    }
    let mut conflicts = Vec::new();
    for idxs in groups.values().filter(|v| v.len() > 1) {
        let output = output_stem_path(out_root, &files[idxs[0]].relative);
        let inputs = idxs.iter().map(|&i| files[i].input.clone()).collect();
        conflicts.push(OutputConflict { output, inputs });
    }
    conflicts
}

/// Построить план: один файл или рекурсивный обход каталога.
///
/// Список фиксируется здесь и больше не перечитывается; порядок стабилен
/// (относительный путь без учёта регистра, затем точный). Конфликты выходных
/// имён обнаруживаются на этом же шаге — до первого синтеза.
pub fn plan_batch(source: &Path, out_root: &Path) -> Result<BatchPlan> {
    let meta = std::fs::symlink_metadata(source)
        .with_context(|| format!("источник не найден: {}", source.display()))?;
    if is_reparse_metadata(&meta) {
        bail!(
            "источник — ссылка/точка перенаправления (reparse point), не поддерживается: {}",
            source.display()
        );
    }
    // Выходная папка не должна проходить через symlink/junction: иначе
    // лексическая проверка «внутри out_root» ничего не гарантирует.
    ensure_no_reparse_components(out_root)?;
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
        // not only textual equality: the same directory can be reached via
        // symlink/junction/alternative spelling.
        if same_path(&out_abs, &root_abs) || physically_same(out_root, &root) {
            bail!(
                "выходная папка физически совпадает с исходной (в т.ч. через ссылку/альтернативный путь) — выберите другую папку результата: {}",
                out_root.display()
            );
        }
        // Строгое исключение выходной папки нужно ТОЛЬКО когда она внутри
        // источника. Если выход — предок источника, исключать нельзя.
        let exclude_out = is_under(&out_abs, &root_abs) || physically_under(out_root, &root);
        (
            root,
            collect_files(source, &out_abs, out_root, exclude_out, &mut warnings)?,
        )
    } else {
        bail!("источник не файл и не каталог: {}", source.display());
    };

    files.sort_by(|a, b| {
        let ka = a.relative.to_string_lossy().to_ascii_lowercase();
        let kb = b.relative.to_string_lossy().to_ascii_lowercase();
        ka.cmp(&kb).then_with(|| a.relative.cmp(&b.relative))
    });

    let conflicts = find_conflicts(out_root, &files);
    for c in &conflicts {
        let names = c
            .inputs
            .iter()
            .map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.display().to_string())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let out_name = c
            .output
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| c.output.display().to_string());
        warnings.push(format!(
            "КОНФЛИКТ выходных имён: {names} → {out_name}.<формат> — синтез этих файлов не запускается"
        ));
    }

    Ok(BatchPlan {
        source_root,
        out_root: out_root.to_path_buf(),
        files,
        warnings,
        conflicts,
    })
}

fn collect_files(
    root: &Path,
    out_abs: &Path,
    out_root: &Path,
    exclude_out: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<BatchFile>> {
    let mut files = Vec::new();
    // DFS без рекурсии; каталоги-ссылки/точки перенаправления не раскрываются.
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
            let child = entry.path();
            if is_reparse_point(&child) {
                warnings.push(format!(
                    "ссылка/точка перенаправления пропущена: {}",
                    child.display()
                ));
                continue;
            }
            let child_rel = rel.join(entry.file_name());
            if ft.is_dir() {
                // Физическое сравнение ловит выходную папку, заданную через
                // альтернативный путь (source — alias, out — реальный путь).
                if exclude_out && (is_under(&child, out_abs) || physically_under(&child, out_root))
                {
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
    // Даже при лексически корректном пути существующий компонент может быть
    // ссылкой/точкой перенаправления — запись по такому пути запрещена.
    ensure_no_reparse_components(&out)?;
    Ok(out)
}

fn conflict_path_with_ext(stem_path: &Path, ext: &str) -> PathBuf {
    let name = stem_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".into());
    stem_path.with_file_name(format!("{name}.{ext}"))
}

/// Файл, готовый к синтезу: все предзапусковые проверки плана пройдены
/// (не конфликт, путь результата безопасен лексически, существующего
/// результата нет либо разрешена перезапись).
#[derive(Debug, Clone)]
pub struct PreparedFile {
    /// Индекс в `plan.files` (0-based); K для событий = index + 1.
    pub index: usize,
    pub input: PathBuf,
    pub relative: PathBuf,
    pub out: PathBuf,
}

/// Результат предзапусковых проверок пакета: отчёты всех файлов плана
/// (index-aligned) и список файлов, готовых к синтезу.
#[derive(Debug, Clone)]
pub struct PreparedBatch {
    pub reports: Vec<FileReport>,
    pub to_synth: Vec<PreparedFile>,
    /// Конфликты с уже подставленным расширением результата.
    pub conflicts: Vec<OutputConflict>,
}

/// Предзапусковые проверки пакета (G6.1): конфликты имён, безопасность
/// выходного пути и существующие результаты разрешаются ДО запуска воркеров.
/// Файлы с проблемами получают готовый FileReport (+FileFinished);
/// синтезируемые — плейсхолдер, который заменит раннер.
pub fn preflight_batch(
    plan: &BatchPlan,
    extension: &str,
    overwrite: bool,
    cancel: &AtomicBool,
    emit: &dyn Fn(BatchEvent),
) -> PreparedBatch {
    let total = plan.files.len();
    emit(BatchEvent::Planned {
        total,
        warnings: plan.warnings.clone(),
    });
    for w in &plan.warnings {
        emit(BatchEvent::Log(format!("ВНИМАНИЕ: {w}")));
    }

    let mut conflict_by_key: BTreeMap<String, usize> = BTreeMap::new();
    for (i, c) in plan.conflicts.iter().enumerate() {
        conflict_by_key.insert(norm_str(&c.output), i);
    }
    // Точные пути конфликтов сообщаем ДО первого синтеза.
    for c in &plan.conflicts {
        let out = conflict_path_with_ext(&c.output, extension);
        let names = c
            .inputs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        emit(BatchEvent::Log(format!(
            "КОНФЛИКТ ВЫХОДНЫХ ИМЁН: {names} → {} — синтез не запускается",
            out.display()
        )));
    }

    let mut reports = Vec::with_capacity(total);
    let mut to_synth = Vec::new();
    let cancelled_upfront = cancel.load(Ordering::SeqCst);
    for (idx, file) in plan.files.iter().enumerate() {
        let report = if cancelled_upfront {
            let out = output_path_for(&plan.out_root, &file.relative, extension).ok();
            FileReport::skipped(file, out, "отменено до запуска")
        } else if let Some(ci) =
            conflict_by_key.get(&norm_str(&output_stem_path(&plan.out_root, &file.relative)))
        {
            let c = &plan.conflicts[*ci];
            let conflict_out = conflict_path_with_ext(&c.output, extension);
            let names = c
                .inputs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let out = output_path_for(&plan.out_root, &file.relative, extension).ok();
            FileReport::skipped(
                file,
                out,
                format!(
                    "конфликт выходных имён: {names} → {} — синтез не запускался",
                    conflict_out.display()
                ),
            )
        } else {
            match output_path_for(&plan.out_root, &file.relative, extension) {
                Ok(out) => {
                    if out.exists() && !overwrite {
                        FileReport::skipped(
                            file,
                            Some(out),
                            "результат уже существует — перезапись выключена",
                        )
                    } else {
                        to_synth.push(PreparedFile {
                            index: idx,
                            input: file.input.clone(),
                            relative: file.relative.clone(),
                            out: out.clone(),
                        });
                        FileReport::new(file, Some(out))
                    }
                }
                Err(e) => FileReport::failed(file, None, format!("{e:#}")),
            }
        };
        let is_placeholder = report.status == FileStatus::Skipped && report.reason.is_none();
        if !is_placeholder {
            emit(BatchEvent::FileFinished {
                index: idx + 1,
                total,
                report: Box::new(report.clone()),
            });
        }
        reports.push(report);
    }

    let conflicts = plan
        .conflicts
        .iter()
        .map(|c| OutputConflict {
            output: conflict_path_with_ext(&c.output, extension),
            inputs: c.inputs.clone(),
        })
        .collect();
    PreparedBatch {
        reports,
        to_synth,
        conflicts,
    }
}

/// Быстрый подсчёт файлов-кандидатов на синтез (те же условия, что
/// `preflight_batch`, без событий) — для решения о постоянном пуле ДО
/// его создания (G6.1: не грузить 2 Pocket ради 1 файла).
pub fn count_actionable(plan: &BatchPlan, extension: &str, overwrite: bool) -> usize {
    let conflict_keys: std::collections::BTreeSet<String> =
        plan.conflicts.iter().map(|c| norm_str(&c.output)).collect();
    plan.files
        .iter()
        .filter(|f| {
            !conflict_keys.contains(&norm_str(&output_stem_path(&plan.out_root, &f.relative)))
        })
        .filter_map(|f| output_path_for(&plan.out_root, &f.relative, extension).ok())
        .filter(|out| overwrite || !out.exists())
        .count()
}

/// Собрать BatchReport из готовых отчётов + признака отмены.
fn assemble_report(prepared: PreparedBatch, cancelled: bool) -> BatchReport {
    BatchReport {
        files: prepared.reports,
        conflicts: prepared.conflicts,
        cancelled,
        pool_loads: None,
        pool_workers: None,
    }
}

/// Прогнать план ПОСЛЕДОВАТЕЛЬНО (существующий путь G3: пул воркеров
/// создаётся/освобождается на каждый файл внутри `synth`). `synth`
/// инжектируется (в GUI — реальный `execute_job`, в тестах — мок). Ошибки
/// одного файла изолированы; отмена не запускает следующий файл и помечает
/// оставшиеся как Skipped.
pub fn run_batch<F>(
    plan: &BatchPlan,
    extension: &str,
    overwrite: bool,
    cancel: &AtomicBool,
    emit: &dyn Fn(BatchEvent),
    synth: F,
) -> BatchReport
where
    F: FnMut(&BatchFile, &Path, &AtomicBool, &mut dyn FnMut(usize, usize)) -> FileReport,
{
    let prepared = preflight_batch(plan, extension, overwrite, cancel, emit);
    run_prepared_sequential(plan, prepared, cancel, emit, synth)
}

/// Последовательный раннер подготовленного пакета.
pub fn run_prepared_sequential<F>(
    plan: &BatchPlan,
    prepared: PreparedBatch,
    cancel: &AtomicBool,
    emit: &dyn Fn(BatchEvent),
    mut synth: F,
) -> BatchReport
where
    F: FnMut(&BatchFile, &Path, &AtomicBool, &mut dyn FnMut(usize, usize)) -> FileReport,
{
    let total = plan.files.len();
    let mut reports = prepared.reports;
    let mut stopped = false;
    for pf in prepared.to_synth {
        let idx = pf.index;
        let file = &plan.files[idx];
        if stopped || cancel.load(Ordering::SeqCst) {
            stopped = true;
            let r = FileReport::skipped(file, Some(pf.out.clone()), "отменено до запуска");
            emit(BatchEvent::FileFinished {
                index: idx + 1,
                total,
                report: Box::new(r.clone()),
            });
            reports[idx] = r;
            continue;
        }
        emit(BatchEvent::FileStarted {
            index: idx + 1,
            total,
            input: file.input.clone(),
            output: pf.out.clone(),
        });
        let mut progress = |done: usize, chunks_total: usize| {
            emit(BatchEvent::FileProgress {
                index: idx + 1,
                total,
                done,
                chunks_total,
            });
        };
        let report = synth(file, &pf.out, cancel, &mut progress);
        let was_cancel = report.status == FileStatus::Cancelled;
        emit(BatchEvent::FileFinished {
            index: idx + 1,
            total,
            report: Box::new(report.clone()),
        });
        reports[idx] = report;
        if was_cancel || cancel.load(Ordering::SeqCst) {
            stopped = true;
        }
    }
    // отмена до первого файла (preflight пометил всё Skipped) — пакет
    // обязан остаться Cancelled, а не NoWork
    let cancelled = stopped || cancel.load(Ordering::SeqCst);
    assemble_report(
        PreparedBatch {
            reports,
            to_synth: Vec::new(),
            conflicts: prepared.conflicts,
        },
        cancelled,
    )
}

/// G6.1: параллельный раннер подготовленного пакета на ПОСТОЯННОМ пуле
/// воркеров. Каждый воркер `slots` в собственном потоке берёт следующий файл
/// из общей динамической очереди и обрабатывает его ЦЕЛИКОМ (`synth`).
/// События файлов (Started/Progress/Finished) приходят из разных потоков
/// вперемешку — приёмник обязан различать их по индексу файла.
/// Отмена останавливает выдачу новых файлов; паника `synth` отравляет слот
/// (Pocket не переиспользуется), файл получает Failed, остальные файлы
/// разбирают живые воркеры.
pub fn run_prepared_parallel<W, S>(
    plan: &BatchPlan,
    prepared: PreparedBatch,
    cancel: &std::sync::Arc<AtomicBool>,
    slots: Vec<app_core::persistent::WorkerSlot<W>>,
    emit: &std::sync::Arc<dyn Fn(BatchEvent) + Send + Sync>,
    synth: S,
) -> BatchReport
where
    W: Send + 'static,
    S: Fn(&PreparedFile, usize, &app_core::persistent::WorkerSlot<W>) -> FileReport
        + Send
        + Sync
        + 'static,
{
    let total = plan.files.len();
    let to_synth = std::sync::Arc::new(prepared.to_synth);
    let conflicts = prepared.conflicts;
    let mut reports = prepared.reports;
    let to_synth_iter = std::sync::Arc::clone(&to_synth);

    let n = to_synth.len();
    let emit_q = std::sync::Arc::clone(emit);
    let synth = std::sync::Arc::new(synth);
    let job = move |file_pos: usize, wid: usize, slot: &app_core::persistent::WorkerSlot<W>| {
        let pf = &to_synth[file_pos];
        emit_q(BatchEvent::FileStarted {
            index: pf.index + 1,
            total,
            input: pf.input.clone(),
            output: pf.out.clone(),
        });
        let report = synth(pf, wid, slot);
        emit_q(BatchEvent::FileFinished {
            index: pf.index + 1,
            total,
            report: Box::new(report.clone()),
        });
        report
    };
    let mut outcome = app_core::persistent::run_dynamic_queue(slots, n, cancel, job);

    for (wid, fp, msg) in &outcome.worker_fatal {
        emit(BatchEvent::Log(format!(
            "ВНИМАНИЕ: фатальный сбой воркера {wid} на файле {}: {msg}",
            fp + 1
        )));
    }
    for (pos, pf) in to_synth_iter.iter().enumerate() {
        let report = match outcome.results[pos].take() {
            Some(r) => r,
            None => {
                // файл не был завершён: паника воркера на нём, отмена до
                // выдачи или исчерпание живых воркеров
                let file = &plan.files[pf.index];
                let r = if let Some((_, _, msg)) =
                    outcome.worker_fatal.iter().find(|(_, fp, _)| *fp == pos)
                {
                    FileReport::failed(
                        file,
                        Some(pf.out.clone()),
                        format!("фатальный сбой воркера: {msg}"),
                    )
                } else if cancel.load(Ordering::SeqCst) {
                    FileReport::skipped(file, Some(pf.out.clone()), "отменено до запуска")
                } else {
                    FileReport::skipped(
                        file,
                        Some(pf.out.clone()),
                        "не запущено: воркеры пула завершились",
                    )
                };
                // K/N-счётчик UI должен двигаться и на незапущенных файлах
                emit(BatchEvent::FileFinished {
                    index: pf.index + 1,
                    total,
                    report: Box::new(r.clone()),
                });
                r
            }
        };
        reports[pf.index] = report;
    }

    let cancelled =
        cancel.load(Ordering::SeqCst) || reports.iter().any(|r| r.status == FileStatus::Cancelled);
    let mut report = assemble_report(
        PreparedBatch {
            reports,
            to_synth: Vec::new(),
            conflicts,
        },
        cancelled,
    );
    report.pool_workers = Some(outcome.claimed.len().max(1));
    report
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
        let report = run_batch(&plan, "wav", false, &cancel, &|_| {}, |file, o, c, p| {
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
    fn overwrite_true_resynthesizes_existing_result() {
        let d = tmpdir("exists_overwrite");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        write(&src.join("b.txt"), b"b");
        let out = d.join("out");
        write(&out.join("a.wav"), b"already");
        let plan = plan_batch(&src, &out).unwrap();
        let cancel = AtomicBool::new(false);
        let mut called = 0usize;
        let report = run_batch(&plan, "wav", true, &cancel, &|_| {}, |file, o, c, p| {
            called += 1;
            mock_ok(file, o, c, p)
        });
        assert_eq!(
            called, 2,
            "при overwrite=true существующий результат синтезируется"
        );
        assert_eq!(report.skipped(), 0);
        assert_eq!(report.completed(), 2);
        assert_eq!(std::fs::read(out.join("a.wav")).unwrap(), b"mock-audio");
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
        let report = run_batch(&plan, "wav", false, &cancel, &|_| {}, |file, o, c, p| {
            if file.input.ends_with("bad.txt") {
                FileReport::failed(file, Some(o.to_path_buf()), "мок-сбой")
            } else {
                mock_ok(file, o, c, p)
            }
        });
        assert_eq!(report.completed(), 2);
        assert_eq!(report.failed(), 1);
        assert_eq!(report.skipped(), 0);
        // часть файлов готова, часть — с ошибками: это не полный успех
        assert_eq!(report.status(), BatchStatus::Partial);
        assert!(
            report.summary().contains("ЧАСТИЧНЫЙ УСПЕХ"),
            "{}",
            report.summary()
        );
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
        let report = run_batch(&plan, "wav", false, &cancel, &|_| {}, |file, o, c, p| {
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
        let r1 = run_batch(&plan, "wav", false, &cancel1, &|_| {}, |file, o, c, p| {
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
        assert_eq!(r1.status(), BatchStatus::Cancelled);
        assert!(r1.cancelled, "отмена после готового файла сохранена");
        assert!(out.join("a.wav").is_file());

        // повторный прогон: a пропущен (результат есть), b доделан
        let cancel2 = AtomicBool::new(false);
        let r2 = run_batch(&plan, "wav", false, &cancel2, &|_| {}, mock_ok);
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
            .any(|w| w.contains("ссылка/точка перенаправления пропущена")));
    }

    // ===== G3.1: отмена пакета (4 сценария) =====

    #[test]
    fn cancel_before_first_file_is_cancelled() {
        let d = tmpdir("cancel_first");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        write(&src.join("b.txt"), b"b");
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        let cancel = AtomicBool::new(true);
        let mut called = 0usize;
        let report = run_batch(&plan, "wav", false, &cancel, &|_| {}, |file, o, c, p| {
            called += 1;
            mock_ok(file, o, c, p)
        });
        assert_eq!(called, 0, "ни один файл не запускается");
        assert_eq!(report.skipped(), 2);
        assert_eq!(report.status(), BatchStatus::Cancelled);
        assert!(report.cancelled);
        assert!(report.summary().contains("ОТМЕНЕНО"));
        assert!(report
            .files
            .iter()
            .all(|f| f.reason.as_deref() == Some("отменено до запуска")));
    }

    #[test]
    fn cancel_after_one_completed_file_is_cancelled() {
        let d = tmpdir("cancel_after_ok");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        write(&src.join("b.txt"), b"b");
        write(&src.join("c.txt"), b"c");
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        let cancel = AtomicBool::new(false);
        let mut calls = Vec::new();
        let report = run_batch(&plan, "wav", false, &cancel, &|_| {}, |file, o, c, p| {
            calls.push(file.relative.clone());
            let r = mock_ok(file, o, c, p);
            // «Стоп» после первого успешного файла
            c.store(true, Ordering::SeqCst);
            r
        });
        assert_eq!(calls, vec![PathBuf::from("a.txt")]);
        assert_eq!(report.completed(), 1);
        assert_eq!(report.skipped(), 2);
        assert_eq!(report.status(), BatchStatus::Cancelled);
        assert!(report.cancelled);
    }

    #[test]
    fn cancel_concurrent_with_current_file_error_is_cancelled() {
        let d = tmpdir("cancel_error");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        write(&src.join("b.txt"), b"b");
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        let cancel = AtomicBool::new(false);
        let report = run_batch(&plan, "wav", false, &cancel, &|_| {}, |file, o, c, _p| {
            // «Стоп» пришёл одновременно с ошибкой файла
            c.store(true, Ordering::SeqCst);
            FileReport::failed(file, Some(o.to_path_buf()), "мок-сбой")
        });
        assert_eq!(report.failed(), 1);
        assert_eq!(report.skipped(), 1);
        assert_eq!(report.status(), BatchStatus::Cancelled);
        assert!(report.cancelled, "ошибка не маскирует факт отмены");
    }

    #[test]
    fn cancel_before_next_file_is_cancelled() {
        let d = tmpdir("cancel_next");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        write(&src.join("b.txt"), b"b");
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        let cancel = AtomicBool::new(false);
        let mut calls = 0usize;
        let report = run_batch(&plan, "wav", false, &cancel, &|_| {}, |file, o, c, p| {
            calls += 1;
            // отмена устанавливается ПОСЛЕ завершения файла, до проверки
            // следующего — следующий не должен стартовать
            let r = mock_ok(file, o, c, p);
            c.store(true, Ordering::SeqCst);
            r
        });
        assert_eq!(calls, 1);
        assert_eq!(report.completed(), 1);
        assert_eq!(report.skipped(), 1);
        assert_eq!(report.status(), BatchStatus::Cancelled);
    }

    // ===== G3.1: конфликты выходных имён =====

    #[test]
    fn stem_conflict_is_detected_before_generation_and_skipped() {
        let d = tmpdir("conflict_stem");
        let src = d.join("src");
        write(&src.join("chapter.txt"), b"a");
        write(&src.join("chapter.md"), b"b");
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].inputs.len(), 2);
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("КОНФЛИКТ выходных имён")));

        let cancel = AtomicBool::new(false);
        let mut called = 0usize;
        let report = run_batch(&plan, "wav", false, &cancel, &|_| {}, |file, o, c, p| {
            called += 1;
            mock_ok(file, o, c, p)
        });
        assert_eq!(called, 0, "конфликтующие файлы не синтезируются");
        assert_eq!(report.skipped(), 2);
        assert_eq!(report.status(), BatchStatus::NoWork);
        assert_eq!(report.conflicts.len(), 1);
        assert!(report.conflicts[0].output.ends_with("chapter.wav"));
        assert!(report.files.iter().all(|f| f
            .reason
            .as_deref()
            .unwrap()
            .contains("конфликт выходных имён")));
    }

    #[test]
    fn conflict_is_case_insensitive_on_windows() {
        let d = tmpdir("conflict_case");
        let src = d.join("src");
        write(&src.join("Chapter.txt"), b"a");
        write(&src.join("chapter.md"), b"b");
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        assert_eq!(plan.conflicts.len(), 1, "регистр не создаёт разные имена");
        assert_eq!(plan.conflicts[0].inputs.len(), 2);
        let report = run_batch(
            &plan,
            "wav",
            false,
            &AtomicBool::new(false),
            &|_| {},
            |f, o, c, p| mock_ok(f, o, c, p),
        );
        assert_eq!(report.skipped(), 2);
        assert_eq!(report.completed(), 0);
    }

    #[test]
    fn conflict_with_existing_output_does_not_overwrite() {
        let d = tmpdir("conflict_existing");
        let src = d.join("src");
        let out = d.join("out");
        write(&src.join("a.txt"), b"one");
        write(&src.join("a.md"), b"two");
        write(&out.join("a.wav"), b"keep");
        let plan = plan_batch(&src, &out).unwrap();
        let report = run_batch(
            &plan,
            "wav",
            false,
            &AtomicBool::new(false),
            &|_| {},
            |f, o, c, p| mock_ok(f, o, c, p),
        );
        assert_eq!(report.skipped(), 2);
        assert_eq!(report.completed(), 0);
        assert_eq!(std::fs::read(out.join("a.wav")).unwrap(), b"keep");
        assert!(report
            .files
            .iter()
            .all(|f| f.reason.as_deref().unwrap().contains("конфликт")));
    }

    #[test]
    fn empty_and_all_skipped_are_no_work() {
        let empty = BatchReport::empty();
        assert_eq!(empty.status(), BatchStatus::NoWork);
        assert!(empty.summary().contains("НЕЧЕГО ДЕЛАТЬ"));

        let d = tmpdir("all_skipped");
        let src = d.join("src");
        let out = d.join("out");
        write(&src.join("a.txt"), b"a");
        write(&out.join("a.wav"), b"already");
        let plan = plan_batch(&src, &out).unwrap();
        let report = run_batch(
            &plan,
            "wav",
            false,
            &AtomicBool::new(false),
            &|_| {},
            |f, o, c, p| mock_ok(f, o, c, p),
        );
        assert_eq!(report.skipped(), 1);
        assert_eq!(report.status(), BatchStatus::NoWork);
        assert!(report.summary().contains("НЕЧЕГО ДЕЛАТЬ"));
    }

    // ===== G3.1: физическая безопасность путей =====

    #[cfg(windows)]
    fn make_junction(target: &Path, link: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[cfg(windows)]
    #[test]
    fn output_path_rejects_junction_component() {
        let d = tmpdir("junction_out");
        let real = d.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = d.join("link");
        if !make_junction(&real, &link) {
            eprintln!("SKIP output_path_rejects_junction_component: нет прав на junction");
            return;
        }
        let err = output_path_for(&link.join("out"), Path::new("a.txt"), "wav").unwrap_err();
        assert!(format!("{err:#}").contains("перенаправления"), "{err:#}");
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let err = plan_batch(&src, &link).unwrap_err();
        assert!(format!("{err:#}").contains("перенаправления"), "{err:#}");
        assert!(
            !link.join("out").exists(),
            "по junction ничего не создаётся"
        );
    }

    #[cfg(windows)]
    #[test]
    fn source_junction_is_rejected() {
        let d = tmpdir("junction_src");
        let real = d.join("real");
        let src = real.join("sub");
        std::fs::create_dir_all(&src).unwrap();
        write(&src.join("a.txt"), b"a");
        let link = d.join("alias");
        if !make_junction(&real, &link) {
            eprintln!("SKIP source_junction_is_rejected: нет прав на junction");
            return;
        }
        let err = plan_batch(&link, &d.join("out")).unwrap_err();
        assert!(format!("{err:#}").contains("перенаправления"), "{err:#}");
    }

    #[cfg(windows)]
    #[test]
    fn physical_overlap_via_junction_is_detected() {
        let d = tmpdir("junction_overlap");
        let real = d.join("real");
        let src = real.join("sub");
        write(&src.join("a.txt"), b"a");
        write(&src.join("out").join("old.txt"), b"old");
        let alias = d.join("alias");
        if !make_junction(&real, &alias) {
            eprintln!("SKIP physical_overlap_via_junction_is_detected: нет прав на junction");
            return;
        }
        // источник — через alias, выход — через реальный путь; лексически
        // пути не связаны, физически out находится внутри source
        let plan = plan_batch(&alias.join("sub"), &src.join("out")).unwrap();
        let rels: Vec<String> = plan
            .files
            .iter()
            .map(|f| f.relative.to_string_lossy().to_string())
            .collect();
        assert_eq!(rels, vec!["a.txt"], "{rels:?}");
    }

    #[cfg(windows)]
    #[test]
    fn physical_equality_via_junction_is_error() {
        let d = tmpdir("junction_equal");
        let real = d.join("real");
        let src = real.join("sub");
        write(&src.join("a.txt"), b"a");
        let alias = d.join("alias");
        if !make_junction(&real, &alias) {
            eprintln!("SKIP physical_equality_via_junction_is_error: нет прав на junction");
            return;
        }
        let err = plan_batch(&alias.join("sub"), &src).unwrap_err();
        assert!(
            format!("{err:#}").contains("физически совпадает"),
            "{err:#}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn existing_output_symlink_is_rejected() {
        use std::os::windows::fs::symlink_file;
        let d = tmpdir("out_symlink");
        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let target = d.join("target.wav");
        std::fs::write(&target, b"target").unwrap();
        let link = out.join("a.wav");
        if symlink_file(&target, &link).is_err() {
            eprintln!("SKIP existing_output_symlink_is_rejected: нет прав на symlink");
            return;
        }
        let err = output_path_for(&out, Path::new("a.txt"), "wav").unwrap_err();
        assert!(format!("{err:#}").contains("перенаправления"), "{err:#}");
        assert_eq!(std::fs::read(&target).unwrap(), b"target");
    }

    // ===== G6.1: параллельный раннер на постоянном пуле (мок-воркеры) =====

    use std::sync::atomic::AtomicBool as AB;
    use std::sync::{Arc, Mutex};

    /// Мок постоянного воркера: u8 «Pocket», переживает все файлы.
    fn pool_slots(n: usize) -> Vec<app_core::persistent::WorkerSlot<u8>> {
        (0..n as u8)
            .map(|i| Arc::new(Mutex::new(Some(i))))
            .collect()
    }

    /// Параллельный прогон 4 файлов на 2 воркерах: все файлы готовы, отчёт в
    /// ИСХОДНОМ порядке плана, K/N событий корректны, оба слота пережили пакет.
    #[test]
    fn parallel_run_completes_in_plan_order() {
        let d = tmpdir("par_ok");
        let src = d.join("src");
        write(&src.join("a.txt"), b"a");
        write(&src.join("b.txt"), b"b");
        write(&src.join("c.txt"), b"c");
        write(&src.join("d.txt"), b"d");
        let out = d.join("out");
        let plan = plan_batch(&src, &out).unwrap();
        let slots = pool_slots(2);
        let slot_copies: Vec<_> = slots.iter().map(Arc::clone).collect();
        let cancel = Arc::new(AB::new(false));
        let emit: Arc<dyn Fn(BatchEvent) + Send + Sync> = Arc::new(|_| {});
        let started = Arc::new(Mutex::new(Vec::new()));
        let started_c = Arc::clone(&started);
        let report = run_prepared_parallel(
            &plan,
            preflight_batch(&plan, "wav", false, &cancel, &|_| {}),
            &cancel,
            slots,
            &emit,
            move |pf, _wid, _slot| {
                started_c.lock().unwrap().push(pf.index);
                if let Some(p) = pf.out.parent() {
                    std::fs::create_dir_all(p).unwrap();
                }
                std::fs::write(&pf.out, b"mock").unwrap();
                let mut r = FileReport::new(
                    &BatchFile {
                        input: pf.input.clone(),
                        relative: pf.relative.clone(),
                    },
                    Some(pf.out.clone()),
                );
                r.status = FileStatus::Completed;
                r
            },
        );
        assert_eq!(
            report.status(),
            BatchStatus::Completed,
            "{}",
            report.summary()
        );
        assert_eq!(report.completed(), 4);
        // отчёт в исходном порядке плана
        let rels: Vec<String> = report
            .files
            .iter()
            .map(|f| f.relative.to_string_lossy().to_string())
            .collect();
        assert_eq!(rels, vec!["a.txt", "b.txt", "c.txt", "d.txt"]);
        // каждый файл стартовал ровно один раз
        let mut s = started.lock().unwrap().clone();
        s.sort_unstable();
        assert_eq!(s, vec![0, 1, 2, 3]);
        // 4 файла на 2 слотах без пересоздания: воркеры переиспользованы;
        // после пакета Pocket освобождены (слоты пусты — жизненный цикл G6.1)
        assert!(
            slot_copies.iter().all(|sl| sl.lock().unwrap().is_none()),
            "Pocket освобождены после пакета"
        );
        assert_eq!(report.pool_workers, Some(2));
        assert!(
            report.pool_loads.is_none(),
            "pool_loads ставит вызывающий (job.rs)"
        );
    }

    /// Отмена параллельного пакета: незапущенные файлы — Skipped «отменено
    /// до запуска», готовые до отмены — Completed, статус Cancelled.
    #[test]
    fn parallel_run_cancel_marks_remaining_skipped() {
        let d = tmpdir("par_cancel");
        let src = d.join("src");
        for i in 0..4 {
            write(&src.join(format!("{i}.txt")), b"x");
        }
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        let slots = pool_slots(2);
        let cancel = Arc::new(AB::new(false));
        let cancel_synth = Arc::clone(&cancel);
        let emit: Arc<dyn Fn(BatchEvent) + Send + Sync> = Arc::new(|_| {});
        let report = run_prepared_parallel(
            &plan,
            preflight_batch(&plan, "wav", false, &cancel, &|_| {}),
            &cancel,
            slots,
            &emit,
            move |pf, _wid, _slot| {
                // отмена после первого готового файла
                cancel_synth.store(true, Ordering::SeqCst);
                let mut r = FileReport::new(
                    &BatchFile {
                        input: pf.input.clone(),
                        relative: pf.relative.clone(),
                    },
                    Some(pf.out.clone()),
                );
                r.status = FileStatus::Completed;
                r
            },
        );
        assert_eq!(
            report.status(),
            BatchStatus::Cancelled,
            "{}",
            report.summary()
        );
        assert!(report.cancelled);
        assert!(report.completed() >= 1, "готовое до отмены сохранено");
        assert!(report.skipped() >= 1, "незапущенное помечено Skipped");
        assert!(report
            .files
            .iter()
            .filter(|f| f.status == FileStatus::Skipped)
            .all(|f| f.reason.as_deref() == Some("отменено до запуска")));
    }

    /// Паника synth в воркере: файл — Failed с диагнозом, очередь доделывают
    /// живые воркеры, слот паниковавшего отравлен (Pocket не переиспользуется).
    #[test]
    fn parallel_run_panic_fails_file_poisons_slot() {
        let d = tmpdir("par_panic");
        let src = d.join("src");
        for i in 0..3 {
            write(&src.join(format!("{i}.txt")), b"x");
        }
        let plan = plan_batch(&src, &d.join("out")).unwrap();
        let slots = pool_slots(2);
        let slot_copies: Vec<_> = slots.iter().map(Arc::clone).collect();
        let cancel = Arc::new(AB::new(false));
        let logs = Arc::new(Mutex::new(Vec::new()));
        let logs_c = Arc::clone(&logs);
        let emit: Arc<dyn Fn(BatchEvent) + Send + Sync> = Arc::new(move |ev| {
            if let BatchEvent::Log(l) = ev {
                logs_c.lock().unwrap().push(l);
            }
        });
        let report = run_prepared_parallel(
            &plan,
            preflight_batch(&plan, "wav", false, &cancel, &|_| {}),
            &cancel,
            slots,
            &emit,
            move |pf, _wid, _slot| {
                if pf.relative.to_string_lossy().contains('1') {
                    panic!("мок-паника воркера");
                }
                let mut r = FileReport::new(
                    &BatchFile {
                        input: pf.input.clone(),
                        relative: pf.relative.clone(),
                    },
                    Some(pf.out.clone()),
                );
                r.status = FileStatus::Completed;
                r
            },
        );
        // паниковавший файл — Failed с текстом паники; остальные — готово
        let failed: Vec<&FileReport> = report
            .files
            .iter()
            .filter(|f| f.status == FileStatus::Failed)
            .collect();
        assert_eq!(failed.len(), 1, "{}", report.summary());
        assert!(failed[0].reason.as_deref().unwrap().contains("мок-паника"));
        assert_eq!(report.completed(), 2);
        assert_eq!(report.status(), BatchStatus::Partial);
        assert!(
            slot_copies.iter().any(|s| s.lock().unwrap().is_none()),
            "слот паниковавшего отравлен"
        );
        assert!(logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("фатальный сбой")));
    }

    /// Preflight исключает конфликты/существующие результаты из to_synth
    /// ДО запуска воркеров (мок-пул их не получает вовсе).
    #[test]
    fn parallel_preflight_excludes_conflicts_and_existing() {
        let d = tmpdir("par_pre");
        let src = d.join("src");
        write(&src.join("chapter.txt"), b"a");
        write(&src.join("chapter.md"), b"b");
        write(&src.join("c.txt"), b"c");
        let out = d.join("out");
        write(&out.join("c.wav"), b"already");
        let plan = plan_batch(&src, &out).unwrap();
        let cancel = AB::new(false);
        let prepared = preflight_batch(&plan, "wav", false, &cancel, &|_| {});
        assert_eq!(
            prepared.to_synth.len(),
            0,
            "конфликт и существующий исключены"
        );
        assert_eq!(prepared.reports.len(), 3);
        assert_eq!(
            count_actionable(&plan, "wav", false),
            0,
            "count_actionable согласован с preflight"
        );
        // с overwrite существующий снова кандидат
        assert_eq!(count_actionable(&plan, "wav", true), 1);
    }
}
