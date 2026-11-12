//! Предобработка неизвестных модели символов — 1:1 порт `apply_char_rules`
//! из synthesis_worker.py ВЕТКИ comma-semantics (проект не менялся; источник
//! golden — tools/gen_char_rules_golden.py через `git show`). Отличие ветки
//! от main: хвостовая запятая чанка СОХРАНЯЕТСЯ (правило
//! `(?<=\S),(?=\s*$)` удалено) — модель сама отыгрывает паузу после неё.
//!
//! Фазы (после to_model_stress_notation, до движка):
//! 1. char_map — простые замены (порядок = порядок JSON); прямая кавычка " —
//!    направление по соседу: после пустоты/пробела «, иначе ».
//! 2. regex_rules — регэксп-замены (невалидный паттерн молча пропускается,
//!    как Python `except re.error: pass`).
//! 3. dash_context — неизвестные тире: внутри слова -> '-', отдельно -> '—'.
//! 4. bracket_comma — скобки -> запятые с умной пунктуацией:
//!    «текст, (текст)!» -> «текст, текст!»; закрывающая перед .!?…;:—,/концом
//!    строки — без запятой.
//! 5. delete_chars — удаление с нормализацией пробелов:
//!    «текст % текст» -> «текст текст», «50% скидка» -> «50 скидка».
//! 6. Нормализация пробелов/пунктуации (хвостовую запятую НЕ трогаем).

use fancy_regex::Regex;
use serde::Deserialize;
use std::sync::OnceLock;

/// Правила из runtime.json модели (`char_rules`). Все секции опциональны.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CharRules {
    /// Простые замены символ->строка. Порядок сохраняется как в JSON
    /// (serde_json preserve_order), как порядок dict в Python.
    #[serde(default)]
    pub char_map: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub regex_rules: Vec<RegexRule>,
    #[serde(default)]
    pub dash_context: String,
    #[serde(default)]
    pub bracket_comma: String,
    #[serde(default)]
    pub delete_chars: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegexRule {
    pub pattern: String,
    #[serde(default)]
    pub replacement: String,
}

/// Пары скобок для bracket_comma (Python `_BRACKET_PAIRS`).
const BRACKET_PAIRS: &[(char, char)] =
    &[('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('⟨', '⟩')];

/// Знаки, после закрывающей скобки перед которыми запятая не ставится
/// (Python `_STRONG_AFTER`).
const STRONG_AFTER: &str = ".!?…;:—,";

/// Нормализационные регэкспы фазы 6 (порядок как в Python).
fn norm_rules() -> &'static [(Regex, &'static str)] {
    static R: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    R.get_or_init(|| {
        [
            (r" {2,}", " "),
            (r"\s+([.!?…;:,»])", "$1"),
            (r"([,;:])(?=[^\s.!?…;:,])", "$1 "),
            (r",\s*([.!?…])", "$1"),
            // (comma-semantics: срезка хвостовой запятой УДАЛЕНА — сохраняем)
            (r"(?m)^,\s*", ""),
            (r"(?m)[ \t]+$", ""),
        ]
        .iter()
        .map(|(p, r)| (Regex::new(p).expect("norm pattern"), *r))
        .collect()
    })
}

/// Python re.sub replacement `\1` -> fancy-regex `$1` (только backrefs;
/// паттерны runtime.json других escape в replacement не используют).
fn py_repl_to_rust(repl: &str) -> String {
    let mut out = String::with_capacity(repl.len() + 2);
    let mut chars = repl.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek().is_some_and(|n| n.is_ascii_digit()) {
            out.push('$');
            let mut num = String::new();
            while chars.peek().is_some_and(|n| n.is_ascii_digit()) && num.len() < 2 {
                num.push(chars.next().unwrap());
            }
            out.push_str(&num);
        } else if c == '$' {
            out.push_str("$$"); // literal $ в fancy-regex replacement
        } else {
            out.push(c);
        }
    }
    out
}

fn is_alnum_py(c: char) -> bool {
    // Python str.isalnum(): буквы и числа Unicode (категории L*, N*)
    c.is_alphanumeric()
}

/// Порт `apply_char_rules`. `rules` обычно распарсен из секции `char_rules`
/// runtime.json модели (пустые секции == отсутствие правил).
pub fn apply_char_rules(text: &str, rules: &CharRules) -> String {
    // --- Фаза 1: char_map (кроме ") + направление прямой кавычки ---
    let mut text = text.to_string();
    for (src, dst) in rules.char_map.iter() {
        let Some(dst_str) = dst.as_str() else {
            continue;
        };
        if src == "\"" || src.chars().count() != 1 {
            continue; // Python: src != '"' и isinstance(src/dst, str)
        }
        text = text.replace(src, dst_str);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        if ch == '"' {
            let prev = out.chars().last();
            out.push(match prev {
                None => '«',
                Some(' ') => '«',
                Some(_) => '»',
            });
        } else {
            out.push(ch);
        }
    }
    text = out;

    // --- Фаза 2: regex_rules (невалидный паттерн — молча пропуск) ---
    for rule in &rules.regex_rules {
        if let Ok(re) = Regex::new(&rule.pattern) {
            let repl = py_repl_to_rust(&rule.replacement);
            text = re.replace_all(&text, repl.as_str()).into_owned();
        }
    }

    // --- Фаза 3: dash_context ---
    if !rules.dash_context.is_empty() {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let mut out = String::with_capacity(text.len());
        for (i, &ch) in chars.iter().enumerate() {
            if rules.dash_context.contains(ch) {
                let left = if i > 0 { Some(chars[i - 1]) } else { None };
                let right = if i + 1 < n { Some(chars[i + 1]) } else { None };
                let inside = match (left, right) {
                    (Some(l), Some(r)) => l != ' ' && r != ' ',
                    _ => false,
                };
                out.push(if inside { '-' } else { '—' });
            } else {
                out.push(ch);
            }
        }
        text = out;
    }

    // --- Фаза 4: bracket_comma ---
    if !rules.bracket_comma.is_empty() {
        let opens: Vec<char> = rules
            .bracket_comma
            .chars()
            .filter(|c| BRACKET_PAIRS.iter().any(|(o, _)| o == c))
            .collect();
        let closes: Vec<char> = rules
            .bracket_comma
            .chars()
            .filter(|c| BRACKET_PAIRS.iter().any(|(_, cl)| cl == c))
            .collect();
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let mut out = String::with_capacity(text.len() + 8);
        for (i, &ch) in chars.iter().enumerate() {
            if opens.contains(&ch) {
                // назад по ВЫХОДНОМУ буферу, пропуская пробелы
                let mut left = None;
                for c in out.chars().rev() {
                    if c != ' ' {
                        left = Some(c);
                        break;
                    }
                }
                match left {
                    None | Some(',') => {}
                    Some(_) => out.push(','),
                }
            } else if closes.contains(&ch) {
                let mut j = i + 1;
                while j < n && chars[j] == ' ' {
                    j += 1;
                }
                let right = if j < n { Some(chars[j]) } else { None };
                match right {
                    None | Some('\n') => {}
                    Some(r) if STRONG_AFTER.contains(r) => {}
                    Some(_) => out.push(','),
                }
            } else {
                out.push(ch);
            }
        }
        text = out;
    }

    // --- Фаза 5: delete_chars ---
    if !rules.delete_chars.is_empty() {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let mut out = String::with_capacity(text.len());
        let mut i = 0usize;
        while i < n {
            let ch = chars[i];
            if !rules.delete_chars.contains(ch) {
                out.push(ch);
                i += 1;
                continue;
            }
            let left = out.chars().last();
            let right = if i + 1 < n { Some(chars[i + 1]) } else { None };
            match (left, right) {
                (Some(l), Some(r)) if is_alnum_py(l) && is_alnum_py(r) => {
                    out.push(' '); // между словами — разделитель
                    i += 1;
                }
                (None, _) | (Some(' '), _) => {
                    // начало/после пробела — съесть правые пробелы
                    i += 1;
                    while i < n && chars[i] == ' ' {
                        i += 1;
                    }
                }
                _ => {
                    i += 1; // вплотную к знаку/концу — просто удалить
                }
            }
        }
        text = out;
    }

    // --- Фаза 6: нормализация (хвостовую запятую сохраняем — ветка) ---
    for (re, repl) in norm_rules() {
        text = re.replace_all(&text, *repl).into_owned();
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_direction_by_neighbour() {
        let rules = CharRules::default();
        // направление ТОЛЬКО по предыдущему символу (Python): после
        // пробела/начала — «, после непробела — »; следующий символ не важен
        assert_eq!(
            apply_char_rules("\"в начале\", затем \"в середине\"", &rules),
            "«в начале», затем «в середине»"
        );
        assert_eq!(apply_char_rules("сказа\"л", &rules), "сказа»л");
    }

    #[test]
    fn dash_context_inside_vs_alone() {
        let rules = CharRules {
            dash_context: "–−‒‑―".into(),
            ..Default::default()
        };
        assert_eq!(
            apply_char_rules("кто–то и год – два", &rules),
            "кто-то и год — два"
        );
        assert_eq!(apply_char_rules("– начало", &rules), "— начало");
    }

    #[test]
    fn brackets_comma_logic() {
        let rules = CharRules {
            bracket_comma: "()[]{}⟨⟩".into(),
            ..Default::default()
        };
        assert_eq!(
            apply_char_rules("текст (внутри) продолжение", &rules),
            "текст, внутри, продолжение"
        );
        assert_eq!(
            apply_char_rules("текст (внутри)! далее", &rules),
            "текст, внутри! далее"
        );
        assert_eq!(
            apply_char_rules("скобка (в конце)", &rules),
            "скобка, в конце"
        );
    }

    #[test]
    fn delete_chars_spacing() {
        let rules = CharRules {
            delete_chars: "%&@#*/_$€£¥§¶†‡•".into(),
            ..Default::default()
        };
        assert_eq!(
            apply_char_rules("скидка 50% ends", &rules),
            "скидка 50 ends"
        );
        assert_eq!(apply_char_rules("текст % текст", &rules), "текст текст");
        assert_eq!(apply_char_rules("50% скидка", &rules), "50 скидка");
        assert_eq!(apply_char_rules("конец%", &rules), "конец");
        assert_eq!(apply_char_rules("%начало", &rules), "начало");
        assert_eq!(
            apply_char_rules("символ%вплотную", &rules),
            "символ вплотную"
        );
    }

    #[test]
    fn trailing_comma_is_preserved() {
        let rules = CharRules::default();
        // семантика ветки comma-semantics: хвостовую запятую НЕ срезаем
        assert_eq!(apply_char_rules("конец фразы,", &rules), "конец фразы,");
        assert_eq!(apply_char_rules("конец фразы,  ", &rules), "конец фразы,");
    }

    #[test]
    fn normalization() {
        let rules = CharRules::default();
        assert_eq!(apply_char_rules("а , б", &rules), "а, б");
        assert_eq!(apply_char_rules("запятая,перед", &rules), "запятая, перед");
        assert_eq!(apply_char_rules(", строка", &rules), "строка");
        assert_eq!(
            apply_char_rules("много   пробелов", &rules),
            "много пробелов"
        );
        assert_eq!(apply_char_rules("перед . ! ?", &rules), "перед.!?");
        assert_eq!(apply_char_rules("фраза, . дальше", &rules), "фраза. дальше");
    }

    #[test]
    fn py_repl_backrefs() {
        assert_eq!(py_repl_to_rust(r"\1"), "$1");
        assert_eq!(py_repl_to_rust(r"до \1 после"), "до $1 после");
        assert_eq!(py_repl_to_rust("нет"), "нет");
        assert_eq!(py_repl_to_rust("$лит"), "$$лит");
    }

    #[test]
    fn invalid_regex_rule_is_skipped() {
        let rules = CharRules {
            regex_rules: vec![
                RegexRule {
                    pattern: "[broken".into(),
                    replacement: "x".into(),
                },
                RegexRule {
                    pattern: "а{2,}".into(),
                    replacement: "а".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            apply_char_rules("барабан [ok] аа", &rules),
            "барабан [ok] а"
        );
    }

    #[test]
    fn golden_corpus_matches_python() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/golden/char_rules_golden.json"
        );
        let raw = std::fs::read_to_string(path)
            .expect("golden-файл char_rules_golden.json отсутствует: запустите tools/gen_char_rules_golden.py (venv-питоном) и повторите");
        let cases: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("golden json");
        let mut checked = 0usize;
        for case in &cases {
            let name = case["name"].as_str().unwrap_or("?");
            let text = case["text"].as_str().unwrap();
            let expected = case["expected"].as_str().unwrap();
            let rules: CharRules =
                serde_json::from_value(case["rules"].clone()).expect("rules parse");
            let got = apply_char_rules(text, &rules);
            assert_eq!(
                got, expected,
                "{name}:\n  in:  {text:?}\n  rust: {got:?}\n  py:   {expected:?}"
            );
            checked += 1;
        }
        assert!(checked > 200, "golden подозрительно мал: {checked}");
        eprintln!("golden char_rules: {checked} cases OK");
    }
}
