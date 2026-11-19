//! Разбор SubRip (.srt) — 1:1 порт `parse_srt`/`strip_srt_tags` из
//! Python-эталона `C:\PocketTTS-GUI\PocketTTS-GUI\pocket-tts-srt_gui.py`.
//!
//! Контракт этапа SRT.1 — docs/SRT_0_AUDIT_CONTRACT.md §3:
//! * блоки по пустым строкам (`\n\s*\n`), таймкод в строке 0 или 1 блока
//!   (`HH:MM:SS,mmm` или `HH:MM:SS.mmm` — ровно 2 цифры ч/м/с и 3 мс);
//! * индекс блока — `int` строки 0 (при таймкоде на строке 1), иначе
//!   последовательный номер; сортировка СТАБИЛЬНАЯ по индексу (политика
//!   Python: хронология не переупорядочивается молча);
//! * реплики, пустые после очистки тегов, НЕ попадают в cues — их
//!   таймкоды участвуют только в расчёте пауз планировщика SRT;
//! * предупреждения (не фатальны): пропущенные блоки, end <= start,
//!   дубликаты индексов, нарушение хронологии, перекрытия, длительность
//!   > 60 с. Фатальна только пустая выдача (нет ни одного блока).
//!
//! Отдельные оговорки порта (документированные):
//! * html-entities: именованные — кураторская таблица (~50 частых),
//!   только с `;`; числовые `&#NNN;`/`&#xHH;` — полная поддержка;
//!   неизвестные остаются как есть (Python html.unescape знает все
//!   имена HTML5 и часть форм без `;`);
//! * `int()` индекса: принимаются те же формы, что Rust `i64::from_str`
//!   (знак/ведущие нули); подчёркивания (`int("1_0")`) не поддержаны.

use fancy_regex::Regex;
use std::sync::OnceLock;

/// Реплика субтитров после разбора. `index` — исходный номер блока SRT
/// (i64 — паритет с Python int, возможны отрицательные/дубликаты);
/// последовательная позиция реплики — её индекс в `SrtParseResult::cues`
/// (присваивается после стабильной сортировки по `index`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleCue {
    pub index: i64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Результат разбора SRT: реплики (по порядку индексов) + предупреждения.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SrtParseResult {
    pub cues: Vec<SubtitleCue>,
    pub warnings: Vec<String>,
}

/// Максимальная «нормальная» длительность реплики для предупреждения.
const LONG_CUE_MS: u64 = 60_000;

fn re_timecode() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(\d{2}:\d{2}:\d{2}[,.]\d{3})\s*-->\s*(\d{2}:\d{2}:\d{2}[,.]\d{3})")
            .expect("srt timecode pattern")
    })
}

fn re_ass_tag() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\{\\[^}]*\}").expect("srt ass tag pattern"))
}

fn re_html_tag() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"</?[A-Za-z][^<>]*>").expect("srt html tag pattern"))
}

fn re_block_gap() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\n\s*\n").expect("srt block gap pattern"))
}

/// Чтение байтов SRT как Python `open(..., encoding='utf-8-sig',
/// errors='replace')`: пропуск UTF-8 BOM, битые байты → U+FFFD.
pub fn decode_srt_bytes(bytes: &[u8]) -> String {
    let bytes = match bytes.strip_prefix("\u{feff}".as_bytes()) {
        Some(rest) => rest,
        None => bytes,
    };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Таймкод `HH:MM:SS,mmm` / `HH:MM:SS.mmm` в миллисекунды.
/// Формат гарантируется regex; паритет Python `time_to_ms`.
fn timecode_ms(s: &str) -> u64 {
    let (hms, ms) = s.rsplit_once([',', '.']).expect("srt timecode separator");
    let mut parts = hms.split(':');
    let h: u64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let m: u64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let sec: u64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let ms: u64 = ms.parse().unwrap_or(0);
    (h * 3600 + m * 60 + sec) * 1000 + ms
}

fn re_whitespaces_run() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[^\S\n]+").expect("srt whitespace run pattern"))
}

fn re_newlines_run() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\n{3,}").expect("srt newline run pattern"))
}

fn re_replace_all(re: &Regex, text: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for m in re.find_iter(text).flatten() {
        out.push_str(&text[last..m.start()]);
        out.push_str(replacement);
        last = m.end();
    }
    out.push_str(&text[last..]);
    out
}

/// Очистка текста реплики от разметки — порт `strip_srt_tags`:
/// ASS-оверрайды `{\...}` и HTML-теги заменяются пробелом (склейка слов
/// вокруг удалённой разметки не слипается), декодируются entities,
/// NBSP → пробел, пробельные прогоны (кроме `\n`) схлопываются,
/// 3+ перевода строк — в два, края чистятся от пробелов/переводов.
/// Переводы строк СОХРАНЯЮТСЯ: по ним чанкер режет абзацы.
pub fn strip_srt_tags(text: &str) -> String {
    let text = re_replace_all(re_ass_tag(), text, " ");
    let text = re_replace_all(re_html_tag(), &text, " ");
    let text = html_unescape(&text).replace('\u{a0}', " ");
    let text = re_replace_all(re_whitespaces_run(), &text, " ");
    let text = re_replace_all(re_newlines_run(), &text, "\n\n");
    text.trim_matches(|c| c == ' ' || c == '\n').to_string()
}

/// Кураторская таблица именованных entities (с `;`). Расширение HTML5
/// из Python не переносится — покрываются частые в субтитрах имена.
fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "ndash" => "–",
        "mdash" => "—",
        "hellip" => "…",
        "laquo" => "«",
        "raquo" => "»",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "bull" => "•",
        "dagger" => "†",
        "Dagger" => "‡",
        "permil" => "‰",
        "prime" => "′",
        "Prime" => "″",
        "times" => "×",
        "divide" => "÷",
        "minus" => "−",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "deg" => "°",
        "plusmn" => "±",
        "frac12" => "½",
        "frac14" => "¼",
        "frac34" => "¾",
        "sup2" => "²",
        "sup3" => "³",
        "micro" => "µ",
        "middot" => "·",
        "sect" => "§",
        "para" => "¶",
        "iexcl" => "¡",
        "iquest" => "¿",
        "szlig" => "ß",
        "agrave" => "à",
        "aacute" => "á",
        "eacute" => "é",
        "egrave" => "è",
        "iacute" => "í",
        "oacute" => "ó",
        "uacute" => "ú",
        "yacute" => "ý",
        "ccedil" => "ç",
        "ntilde" => "ñ",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        _ => return None,
    })
}

fn push_entity(out: &mut String, body: &str) -> bool {
    // body — содержимое между '&' и ';' (без них).
    if let Some(num) = body.strip_prefix('#') {
        let cp = if let Some(hex) = num.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            num.parse::<u32>().ok()
        };
        match cp.and_then(char::from_u32) {
            Some(c) => out.push(c),
            // Python chr() пропускает суррогаты, UTF-8 Rust — нет;
            // некодируемая позиция → U+FFFD (как ошибки декодирования).
            None => out.push('\u{fffd}'),
        }
        return true;
    }
    if let Some(s) = named_entity(body) {
        out.push_str(s);
        true
    } else {
        false
    }
}

/// Порт `html.unescape` в объёме таблицы выше: `&name;`, `&#NNN;`,
/// `&#xHH;`. Неизвестные имена и формы без `;` остаются как есть.
fn html_unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // Сбор тела entity: буквы/цифры до ';' (не длиннее 32 символов).
        let mut j = i + 1;
        let mut body = String::new();
        while j < chars.len()
            && body.len() < 32
            && (chars[j].is_ascii_alphanumeric() || chars[j] == '#')
        {
            if chars[j] == '#' && j != i + 1 {
                break; // '#' допустим только первым символом тела
            }
            body.push(chars[j]);
            j += 1;
        }
        if !body.is_empty() && j < chars.len() && chars[j] == ';' && push_entity(&mut out, &body) {
            i = j + 1;
        } else {
            out.push('&');
            i += 1;
        }
    }
    out
}

/// Разбор содержимого SRT (без BOM — см. `decode_srt_bytes`) в реплики.
/// Порт `parse_srt`: блоки по `\n\s*\n`, таймкод на строке 0/1, индекс
/// из строки 0 (int) или последовательный, пустой текст → блок молча
/// пропускается (как Python), стабильная сортировка по индексу.
/// Предупреждения — о пропущенных блоках и аномалиях (см. шапку модуля).
pub fn parse_srt(content: &str) -> SrtParseResult {
    let mut res = SrtParseResult::default();
    let content = content.trim();
    if content.is_empty() {
        res.warnings
            .push("SRT пуст: нет ни одного блока".to_string());
        return res;
    }

    // Python re.split(r'\n\s*\n', content): разделители — неперекрывающиеся
    // match'и паттерна, части между ними.
    let mut blocks: Vec<&str> = Vec::new();
    let mut last = 0usize;
    for m in re_block_gap().find_iter(content).flatten() {
        blocks.push(&content[last..m.start()]);
        last = m.end();
    }
    blocks.push(&content[last..]);

    let mut skipped_no_timecode = 0usize;
    let mut skipped_no_text = 0usize;
    for (block_no, block) in blocks.iter().enumerate() {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let lines: Vec<&str> = block.split('\n').map(|l| l.trim()).collect();
        if lines.len() < 2 {
            skipped_no_timecode += 1;
            res.warnings.push(format!(
                "блок {}: меньше двух строк — пропущен",
                block_no + 1
            ));
            continue;
        }
        let time_line_idx = match [0usize, 1usize]
            .iter()
            .find(|&&i| re_timecode().is_match(lines[i]).unwrap_or(false))
        {
            Some(&i) => i,
            None => {
                skipped_no_timecode += 1;
                res.warnings.push(format!(
                    "блок {}: таймкод не найден — пропущен",
                    block_no + 1
                ));
                continue;
            }
        };
        let Some(caps) = re_timecode()
            .captures(lines[time_line_idx])
            .expect("timecode regex")
        else {
            skipped_no_timecode += 1;
            res.warnings.push(format!(
                "блок {}: таймкод не найден — пропущен",
                block_no + 1
            ));
            continue;
        };
        let start = caps.get(1).expect("start group").as_str();
        let end = caps.get(2).expect("end group").as_str();
        let text = lines[time_line_idx + 1..]
            .join("\n")
            .trim_matches(|c| c == ' ' || c == '\n')
            .to_string();
        if text.is_empty() {
            skipped_no_text += 1;
            continue; // Python: пустой текст молча пропускается
        }
        let index = if time_line_idx == 1 {
            lines[0]
                .parse::<i64>()
                .unwrap_or_else(|_| res.cues.len() as i64 + 1)
        } else {
            res.cues.len() as i64 + 1
        };
        res.cues.push(SubtitleCue {
            index,
            start_ms: timecode_ms(start),
            end_ms: timecode_ms(end),
            text,
        });
    }

    if res.cues.is_empty() {
        res.warnings.push(format!(
            "в файле нет ни одной реплики с текстом и таймкодом (пропущено блоков: без таймкода {}, без текста {})",
            skipped_no_timecode, skipped_no_text
        ));
        return res;
    }

    // Стабильная сортировка по индексу — политика Python.
    res.cues.sort_by_key(|c| c.index);

    // Диагностика (не фатальна): дубликаты, аномалии, хронология.
    let mut dup_reported = false;
    for w in res.cues.windows(2) {
        if w[0].index == w[1].index && !dup_reported {
            res.warnings.push(format!(
                "дублирующиеся индексы реплик (например {})",
                w[0].index
            ));
            dup_reported = true;
        }
        if w[1].start_ms < w[0].start_ms {
            res.warnings.push(format!(
                "реплика [{}]: начало раньше начала предыдущей — порядок индексов нарушает хронологию",
                w[1].index
            ));
        }
        if w[1].start_ms < w[0].end_ms {
            res.warnings.push(format!(
                "реплики [{}] и [{}] перекрываются",
                w[0].index, w[1].index
            ));
        }
    }
    for c in &res.cues {
        if c.end_ms <= c.start_ms {
            res.warnings.push(format!(
                "реплика [{}]: конец не позже начала ({}–{} мс)",
                c.index, c.start_ms, c.end_ms
            ));
        }
        if c.end_ms.saturating_sub(c.start_ms) > LONG_CUE_MS {
            res.warnings.push(format!(
                "реплика [{}]: длительность больше {} мс ({})",
                c.index,
                LONG_CUE_MS,
                c.end_ms - c.start_ms
            ));
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(index: i64, start: u64, end: u64, text: &str) -> SubtitleCue {
        SubtitleCue {
            index,
            start_ms: start,
            end_ms: end,
            text: text.to_string(),
        }
    }

    #[test]
    fn standard_srt() {
        let srt = "1\n00:00:01,000 --> 00:00:03,500\nПривет, мир!\n\n2\n00:00:04,000 --> 00:00:06,000\nВторая реплика\n";
        let r = parse_srt(srt);
        assert_eq!(
            r.cues,
            vec![
                cue(1, 1000, 3500, "Привет, мир!"),
                cue(2, 4000, 6000, "Вторая реплика")
            ]
        );
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn bom_and_lossy_bytes() {
        let mut bytes: Vec<u8> = "\u{feff}".as_bytes().to_vec();
        bytes.extend_from_slice(
            b"1\n00:00:00,000 --> 00:00:01,000\n\xd0\x9f\xd1\x80\xd0\xb8\xd0\xb2\xd0\xb5\xd1\x82",
        );
        // последний байт обрезан — lossy даст U+FFFD
        bytes.pop();
        let s = decode_srt_bytes(&bytes);
        assert!(!s.starts_with('\u{feff}'));
        let r = parse_srt(&s);
        assert_eq!(r.cues.len(), 1);
        assert!(r.cues[0].text.ends_with('\u{fffd}'), "{:?}", r.cues[0].text);
    }

    #[test]
    fn crlf_line_endings() {
        let srt = "1\r\n00:00:01,000 --> 00:00:02,000\r\nСтрока одна\r\nСтрока вторая\r\n\r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nЕщё\r\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues.len(), 2);
        assert_eq!(r.cues[0].text, "Строка одна\nСтрока вторая");
        assert_eq!(r.cues[1].text, "Ещё");
    }

    #[test]
    fn dot_timecode_variant() {
        let srt = "1\n00:01:02.345 --> 00:01:03.500\nТочка\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues, vec![cue(1, 62_345, 63_500, "Точка")]);
    }

    #[test]
    fn missing_block_number() {
        let srt = "00:00:01,000 --> 00:00:02,000\nБез номера\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues, vec![cue(1, 1000, 2000, "Без номера")]);
    }

    #[test]
    fn non_numeric_block_number_falls_back_sequential() {
        let srt = "abc\n00:00:01,000 --> 00:00:02,000\nПервая\n\ndef\n00:00:03,000 --> 00:00:04,000\nВторая\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues[0].index, 1);
        assert_eq!(r.cues[1].index, 2);
    }

    #[test]
    fn multiline_text_preserved() {
        let srt = "1\n00:00:01,000 --> 00:00:04,000\nПервая строка\n\nВторая строка после пустой внутри блока\nТретья\n";
        let r = parse_srt(srt);
        // Пустая строка внутри блока разделяет блоки в Python (re.split \n\s*\n)
        assert_eq!(r.cues.len(), 1);
        assert_eq!(r.cues[0].text, "Первая строка"); // хвост ушёл в блок без таймкода
        assert!(!r.warnings.is_empty());
    }

    #[test]
    fn html_and_ass_tags_removed() {
        assert_eq!(
            strip_srt_tags("<i>Курсив</i> и <b>жирный</b>"),
            "Курсив и жирный"
        );
        assert_eq!(strip_srt_tags("{\\an8}Сверху"), "Сверху");
        assert_eq!(strip_srt_tags("{\\pos(10,20)}Позиция"), "Позиция");
        assert_eq!(
            strip_srt_tags("<font color=\"#ff0000\">Красный</font>"),
            "Красный"
        );
        assert_eq!(strip_srt_tags("Слово<BR>слово"), "Слово слово");
        // пробел после '<' ломает шаблон </?[A-Za-z]… — как в Python не тег
        assert_eq!(strip_srt_tags("Слово< BR >слово"), "Слово< BR >слово");
    }

    #[test]
    fn plain_braces_without_backslash_kept() {
        // {\...} требует бэкслэш — {музыка} остаётся как в Python
        assert_eq!(strip_srt_tags("{музыка} играет"), "{музыка} играет");
    }

    #[test]
    fn entities_decoded() {
        assert_eq!(strip_srt_tags("А &amp; Б"), "А & Б");
        assert_eq!(strip_srt_tags("&lt;i&gt; не тег"), "<i> не тег");
        assert_eq!(
            strip_srt_tags("Апостроф &#39;здесь&#39;"),
            "Апостроф 'здесь'"
        );
        assert_eq!(strip_srt_tags("Hex &#x414;&#x443;"), "Hex Ду"); // U+0414 Д, U+0443 у
        assert_eq!(strip_srt_tags("Многоточие&hellip;"), "Многоточие…");
        assert_eq!(
            strip_srt_tags("Неразрывный&nbsp;пробел"),
            "Неразрывный пробел"
        );
        // неизвестное имя и форма без ';' остаются как есть
        assert_eq!(strip_srt_tags("&unknown; и &amp"), "&unknown; и &amp");
    }

    #[test]
    fn whitespace_and_newline_normalization() {
        // пробельные прогоны → одиночный пробел, \n{3,} → \n\n, края чистые
        assert_eq!(
            strip_srt_tags("  много   пробелов\tи \t табов  "),
            "много пробелов и табов"
        );
        assert_eq!(strip_srt_tags("а\n\n\n\nб"), "а\n\nб");
        assert_eq!(strip_srt_tags("\n\n  текст  \n\n"), "текст");
    }

    #[test]
    fn empty_after_cleanup_dropped_silently() {
        // parse_srt теги не чистит (это делает планировщик SRT позже):
        // «<i></i>» для парсера непуст — реплика остаётся. Очистка
        // strip_srt_tags опустошает её уже в SRT.5 (реплика без чанков).
        let srt =
            "1\n00:00:01,000 --> 00:00:02,000\n<i></i>\n\n2\n00:00:03,000 --> 00:00:04,000\nРечь\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues.len(), 2);
        assert_eq!(r.cues[1].text, "Речь");
        // а сама очистка опустошает разметочные реплики (курируется SRT.5)
        assert_eq!(strip_srt_tags("<i></i>"), "");
        assert_eq!(strip_srt_tags("{\\an8}  "), "");
    }

    #[test]
    fn invalid_timecode_block_skipped_with_warning() {
        // однозначные часы — вне строгого формата \d{2}: блок пропускается
        let srt = "1\n1:02:03,400 --> 1:02:05,000\nНеформат\n\n2\n00:02:03,400 --> 00:02:05,000\nФормат\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues.len(), 1);
        assert_eq!(r.cues[0].text, "Формат");
        assert!(
            r.warnings.iter().any(|w| w.contains("таймкод не найден")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn garbage_lines_around_timecode_tolerated() {
        // Python .search — таймкод ищется в любом месте строки 0/1
        let srt = "1\nмусор 00:00:01,000 --> 00:00:02,000 хвост\nРечь\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues.len(), 1);
        assert_eq!(r.cues[0].text, "Речь");
    }

    #[test]
    fn shuffled_indices_sorted_by_index() {
        let srt = "3\n00:00:09,000 --> 00:00:10,000\nТри\n\n1\n00:00:01,000 --> 00:00:02,000\nОдин\n\n2\n00:00:04,000 --> 00:00:05,000\nДва\n";
        let r = parse_srt(srt);
        let texts: Vec<&str> = r.cues.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["Один", "Два", "Три"]);
        // порядок индексов нарушает хронологию? нет — после сортировки всё монотонно
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn stable_sort_for_equal_indices() {
        let srt = "5\n00:00:01,000 --> 00:00:02,000\nПервый\n\n5\n00:00:03,000 --> 00:00:04,000\nВторой\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues[0].text, "Первый");
        assert_eq!(r.cues[1].text, "Второй");
        assert!(r.warnings.iter().any(|w| w.contains("дублирующиеся")));
    }

    #[test]
    fn negative_index_sorts_first() {
        let srt = "1\n00:00:05,000 --> 00:00:06,000\nПоложительный\n\n-2\n00:00:01,000 --> 00:00:02,000\nОтрицательный\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues[0].index, -2);
        assert_eq!(r.cues[0].text, "Отрицательный");
    }

    #[test]
    fn overlapping_cues_warned_not_reordered() {
        let srt = "1\n00:00:01,000 --> 00:00:05,000\nПервый длинный\n\n2\n00:00:03,000 --> 00:00:06,000\nВторой внахлёст\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues.len(), 2);
        assert_eq!(r.cues[0].text, "Первый длинный");
        assert!(
            r.warnings.iter().any(|w| w.contains("перекрываются")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn end_before_start_warned() {
        let srt = "1\n00:00:05,000 --> 00:00:02,000\nИнвертированная\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues.len(), 1);
        assert!(r
            .warnings
            .iter()
            .any(|w| w.contains("конец не позже начала")));
    }

    #[test]
    fn long_cue_warned() {
        let srt = "1\n00:00:00,000 --> 00:02:00,000\nОчень длинная реплика\n";
        let r = parse_srt(srt);
        assert!(r.warnings.iter().any(|w| w.contains("больше 60000")));
    }

    #[test]
    fn non_monotonic_by_index_warned() {
        // индексы отсортированы, но времена идут назад
        let srt =
            "1\n00:00:20,000 --> 00:00:21,000\nПозже\n\n2\n00:00:01,000 --> 00:00:02,000\nРаньше\n";
        let r = parse_srt(srt);
        assert_eq!(r.cues[0].text, "Позже");
        assert!(
            r.warnings.iter().any(|w| w.contains("нарушает хронологию")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn empty_content_is_fatal_warning() {
        let r = parse_srt("   \n  \n");
        assert!(r.cues.is_empty());
        assert!(r.warnings.iter().any(|w| w.contains("SRT пуст")));
        let r = parse_srt("");
        assert!(r.cues.is_empty());
        assert!(!r.warnings.is_empty());
    }

    #[test]
    fn no_valid_blocks_warning() {
        let r = parse_srt("какой-то мусор\nбез таймкодов\n\nещё блок\n");
        assert!(r.cues.is_empty());
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("нет ни одной реплики")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn hours_minutes_seconds_boundary_values() {
        // Второй таймкод «100:00:00,000» не удовлетворяет \d{2}:… — и весь
        // regex не матчится (второй группы после --> нет), поэтому Python
        // блок пропускает. Паритет: блок пропущен с предупреждением.
        let srt = "1\n99:59:59,999 --> 100:00:00,000\nГраница\n";
        let r = parse_srt(srt);
        assert!(r.cues.is_empty(), "{:?}", r.cues);
        assert!(
            r.warnings.iter().any(|w| w.contains("таймкод не найден")),
            "{:?}",
            r.warnings
        );
        // 99 часов — валидные две цифры, парсится
        let r2 = parse_srt("1\n99:59:59,999 --> 99:59:59,999\nХвост суток\n");
        assert_eq!(r2.cues[0].start_ms, 359_999_999);
    }

    #[test]
    fn strip_only_spaces_and_newlines_at_edges() {
        // внутри текста всё уже нормализовано; концевые \r тоже ушли
        assert_eq!(strip_srt_tags("текст\n"), "текст");
        assert_eq!(strip_srt_tags("\nтекст"), "текст");
    }
}
