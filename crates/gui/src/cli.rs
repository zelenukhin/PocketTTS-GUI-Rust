//! G6.1.1: диспетчеризация командной строки (дефект №2).
//!
//! Прежнее поведение: `main` искал известные `--portable-*` флаги через
//! `args.iter().any(..)`, а при НЕсовпадении молча открывал GUI — опечатка
//! `--portable-batch-selftest` вместо `--portable-selftest-batch` запускала
//! окно вместо диагностики (реальный инцидент приёмки G6.1).
//!
//! Теперь аргументы разбираются строго:
//! - без аргументов — обычный GUI (как раньше);
//! - ровно одна диагностическая команда — соответствующий headless-режим;
//! - `--help`/`-h` — справка без открытия окна;
//! - опции (`--device`, `--format`, `--hold-ms`) допустимы ТОЛЬКО вместе с
//!   диагностической командой и требуют корректного значения;
//! - любой неизвестный/лишний/повторный аргумент, отсутствующее значение
//!   параметра или несовместимые команды — диагностическая ошибка: сообщение
//!   в stderr, подсказка `--help`, код выхода 2, окно НЕ открывается.
//!
//! Пути со пробелами/Unicode в аргументах не участвуют (диагностические
//! опции принимают только закрытые множества значений), поэтому специальных
//! правил экранирования не требуется; `std::env::args` отдаёт Unicode как есть.

/// Диагностическая команда или обычный GUI (решение по аргументам).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliCommand {
    /// Без аргументов — интерактивное окно.
    Gui,
    /// `--help`/`-h`: справка, код 0, окно не открывается.
    Help,
    PortableCheck,
    PortableSelftest,
    PortableSelftestBatch,
    PortableSelftestCancel,
}

impl CliCommand {
    /// Имя команды для сообщений об ошибках (None — Gui/Help).
    pub fn flag_name(&self) -> Option<&'static str> {
        match self {
            CliCommand::Gui | CliCommand::Help => None,
            CliCommand::PortableCheck => Some("--portable-check"),
            CliCommand::PortableSelftest => Some("--portable-selftest"),
            CliCommand::PortableSelftestBatch => Some("--portable-selftest-batch"),
            CliCommand::PortableSelftestCancel => Some("--portable-selftest-cancel"),
        }
    }
}

/// Справка по запуску (используется `--help` и сообщениями об ошибках).
pub fn usage_text() -> String {
    "ShtormTTS GUI — запуск:\n\
     \x20 без аргументов              обычное окно GUI\n\
     \x20 --portable-check            headless-проверка ресурсов (модель/ORT/ffmpeg)\n\
     \x20 --portable-selftest         headless-синтез одного файла\n\
     \x20 --portable-selftest-batch   headless-пакет из двух файлов\n\
     \x20 --portable-selftest-cancel  headless-проверка отмены и повторного запуска\n\
     \x20 --help | -h                 эта справка (окно не открывается)\n\
     Опции диагностических режимов (только вместе с командой выше):\n\
     \x20 --device cpu|cuda           устройство синтеза\n\
     \x20 --format wav|mp3|opus       формат вывода\n\
     \x20 --hold-ms <мс>              задержка перед выходом (неотрицательное целое)\n"
        .into()
}

/// Разобрать аргументы (без имени программы). Любое нарушение — `Err` с
/// человекочитаемой причиной; вызывающий обязан НЕ открывать GUI при `Err`.
/// Опции допускаются до или после диагностической команды (существующие
/// скрипты используют оба порядка), но только вместе с ней.
pub fn parse_cli_args(args: &[String]) -> Result<CliCommand, String> {
    let mut command: Option<CliCommand> = None;
    let mut saw_option = false;
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();
        let cmd = match arg {
            "--help" | "-h" | "/?" => Some(CliCommand::Help),
            "--portable-check" => Some(CliCommand::PortableCheck),
            "--portable-selftest" => Some(CliCommand::PortableSelftest),
            "--portable-selftest-batch" => Some(CliCommand::PortableSelftestBatch),
            "--portable-selftest-cancel" => Some(CliCommand::PortableSelftestCancel),
            "--device" | "--format" | "--hold-ms" => {
                // значение обязательно; значение-опция — ошибка (не принимать
                // ошибочное значение за самостоятельную опцию)
                saw_option = true;
                let value = args.get(i + 1).ok_or_else(|| {
                    format!("отсутствует значение параметра {arg} (после него ничего нет)")
                })?;
                if value.starts_with('-') && value.len() > 1 {
                    return Err(format!(
                        "отсутствует значение параметра {arg}: «{value}» — следующая опция, \
                         а не значение"
                    ));
                }
                validate_option_value(arg, value)?;
                i += 2;
                continue;
            }
            other => {
                return Err(format!(
                    "неизвестный аргумент: {other} — используйте --help для списка режимов"
                ));
            }
        };
        if let Some(cmd) = cmd {
            if let Some(prev) = command {
                let (a, b) = (
                    prev.flag_name().unwrap_or("?"),
                    cmd.flag_name().unwrap_or("?"),
                );
                return Err(format!(
                    "диагностические команды несовместимы: {a} и {b} — укажите ровно одну"
                ));
            }
            command = Some(cmd);
        }
        i += 1;
    }
    if saw_option
        && !matches!(
            command,
            Some(
                CliCommand::PortableCheck
                    | CliCommand::PortableSelftest
                    | CliCommand::PortableSelftestBatch
                    | CliCommand::PortableSelftestCancel
            )
        )
    {
        return Err(
            "опции --device/--format/--hold-ms применимы только вместе с диагностической \
             командой (--portable-check / --portable-selftest[-batch|-cancel])"
                .into(),
        );
    }
    Ok(command.unwrap_or(CliCommand::Gui))
}

/// Значения опций — закрытые множества; опечатка в значении диагностируется,
/// а не принимается молча (прежний код любую строку --device считал cpu).
fn validate_option_value(key: &str, value: &str) -> Result<(), String> {
    match key {
        "--device" => {
            if value.eq_ignore_ascii_case("cpu") || value.eq_ignore_ascii_case("cuda") {
                Ok(())
            } else {
                Err(format!(
                    "недопустимое значение {key}: «{value}» (ожидается cpu или cuda)"
                ))
            }
        }
        "--format" => match value.to_ascii_lowercase().as_str() {
            "wav" | "mp3" | "opus" => Ok(()),
            _ => Err(format!(
                "недопустимое значение {key}: «{value}» (ожидается wav, mp3 или opus)"
            )),
        },
        "--hold-ms" => value.parse::<u64>().map(|_| ()).map_err(|_| {
            format!("недопустимое значение {key}: «{value}» (ожидается целое число мс)")
        }),
        _ => Err(format!(
            "внутренняя ошибка валидации: неизвестная опция {key}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Таблица диспетчеризации из задания G6.1.1 §6.
    #[test]
    fn dispatch_table() {
        assert_eq!(parse_cli_args(&args(&[])), Ok(CliCommand::Gui));
        assert_eq!(
            parse_cli_args(&args(&["--portable-check"])),
            Ok(CliCommand::PortableCheck)
        );
        assert_eq!(
            parse_cli_args(&args(&["--portable-selftest"])),
            Ok(CliCommand::PortableSelftest)
        );
        assert_eq!(
            parse_cli_args(&args(&["--portable-selftest-batch"])),
            Ok(CliCommand::PortableSelftestBatch)
        );
        assert_eq!(
            parse_cli_args(&args(&["--portable-selftest-cancel"])),
            Ok(CliCommand::PortableSelftestCancel)
        );
        // инцидент G6.1: перепутанное имя НЕ открывает GUI
        assert!(parse_cli_args(&args(&["--portable-batch-selftest"])).is_err());
        assert!(parse_cli_args(&args(&["--unknown-option"])).is_err());
        // неизвестный флаг рядом с корректной командой — ошибка
        assert!(parse_cli_args(&args(&["--portable-check", "--unknown-option"])).is_err());
        // отсутствующее значение параметра — ошибка
        assert!(parse_cli_args(&args(&["--portable-check", "--device"])).is_err());
        // две команды — ошибка
        assert!(parse_cli_args(&args(&["--portable-check", "--portable-selftest"])).is_err());
        // справка без окна
        assert_eq!(parse_cli_args(&args(&["--help"])), Ok(CliCommand::Help));
        assert_eq!(parse_cli_args(&args(&["-h"])), Ok(CliCommand::Help));
    }

    /// Опции: корректные значения принимаются в любом регистре; порядок
    /// «опции до команды» тоже допустим (существующие скрипты так делают).
    #[test]
    fn options_valid_in_any_order() {
        for ok in [
            vec!["--portable-check", "--device", "cuda"],
            vec!["--device", "cuda", "--portable-check"],
            vec!["--portable-selftest", "--format", "MP3"],
            vec!["--portable-selftest-batch", "--hold-ms", "500"],
            vec![
                "--portable-check",
                "--device",
                "CPU",
                "--format",
                "opus",
                "--hold-ms",
                "0",
            ],
        ] {
            assert!(
                parse_cli_args(&args(&ok)).is_ok(),
                "должно проходить: {ok:?}"
            );
        }
    }

    /// Некорректные значения опций и структурные ошибки — диагностика,
    /// GUI не открывается (Err).
    #[test]
    fn options_invalid_are_errors() {
        for bad in [
            vec!["--portable-check", "--device", "gpu"], // опечатка устройства
            vec!["--portable-check", "--format", "flac"],
            vec!["--portable-check", "--hold-ms", "-5"],
            vec!["--portable-check", "--hold-ms", "abc"],
            vec!["--portable-check", "--device"], // значение отсутствует
            vec!["--portable-check", "--device", "--format"], // значение = опция
            vec!["--device", "cuda"],             // опция без команды
            vec!["--portable-check", "--portable-check"], // команда дважды
            vec!["--help", "--portable-check"],   // help + команда
            vec!["--portable-check", "extra-positional"], // посторонний аргумент
        ] {
            assert!(
                parse_cli_args(&args(&bad)).is_err(),
                "должно быть ошибкой: {bad:?}"
            );
        }
    }

    /// Сообщения об ошибках понятны и ведут к --help: упоминают сам аргумент.
    #[test]
    fn error_messages_are_diagnostic() {
        let e = parse_cli_args(&args(&["--portable-batch-selftest"])).unwrap_err();
        assert!(e.contains("--portable-batch-selftest"), "{e}");
        assert!(e.contains("--help"), "{e}");
        let e = parse_cli_args(&args(&["--portable-check", "--device"])).unwrap_err();
        assert!(e.contains("--device"), "{e}");
        assert!(e.contains("значение"), "{e}");
        let e =
            parse_cli_args(&args(&["--portable-check", "--portable-selftest-batch"])).unwrap_err();
        assert!(e.contains("несовместимы"), "{e}");
        let e = parse_cli_args(&args(&["--portable-check", "--device", "gpu"])).unwrap_err();
        assert!(e.contains("gpu") && e.contains("cpu"), "{e}");
        assert!(!usage_text().is_empty());
    }
}
