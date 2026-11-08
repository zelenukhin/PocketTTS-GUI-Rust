//! Нарезка текста на чанки — 1:1 порт Python GUI (ветка comma-semantics):
//! `chunk_text` + `annotate_soft_breaks` + `terminate_bare_chunks`
//! (pocket-tts_gui.py, commit ветки `ec20ec8`). Источник — golden-тесты
//! tests/golden/chunking_golden.json, сгенерированные из фактического поведения
//! Python-кода; оба обязаны совпадать побуквенно.
//!
//! Приоритет разрезов: абзацы (\n+) → конец предложения (.!?…) → двоеточие/
//! точка с запятой/тире (:;—) → запятая → союз «и» → fallback: пробел,
//! ближайший к середине (в байтах). Чанк ограничен max_bytes UTF-8; текст
//! не теряется никогда (беспробельный кусок сверх лимита остаётся целым).

use fancy_regex::Regex;
use std::sync::OnceLock;

/// Оценка GUI: ~2 символа текста на токен, кириллица = 2 байта UTF-8.
pub const CHARS_PER_TOKEN: f32 = 2.0;
pub const BYTES_PER_CHAR: usize = 2;

/// Байтовый лимит чанка из лимита токенов (ползунок «Макс. токенов»).
pub fn compute_max_chunk_bytes(max_tokens: u32) -> usize {
    (max_tokens as f32 * CHARS_PER_TOKEN * BYTES_PER_CHAR as f32) as usize
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub text: String,
    /// Последний чанк своего абзаца.
    pub is_last: bool,
    /// Мягкая граница (разрез по запятой/«и»): паузу отыграет модель.
    pub soft_break: bool,
}

fn split_patterns() -> &'static [Regex] {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        [
            r"(?<=[.!?…])\s+",
            r"(?<=[;:—])\s+",
            r"(?<=[,])\s+",
            r"\s+(?=и\s+)",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("chunk split pattern"))
        .collect()
    })
}

fn re_comma_end() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r",$").unwrap())
}
fn re_i_end() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\bи$").unwrap())
}
fn re_i_start() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)^и\b").unwrap())
}

/// Fallback для длинных бессоюзных кусков: рекурсивный разрез по пробелу,
/// ближайшему к байтовой середине. Без пробелов — кусок целиком (сверх лимита).
fn split_at_midpoint_space(text: &str, max_bytes: usize) -> Vec<String> {
    if text.len() <= max_bytes {
        return vec![text.to_string()];
    }
    let mid = text.len() / 2;
    // Python: best_dist = len(text) + 1 (длина в СИМВОЛАХ; побайтовая дистанция
    // сравнивается с ней — воспроизводим как есть).
    let mut best_pos: Option<usize> = None;
    let mut best_dist: i64 = text.chars().count() as i64 + 1;
    let mut byte_pos = 0usize;
    for (i, ch) in text.char_indices() {
        if ch == ' ' {
            let dist = (byte_pos as i64 - mid as i64).abs();
            if dist < best_dist {
                best_dist = dist;
                best_pos = Some(i);
            }
        }
        byte_pos += ch.len_utf8();
    }
    let Some(pos) = best_pos else {
        return vec![text.to_string()];
    };
    let left = text[..pos].trim();
    let right = text[pos + 1..].trim();
    if left.is_empty() || right.is_empty() {
        return vec![text.to_string()];
    }
    let mut out = split_at_midpoint_space(left, max_bytes);
    out.extend(split_at_midpoint_space(right, max_bytes));
    out
}

/// Python `re.split(pattern, text)`: совпадения поглощаются, части между ними
/// возвращаются как есть, включая пустые (в т.ч. ведущие/хвостовые).
fn py_split<'a>(re: &Regex, text: &'a str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut last = 0usize;
    for m in re.find_iter(text).flatten() {
        out.push(&text[last..m.start()]);
        last = m.end();
    }
    out.push(&text[last..]);
    out
}

/// Порт рекурсивного split_by_pattern: режет текст паттерном, куски сверх
/// лимита уходит в рекурсию со СЛЕДУЮЩИМ паттерном, влезающие копятся в
/// буфере (склейка одним пробелом). Возвращает остаток буфера.
fn split_by_pattern(
    text: &str,
    chunks: &mut Vec<String>,
    merged_buf: String,
    patterns: &[Regex],
    max_bytes: usize,
) -> String {
    let Some((pattern, rest)) = patterns.split_first() else {
        // Резать больше нечем: буфер уходит в чанки, неделимый кусок режется
        // fallback-ом по пробелам в середине (или остаётся целиком).
        if !text.trim().is_empty() {
            if !merged_buf.is_empty() {
                chunks.push(merged_buf);
            }
            chunks.extend(split_at_midpoint_space(text.trim(), max_bytes));
            return String::new();
        }
        return merged_buf;
    };

    let parts = py_split(pattern, text);
    let mut buf = merged_buf;
    for part in parts {
        if part.len() > max_bytes {
            buf = split_by_pattern(part, chunks, buf, rest, max_bytes);
        } else {
            let sep = if buf.is_empty() { 0 } else { 1 };
            if buf.len() + part.len() + sep <= max_bytes {
                if buf.is_empty() {
                    buf.push_str(part);
                } else {
                    buf.push(' ');
                    buf.push_str(part);
                }
            } else {
                if !buf.is_empty() {
                    chunks.push(buf.clone());
                }
                buf = part.to_string();
            }
        }
    }
    buf
}

/// Нарезка текста на чанки (порт `chunk_text(return_dict=True)`).
/// Возвращает плоский список по всем абзацам; последний чанк абзаца помечен
/// `is_last`.
pub fn chunk_text(text: &str, max_bytes: usize) -> Vec<Chunk> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut chunks: Vec<Chunk> = Vec::new();
    // re.split(r'\n+', text): разрез по последовательностям \n; пустых абзацев
    // не бывает (skip ниже всё равно отсекает пустые).
    for para in text.split('\n').filter(|p| !p.is_empty()) {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        let mut local: Vec<String> = Vec::new();
        if para.len() <= max_bytes {
            local.push(para.to_string());
        } else {
            let last =
                split_by_pattern(para, &mut local, String::new(), split_patterns(), max_bytes);
            if !last.trim().is_empty() {
                local.push(last.trim().to_string());
            }
        }
        let n = local.len();
        for (i, c) in local.into_iter().enumerate() {
            chunks.push(Chunk {
                text: c,
                is_last: i + 1 == n,
                soft_break: false,
            });
        }
    }
    chunks
}

/// Порт `annotate_soft_breaks`: мягкая граница — чанк кончается запятой или
/// союзом «и», либо следующий начинается с «и» (после буквенно-цифрового
/// конца). Последний чанк абзаца мягким не считается.
#[allow(clippy::needless_range_loop)] // индекс нужен для поиска в chunks[i+1]
pub fn annotate_soft_breaks(chunks: &mut [Chunk]) {
    for i in 0..chunks.len() {
        let mut soft = false;
        if !chunks[i].is_last {
            let t = chunks[i].text.trim_end();
            soft = re_comma_end().is_match(t).unwrap_or(false)
                || re_i_end().is_match(t).unwrap_or(false);
            if !soft && i + 1 < chunks.len() {
                let last = t.chars().last();
                if last.is_some_and(|c| c.is_alphanumeric()) {
                    let next = chunks[i + 1].text.trim_start();
                    soft = re_i_start().is_match(next).unwrap_or(false);
                }
            }
        }
        chunks[i].soft_break = soft;
    }
}

/// Порт `terminate_bare_chunks` (ветка comma-semantics): чанк, кончающийся
/// «голым» словом, дестабилизирует EOS-логит и даёт задвоение последнего
/// слова. Непоследним дописываем запятую (сигнал продолжения, мягкая граница),
/// последнему — точку. Родная пунктуация не трогается.
#[allow(clippy::needless_range_loop)] // глобальная позиция чанка — суть семантики
pub fn terminate_bare_chunks(chunks: &mut [Chunk]) {
    let n = chunks.len();
    for i in 0..n {
        let text = chunks[i].text.trim_end().to_string();
        let bare = text.chars().last().is_some_and(|c| c.is_alphanumeric());
        if text.is_empty() || !bare {
            continue;
        }
        if i + 1 < n {
            chunks[i].text = format!("{text},");
            chunks[i].soft_break = true;
        } else {
            chunks[i].text = format!("{text}.");
        }
    }
}

/// Полный конвейер ветки comma-semantics:
/// `terminate_bare_chunks(annotate_soft_breaks(chunk_text(text, bytes)))`.
pub fn chunk_pipeline(text: &str, max_tokens: u32) -> Vec<Chunk> {
    let mut chunks = chunk_text(text, compute_max_chunk_bytes(max_tokens));
    annotate_soft_breaks(&mut chunks);
    terminate_bare_chunks(&mut chunks);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_formula() {
        assert_eq!(compute_max_chunk_bytes(50), 200);
        assert_eq!(compute_max_chunk_bytes(150), 600);
        assert_eq!(compute_max_chunk_bytes(300), 1200);
    }

    #[test]
    fn paragraphs_never_merge() {
        let t = "Первый абзац. Ещё предложение.\n\nВторой абзац: с двоеточием.";
        let chunks = chunk_text(t, 200);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].is_last);
        assert!(chunks[1].is_last);
    }

    #[test]
    fn sentence_split_respects_limit() {
        let t = "Первое предложение достаточно длинное чтобы существовать. Второе предложение тоже длинное и осмысленное. Третье короткое.";
        let chunks = chunk_text(t, 60);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(
                c.text.len() <= 60 || !c.text.contains(' '),
                "лимит или монолит: {}",
                c.text
            );
        }
        // Python-семантика буфера: «и осмысленное.» (26 Б) + «Третье короткое.»
        // (32 Б) = 59 Б ≤ 60 → склеиваются в один хвостовой чанк.
        assert_eq!(
            chunks.last().unwrap().text,
            "и осмысленное. Третье короткое."
        );
        // текст не теряется
        let joined: String = chunks
            .iter()
            .flat_map(|c| c.text.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        let src: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(joined, src);
    }

    #[test]
    fn midpoint_fallback_splits_monster() {
        let t = "длинноесловораз длинноесловодва длинноесловотри длинноесловочетыре длинноесловопять длинноесловошесть";
        let chunks = chunk_text(t, 40);
        assert!(chunks.len() >= 2);
        // текст не потерялся
        let joined: String = chunks
            .iter()
            .flat_map(|c| c.text.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        let src: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(joined, src);
    }

    #[test]
    fn terminate_bare_comma_semantics() {
        let mut chunks = vec![
            Chunk {
                text: "Слово без знака".into(),
                is_last: false,
                soft_break: false,
            },
            Chunk {
                text: "Финал без знака".into(),
                is_last: true,
                soft_break: false,
            },
        ];
        terminate_bare_chunks(&mut chunks);
        assert_eq!(chunks[0].text, "Слово без знака,");
        assert!(chunks[0].soft_break);
        assert_eq!(chunks[1].text, "Финал без знака.");
    }

    #[test]
    fn soft_break_on_comma_and_i() {
        let mut chunks = vec![
            Chunk {
                text: "Она открыла окно,".into(),
                is_last: false,
                soft_break: false,
            },
            Chunk {
                text: "посмотрела на улицу".into(),
                is_last: false,
                soft_break: false,
            },
            Chunk {
                text: "и вздохнула.".into(),
                is_last: false,
                soft_break: false,
            },
            Chunk {
                text: "Конец.".into(),
                is_last: true,
                soft_break: false,
            },
        ];
        annotate_soft_breaks(&mut chunks);
        assert!(chunks[0].soft_break, "запятая в конце");
        assert!(chunks[1].soft_break, "следующий начинается с «и»");
        // «и вздохнула.» кончается точкой, следующий не с «и» → не мягкая
        assert!(!chunks[2].soft_break);
    }

    #[test]
    fn golden_corpus_matches_python() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/golden/chunking_golden.json"
        );
        let raw = std::fs::read_to_string(path)
            .expect("golden-файл chunking_golden.json отсутствует: запустите tools/gen_chunking_golden.py (venv-питоном) и повторите");
        let cases: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("golden json");
        let mut checked = 0usize;
        for case in &cases {
            let name = case["name"].as_str().unwrap_or("?");
            let max_tokens = case["max_tokens"].as_u64().unwrap() as u32;
            let input = case["input"].as_str().unwrap();
            let got = chunk_pipeline(input, max_tokens);
            let exp: Vec<serde_json::Value> = case["chunks"].as_array().unwrap().clone();
            assert_eq!(got.len(), exp.len(), "{name}/{max_tokens}: число чанков");
            for (i, g) in got.iter().enumerate() {
                assert_eq!(
                    g.text,
                    exp[i]["text"].as_str().unwrap(),
                    "{name}/{max_tokens} chunk {i}: текст\n  rust: {:?}\n  py:   {:?}",
                    g.text,
                    exp[i]["text"].as_str().unwrap()
                );
                assert_eq!(
                    g.is_last,
                    exp[i]["is_last"].as_bool().unwrap(),
                    "{name}/{max_tokens} chunk {i}: is_last"
                );
                assert_eq!(
                    g.soft_break,
                    exp[i]["soft_break"].as_bool().unwrap(),
                    "{name}/{max_tokens} chunk {i}: soft_break ({:?})",
                    g.text
                );
            }
            // текст не теряется (по буквам/цифрам, как в генераторе)
            let letters =
                |s: &str| -> String { s.chars().filter(|c| c.is_alphanumeric()).collect() };
            let src: String = letters(input);
            let dst: String = got.iter().map(|c| letters(&c.text)).collect();
            assert_eq!(src, dst, "{name}/{max_tokens}: потеря текста");
            checked += 1;
        }
        assert!(checked > 50, "golden пуст?");
        eprintln!("golden: {checked} cases OK");
    }
}
