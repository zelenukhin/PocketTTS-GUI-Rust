//! Безопасная очистка каталога диагностических трассировок.
//!
//! Урок инцидента 2026-11-08: сравнение canonicalize()-пути (с префиксом
//! `\\?\`) с обычным путём всегда ложно — гвард пропустил корень проекта, и
//! очистка удалила исходники. Правила теперь:
//! 1. Обе стороны каждого сравнения канонизируются (или обе — нет).
//! 2. Отказ, если цель: корень диска, cwd, домашний каталог, корень workspace
//!    или ЛЮБОЙ их предок (т.е. цель является одним из защищённых каталогов
//!    ИЛИ лежит выше них по дереву).
//! 3. Отказ, если в каталоге есть хоть один посторонний файл: разрешены
//!    только трассы известных сессий (`<имя>.json*`).
//!
//! Юнит-тесты покрывают катастрофические сценарии напрямую (без удаления).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Имена сессий движка, чьи трассы ожидаем в каталоге.
pub const KNOWN_TRACE_STEMS: [&str; 5] = [
    "mimi_encoder",
    "text_conditioner",
    "flow_lm_main",
    "flow_lm_flow",
    "mimi_decoder",
];

/// Канонизация «мягкая»: без обращения к диску (несуществующий путь — как есть,
/// но с нормализацией разделителей и без \\?\-префикса у существующих).
fn soft_canon(p: &Path) -> PathBuf {
    match p.canonicalize() {
        Ok(c) => dunce_like(c),
        Err(_) => normalize_separators(p),
    }
}

/// canonicalize() на Windows даёт `\\?\C:\...`; для сравнений снимаем префикс.
fn dunce_like(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().to_string();
    let stripped = s
        .strip_prefix(r"\\?\UNC\")
        .map(|x| format!(r"\\{x}"))
        .or_else(|| s.strip_prefix(r"\\?\").map(|x| x.to_string()));
    match stripped {
        Some(x) => PathBuf::from(x),
        None => p,
    }
}

fn normalize_separators(p: &Path) -> PathBuf {
    // несуществующий путь: приводим к абсолютному от cwd без обращения к диску
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    // убираем . и .. лексически
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

/// Главная проверка: можно ли чистить [dir] как каталог трасс?
/// Err — подробная причина; Ok(()) — можно (или каталога нет).
pub fn ensure_cleanable(dir: &Path) -> Result<()> {
    let canon = soft_canon(dir);

    // --- защищённые пути: канонизированные ---
    let protected: [(&str, PathBuf); 3] = [
        (
            "текущий каталог",
            soft_canon(&std::env::current_dir().unwrap_or_default()),
        ),
        (
            "домашний каталог",
            std::env::var_os("USERPROFILE")
                .map(|v| soft_canon(Path::new(&v)))
                .unwrap_or_default(),
        ),
        // корень workspace: известен бинарю/либе на этапе сборки
        (
            "корень проекта",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .map(soft_canon_canon)
                .unwrap_or_default(),
        ),
    ];
    for (why, p) in &protected {
        if p.as_os_str().is_empty() {
            continue;
        }
        if canon == *p {
            bail!(
                "отказ: каталог трасс совпадает с защищённым ({why}): {}",
                canon.display()
            );
        }
        // цель — ПРЕДОК защищённого (лежит выше него): тоже опасно
        if p.starts_with(&canon) && canon != *p {
            bail!(
                "отказ: каталог трасс ({}) содержит защищённый каталог ({why}: {})",
                canon.display(),
                p.display()
            );
        }
    }
    if canon.parent().is_none() {
        bail!("отказ: корень диска");
    }

    // --- содержимое: только наши трассы ---
    let entries = match std::fs::read_dir(&canon) {
        Ok(e) => e,
        Err(_) => return Ok(()), // каталога нет — чистить нечего
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let stem_ok = KNOWN_TRACE_STEMS.iter().any(|k| name.starts_with(k));
        let ext_ok = name.ends_with(".json") || name.contains(".json_2");
        if !(stem_ok && ext_ok) {
            bail!(
                "отказ: посторонний файл «{name}» в каталоге трасс {}",
                canon.display()
            );
        }
    }
    Ok(())
}

fn soft_canon_canon(p: &Path) -> PathBuf {
    soft_canon(p)
}

/// Очистка с гарантиями [ensure_cleanable].
pub fn clean_trace_dir(dir: &Path) -> Result<()> {
    ensure_cleanable(dir).with_context(|| format!("чистка {}", dir.display()))?;
    let canon = soft_canon(dir);
    if canon.exists() {
        std::fs::remove_dir_all(&canon).with_context(|| format!("remove {}", canon.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Каталог с трассой-приманкой; функция должна разрешить удаление,
    // но реально НЕ удаляем — тестируем только ensure_cleanable.

    #[test]
    fn refuses_project_root_even_with_verbatim_prefix() {
        let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        // прямой путь
        assert!(
            ensure_cleanable(ws).is_err(),
            "корень проекта должен быть запрещён"
        );
        // canonicalize-путь с \\?\ — кейс инцидента
        if let Ok(canon) = ws.canonicalize() {
            assert!(
                ensure_cleanable(&canon).is_err(),
                "verbatim-путь корня проекта должен быть запрещён (инцидент 2026-11-08)"
            );
        }
    }

    #[test]
    fn refuses_cwd_home_and_drive_root() {
        let cwd = std::env::current_dir().unwrap();
        assert!(ensure_cleanable(&cwd).is_err());
        if let Some(home) = std::env::var_os("USERPROFILE") {
            assert!(ensure_cleanable(Path::new(&home)).is_err());
        }
        assert!(ensure_cleanable(Path::new("C:\\")).is_err());
        // предок cwd (например, каталог пользователя) содержит защищённое — отказ
        if let Some(parent) = cwd.parent() {
            assert!(
                ensure_cleanable(parent).is_err(),
                "предок защищённого каталога"
            );
        }
    }

    #[test]
    fn refuses_foreign_files_allows_own_traces() {
        let dir = std::env::temp_dir().join("shtorm_trace_clean_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("flow_lm_main.json_2026-11-08_00-00-00.json"),
            b"{}",
        )
        .unwrap();
        assert!(ensure_cleanable(&dir).is_ok(), "свои трассы разрешены");
        std::fs::write(dir.join("Cargo.toml"), b"").unwrap();
        assert!(ensure_cleanable(&dir).is_err(), "посторонний файл запрещён");
        std::fs::remove_file(dir.join("Cargo.toml")).unwrap();
        std::fs::write(dir.join("subdir.json"), b"").unwrap();
        assert!(
            ensure_cleanable(&dir).is_err(),
            "имя без известного стема запрещено"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_ok() {
        assert!(ensure_cleanable(Path::new("Z:/definitely/not/here/ep_traces")).is_ok());
    }
}
