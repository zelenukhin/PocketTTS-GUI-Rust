//! Уникальный каталог трассировок: ТОЛЬКО СОЗДАНИЕ, БЕЗ УДАЛЕНИЙ.
//!
//! После инцидента 2026-11-08 (подробности — docs/SAFETY_AUDIT.md) модуль
//! `trace_clean` c очисткой каталогов полностью удалён из проекта. Принципы:
//!   1. Никаких `remove_dir_all` / `remove_file` — ни в коде, ни в тестах.
//!   2. Один запуск — один НОВЫЙ каталог; существующие никогда не трогаются
//!      (ни удалением, ни перезаписью содержимого).
//!   3. Базовый путь не может совпадать с защищённым (cwd, домашний каталог,
//!      корень проекта, корень диска); сравнение — с канонизацией ОБЕИХ сторон
//!      (урок инцидента: `\\?\`-префикс canonicalize против обычного пути).
//!   4. Родительские каталоги не создаются автоматически: `create_dir` только
//!      лист — запись не «проваливается» в произвольное место дерева.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// Мягкая канонизация: существующий путь — canonicalize без `\\?\`-префикса;
/// несуществующий — лексически (без обращения к диску).
fn soft_canon(p: &Path) -> PathBuf {
    match p.canonicalize() {
        Ok(c) => strip_verbatim(c),
        Err(_) => lexical_abs(p),
    }
}

/// canonicalize() на Windows даёт `\\?\C:\...`; для сравнений снимаем префикс.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().to_string();
    let stripped = s
        .strip_prefix(r"\\?\UNC\")
        .map(|x| format!(r"\\{x}"))
        .or_else(|| s.strip_prefix(r"\\?\").map(str::to_string));
    stripped.map(PathBuf::from).unwrap_or(p)
}

fn lexical_abs(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Защищённые пути: текущий каталог, домашний, корень проекта (известен на
/// этапе сборки), корень диска. Только чтение метаданных — никаких записей.
fn protected_paths() -> Vec<(&'static str, PathBuf)> {
    let mut v = vec![(
        "текущий каталог",
        soft_canon(&std::env::current_dir().unwrap_or_default()),
    )];
    if let Some(home) = std::env::var_os("USERPROFILE") {
        v.push(("домашний каталог", soft_canon(Path::new(&home))));
    }
    if let Some(ws) = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2) {
        v.push(("корень проекта", soft_canon(ws)));
    }
    v
}

/// Является ли путь защищённым (чистая проверка, без записи на диск).
pub fn is_protected(p: &Path) -> bool {
    let c = soft_canon(p);
    if c.parent().is_none() {
        return true; // корень диска
    }
    protected_paths().iter().any(|(_, pp)| c == *pp)
}

/// Создать НОВЫЙ уникальный каталог трассировок от базового пути [base].
///
/// - `base` не существует → создаётся ровно он;
/// - существует → пробуем `base_<время>`, `base_<время>_2` … `_999`
///   в том же родителе (существующее никогда не трогаем);
/// - `base` (или совпадающий канонически путь) защищён → ошибка;
/// - родитель `base` не существует → ошибка (родителей не создаём).
pub fn new_trace_dir(base: &Path) -> Result<PathBuf> {
    if is_protected(base) {
        bail!(
            "каталог трасс {:?} совпадает с защищённым путём (cwd/домашний/корень проекта/корень диска) — отказ",
            base.display()
        );
    }
    if !base.exists() {
        std::fs::create_dir(base)?; // только лист; родитель обязан существовать
        return Ok(base.to_path_buf());
    }
    // base занят: уникальное имя рядом; ничего не удаляем и не перезаписываем
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (parent, name) = (
        base.parent().unwrap_or(Path::new(".")),
        base.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
    let mut candidate = parent.join(format!("{name}_{stamp}"));
    for attempt in 0..1000u32 {
        if !candidate.exists() {
            std::fs::create_dir(&candidate)?;
            return Ok(candidate);
        }
        candidate = parent.join(format!("{name}_{stamp}_{}", attempt + 2));
    }
    bail!(
        "не удалось подобрать уникальное имя трасс рядом с {}",
        base.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // Все тесты БЕЗ удалений. Песочницы создаются уникальными
    // (pid + наносекунды) и ОСТАЮТСЯ в %TEMP% после прогона — сознательный
    // выбор: несколько пустых каталогов в temp безопаснее любого удаления.

    fn sandbox(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "shtorm_tracedir_{}_{}_{}",
            std::process::id(),
            tag,
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn protected_paths_detected_by_both_forms() {
        // обычная и verbatim (\\?\) форма одного пути — кейс инцидента
        let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        assert!(is_protected(ws));
        if let Ok(canon) = ws.canonicalize() {
            assert!(is_protected(&canon), "verbatim-форма корня проекта");
        }
        assert!(is_protected(&std::env::current_dir().unwrap()));
        assert!(is_protected(Path::new("C:\\")));
        if let Some(home) = std::env::var_os("USERPROFILE") {
            assert!(is_protected(Path::new(&home)));
        }
    }

    #[test]
    fn fresh_base_created_exactly() {
        let sb = sandbox("fresh");
        let base = sb.join("ep_traces");
        let got = new_trace_dir(&base).unwrap();
        assert_eq!(got, base);
        assert!(base.is_dir());
    }

    #[test]
    fn existing_base_never_touched_siblings_created() {
        let sb = sandbox("existing");
        let base = sb.join("ep_traces");
        std::fs::create_dir(&base).unwrap();
        std::fs::write(base.join("old_trace.json"), b"keep me").unwrap();
        let got = new_trace_dir(&base).unwrap();
        assert_ne!(got, base, "должно быть новое имя рядом");
        assert!(got.is_dir());
        // существующий каталог и его содержимое нетронуты
        assert_eq!(
            std::fs::read(base.join("old_trace.json")).unwrap(),
            b"keep me"
        );
        // повторный запуск от того же base даёт ещё один новый каталог
        let got2 = new_trace_dir(&base).unwrap();
        assert_ne!(got2, got);
        assert_ne!(got2, base);
    }

    #[test]
    fn protected_base_refused_without_any_creation() {
        let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let count = |p: &Path| p.read_dir().map(|d| d.count()).unwrap_or(usize::MAX);
        let before = count(ws);
        assert!(new_trace_dir(ws).is_err());
        let after = count(ws);
        assert_eq!(before, after, "в защищённом каталоге ничего не создано");
    }

    #[test]
    fn missing_parent_is_error_no_side_creation() {
        let sb = sandbox("no_parent");
        let base = sb.join("no_such_parent").join("ep_traces");
        assert!(new_trace_dir(&base).is_err());
        assert!(!sb.join("no_such_parent").exists(), "родитель не создан");
    }
}
