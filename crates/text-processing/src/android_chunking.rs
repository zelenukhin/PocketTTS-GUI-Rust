//! Режим чанкования «TeraTTS Android» (G7): порт PocketTTS-пути приложения
//! TeraTTS 1.59 (teratts-android, commit `ec0537f`) для файлового синтеза.
//!
//! Источник функций — проверенный harness
//! `out/chunking_android_vs_windows/harness/src/` (дословные копии
//! ec0537f; эквивалентность доказана verification/harness_validation.md):
//! `android_tera.rs` (tera.rs), `android_helper.rs` (helper.rs:371-528),
//! `android_kotlin.rs` (порты Kotlin TTS-пути). Отличия от harness:
//! глобальные static-настройки Android (paragraph_pause/pause_scale)
//! заменены параметрами `AndroidChunkOptions`; U+2063/EMA-латентность и
//! плеерный путь НЕ переносятся (осознанные отклонения, SPEC.md §5).
//! fastStart ПЕРЕНОСИТСЯ дословно: эталон 99 чанков сгенерирован с ним
//! (первый юнит — упакованная фраза 63 симв. > 55; SPEC_CORRECTIONS 0.8).
//!
//! Нормативные документы: out/teratts_android_chunking_spec/ (SPEC.md и
//! раскрытия) + docs/G7_SPEC_CORRECTIONS.md. Семантика Android
//! воспроизводится как есть — «улучшения» (границы при строчной букве,
//! микрочанки, превышение лимита неделимым словом) запрещены.

use fancy_regex::Regex as FRegex;
use regex::Regex;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

use serde::{Deserialize, Serialize};

/// Режим разбиения текста (GUI «Алгоритм разбиения текста», G7).
///
/// Сериализация — нормативный JSON-контракт
/// `"chunking_mode": "standard" | "teratts_android"`: `rename_all =
/// "lowercase"` дал бы `"terattsandroid"`, поэтому вариант переименован
/// явно (SPEC_CORRECTIONS 0.1). Неизвестное значение — ошибка чтения
/// конфига, молчаливая подмена режима не допускается.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChunkingMode {
    /// Существующий Windows-конвейер: `chunk_pipeline` + `max_tokens`
    /// (не меняется).
    #[default]
    Standard,
    /// Конвейер teratts-android: `android_chunk_pipeline` +
    /// `max_chunk_chars` (Unicode-символы).
    #[serde(rename = "teratts_android")]
    TeraTtsAndroid,
}

/// Лимит Pocket-чанка в Android (pocket.rs:27 `POCKET_MAX_CHUNK_CHARS`) —
/// значение по умолчанию настройки `max_chunk_chars`.
pub const DEFAULT_MAX_CHUNK_CHARS: usize = 150;

/// Kotlin-лимит фразы вызова (PlaybackPrefs.ChunkMode.DEFAULT, 300) —
/// КОНСТАНТА режима, не пользовательская настройка (SPEC.md §7).
pub const CALL_CHUNK_LIMIT: usize = 300;

/// Стык между чанками одного вызова (helper.rs `silence_duration` = 0.02,
/// эквивалент MIN_JOINT_S Standard).
pub const SILENCE_JOINT_S: f32 = 0.02;

/// Параметры Android-чанкования (передаются явно; в Android это
/// глобальные настройки движка).
#[derive(Debug, Clone, PartialEq)]
pub struct AndroidChunkOptions {
    /// Лимит чанка в Unicode-символах (chars().count()); 150 — эталон.
    pub max_chunk_chars: usize,
    /// Пауза конца абзаца, с (настройка GUI `paragraph_pause_s`, 0.9).
    pub paragraph_pause_s: f32,
    /// Множитель ВСЕХ пауз плана (настройка GUI `pause_scale`, 1.0).
    pub pause_scale: f32,
    /// Kotlin-лимит фразы вызова (константа 300; параметр для тестов).
    pub call_chunk_limit: usize,
}

impl Default for AndroidChunkOptions {
    fn default() -> Self {
        Self {
            max_chunk_chars: DEFAULT_MAX_CHUNK_CHARS,
            paragraph_pause_s: 0.9,
            pause_scale: 1.0,
            call_chunk_limit: CALL_CHUNK_LIMIT,
        }
    }
}

/// Диагностическая причина/тип границы чанка (в производственный `Task`
/// не входит; журнал/тесты).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AndroidBoundary {
    /// Внутри-вызовной стык (пауза фразы/aside-переход/пол 0.02).
    IntraPhrase,
    /// Хвост вызова; `paragraph` — конец абзаца (пол paragraph_pause_s).
    CallTail { paragraph: bool },
    /// Юнит-сцена «* * *» (пауза PAUSE_SCENE, без генерации).
    SceneBreak,
    /// Глобально последний чанк файла (пауза не вставляется).
    EndOfFile,
}

/// Один элемент плана Android-чанкования.
#[derive(Debug, Clone, PartialEq)]
pub struct AndroidChunk {
    /// Текст чанка после всех стадий разбиения («+»-нотация; в журнал).
    pub text: String,
    /// Модельный текст `pre_normalize → prepare_pocket` (U+0301);
    /// None = фраза отброшена движком (нет кириллицы) либо юнит-сцена.
    pub model_text: Option<String>,
    /// Пауза ПОСЛЕ чанка, с; None — глобально последний чанк файла.
    pub pause_s: Option<f32>,
    /// Скобочная ремарка (aside) — диагностика.
    pub is_aside: bool,
    /// Последний чанк вызова конца абзаца (U+2029) — диагностика.
    pub paragraph_end: bool,
    pub boundary_kind: AndroidBoundary,
}

impl AndroidChunk {
    /// Юнит-сцена-разделитель (текст не синтезируется). Определяется по
    /// тексту: сцена в конце файла получает boundary EndOfFile (пауза
    /// снимается переопределением последнего элемента).
    pub fn is_scene(&self) -> bool {
        self.text == SCENE_BREAK || is_scene_break(&self.text)
    }
}

// ============================================================================
// Kotlin-стадия K2: TextNormalizer.splitIntoSentences (KT:438-556)
// ============================================================================

/// TextNormalizer.kt:49-57 — английские аббревиатуры (плейсхолдеры).
const KT_ABBREVIATIONS: &[&str] = &[
    "Mr.", "Mrs.", "Dr.", "Ms.", "Prof.", "Sr.", "Jr.", "etc.", "vs.", "e.g.", "i.e.", "Jan.",
    "Feb.", "Mar.", "Apr.", "May.", "Jun.", "Jul.", "Aug.", "Sep.", "Oct.", "Nov.", "Dec.", "U.S.",
    "U.K.", "E.U.",
];

/// TextNormalizer.kt:59-60 — lookbehind переменной длины
/// `(?<=[.!?]['"”’]?)` разложен на две альтернативы фиксированной длины
/// (множество матчей идентично — доказано harness_validation.md).
static SENTENCE_SPLIT: LazyLock<FRegex> = LazyLock::new(|| {
    FRegex::new(
        r#"(?<=[.!?])\s+(?=["“‘]?[\p{L}\d])|(?<=[.!?]["”’])\s+(?=["“‘]?[\p{L}\d])|(?<=;)\s+"#,
    )
    .expect("sentence split pattern")
});

/// TextNormalizer.kt:61.
static COMMA_SPLIT: LazyLock<FRegex> =
    LazyLock::new(|| FRegex::new(r"(?<=,)\s+").expect("comma split pattern"));

/// Volatile punctuation (KT:504-516, дефолты PunctuationPrefs: tight*=false):
/// пробел ПЕРЕД конечными `! ? , ;` фразы (replaceFirst).
static VOLATILE_END: LazyLock<FRegex> =
    LazyLock::new(|| FRegex::new(r#"([!?,;])(['"”’]?)\s*$"#).expect("volatile end pattern"));

static ABBR_PATTERNS: LazyLock<Vec<(FRegex, String)>> = LazyLock::new(|| {
    KT_ABBREVIATIONS
        .iter()
        .enumerate()
        .map(|(index, abbr)| {
            let pat = FRegex::new(&format!(r"(?i)\b{}", regex_escape(abbr))).expect("abbr pattern");
            (pat, format!("__ABBR{index}__"))
        })
        .collect()
});

/// Java `Pattern.split` / Kotlin `String.split(Pattern)`: части между
/// непересекающимися матчами; Java отбрасывает только хвостовые пустые.
fn java_split<'a>(re: &FRegex, text: &'a str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut last = 0usize;
    for m in re.find_iter(text).flatten() {
        out.push(&text[last..m.start()]);
        last = m.end();
    }
    out.push(&text[last..]);
    while out.len() > 1 && out.last().is_some_and(|s| s.is_empty()) {
        out.pop();
    }
    out
}

/// `Pattern.quote`-эквивалент для fancy-regex.
fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out
}

/// Плейсхолдеры аббревиатур (KT:448-452, CASE_INSENSITIVE, \b+quote).
fn protect_abbreviations(text: &str) -> String {
    let mut t = text.to_string();
    for (pat, placeholder) in ABBR_PATTERNS.iter() {
        t = pat.replace_all(&t, placeholder.as_str()).into_owned();
    }
    t
}

fn restore_abbreviations(text: &str) -> String {
    let mut t = text.to_string();
    for (index, abbr) in KT_ABBREVIATIONS.iter().enumerate() {
        t = t.replace(&format!("__ABBR{index}__"), abbr);
    }
    t
}

/// TextNormalizer.kt:438-556 `splitIntoSentences`: предложения и фразы
/// вызова ≤ `chunk_limit` (DEFAULT 300). Длина — `chars().count()`
/// (SPEC.md §4.5: единая Unicode-семантика в Rust).
pub fn split_into_sentences(text: &str, chunk_limit: usize) -> Vec<String> {
    let protected = protect_abbreviations(text);
    let raw_sentences = java_split(&SENTENCE_SPLIT, &protected);

    // KT:459-488: длинные (>лимит) — разрез по запятым со склейкой,
    // конечные `,`/`;` ЗАКРЫВАЮЩЕГО куска удаляются («Fix audio cutoff»)
    let mut refined: Vec<String> = Vec::new();
    for raw in &raw_sentences {
        if raw.chars().count() <= chunk_limit {
            refined.push(raw.to_string());
        } else {
            let sub_parts = java_split(&COMMA_SPLIT, raw);
            let mut current = String::new();
            for part in &sub_parts {
                if current.chars().count() + part.chars().count() < chunk_limit {
                    if !current.is_empty() {
                        current.push(' ');
                    }
                    current.push_str(part);
                } else {
                    if !current.is_empty() {
                        if current.ends_with(',') || current.ends_with(';') {
                            current.pop();
                        }
                        refined.push(current.clone());
                        current.clear();
                    }
                    current.push_str(part);
                }
            }
            if !current.is_empty() {
                refined.push(current);
            }
        }
    }

    // KT:490-497: восстановление аббревиатур + trim + фильтр пустых
    let processed: Vec<String> = refined
        .into_iter()
        .map(|s| restore_abbreviations(&s).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // KT:518-553: склейка предложений во фразы вызова ≤ chunk_limit
    let mut chunked: Vec<String> = Vec::new();
    let mut cur = String::new();
    for mut sentence in processed {
        if let Some(caps) = VOLATILE_END.captures(&sentence).unwrap_or(None) {
            let full = caps.get(0).unwrap();
            let m1 = caps.get(1).unwrap().as_str();
            let m2 = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let start = full.start();
            let end = full.end();
            sentence = format!("{} {}{}{}", &sentence[..start], m1, m2, &sentence[end..]);
        }
        // KT: `len(cur)+1+len(s) <= chunk_limit` (для целых эквивалентно `<`)
        if cur.chars().count() + sentence.chars().count() < chunk_limit {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(&sentence);
        } else {
            if !cur.is_empty() {
                chunked.push(cur.clone());
                cur.clear();
            }
            if sentence.chars().count() > chunk_limit {
                chunked.push(sentence);
            } else {
                cur.push_str(&sentence);
            }
        }
    }
    if !cur.is_empty() {
        chunked.push(cur);
    }
    chunked
}

// ============================================================================
// Kotlin-стадия K3: словарно-независимые чистки normalize() (KT:315-334, 344+)
// ============================================================================

static FOOTNOTE_BRACKET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s?[\[{]\s*\d{1,4}(?:\s*[,–-]\s*\d{1,4})*\s*[\]}]").expect("footnote bracket")
});
static FOOTNOTE_SUPERSCRIPT: LazyLock<FRegex> =
    LazyLock::new(|| FRegex::new(r"(?<![мМ])[⁰¹²³⁴⁵⁶⁷⁸⁹]+").expect("footnote superscript"));
static FOOTNOTE_ASTERISK: LazyLock<FRegex> = LazyLock::new(|| {
    // класс — [\p{L}.,!?»")] с ASCII-кавычкой (KT-оригинал)
    FRegex::new(r#"(?<=[\p{L}.,!?»")])\*{1,3}(?![\p{L}*])"#).expect("footnote asterisk")
});
static FOOTNOTE_GLUED: LazyLock<FRegex> = LazyLock::new(|| {
    // Дословный KT-оригинал `…(?=[\s.,;:!?…»”)\\]]|$)`: `\\]` — экранированный
    // бэкслеш, затем `]` ЗАКРЫВАЕТ класс, внешняя `]` — литеральная. Первая
    // альтернатива lookahead требует «символ класса + литеральная ]» и в
    // прозе не срабатывает; реально матчит только `$` — приклеенные цифры
    // снимаются ТОЛЬКО в конце строки (поведение проверено на JVM 21 с
    // точной строкой оригинала; кейс «приклеено1, дальше» не меняется).
    FRegex::new(r#"(?<=[а-яёА-ЯЁ]{2})(?<!(?:км|см|мм|дм|кв))\d{1,3}(?=[\s.,;:!?…»”)\\]]|$)"#)
        .expect("footnote glued")
});
static URL_OR_MAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:https?://|ftp://|www\.)[^\s<>«»"]+[^\s<>«»".,;:!?)\]]|\b[\w.+-]+@[\w-]+(?:\.[\w-]+)+\b"#)
        .expect("url or mail")
});
static INVISIBLE_CHARS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("[\u{00AD}\u{200B}\u{200C}\u{200D}\u{200E}\u{200F}\u{2060}\u{2061}\u{2062}\u{2063}\u{2064}\u{034F}\u{FEFF}]")
        .expect("invisible chars")
});
static ODD_HYPHEN: LazyLock<FRegex> = LazyLock::new(|| {
    FRegex::new(r"(?<=\p{L})[\u{2010}\u{2011}\u{2012}\u{2013}\u{2212}\u{FE63}\u{FF0D}](?=\p{L})")
        .expect("odd hyphen")
});

/// TextNormalizer.kt:330-334 `stripFootnoteMarks` (сноски/надстрочные/
/// приклеенные номера/URL и e-mail — не читаются вслух).
fn strip_footnote_marks(text: &str) -> String {
    let t = FOOTNOTE_BRACKET.replace_all(text, "").into_owned();
    let t = FOOTNOTE_SUPERSCRIPT.replace_all(&t, "").into_owned();
    let t = FOOTNOTE_ASTERISK.replace_all(&t, "").into_owned();
    let t = FOOTNOTE_GLUED.replace_all(&t, "").into_owned();
    URL_OR_MAIL.replace_all(&t, "").into_owned()
}

/// Переносимая часть `TextNormalizer.normalize` (KT:344+, порядок как в
/// оригинале): `applyPunctuationTweaks(stripFootnoteMarks(text))` —
/// при дефолтных PunctuationPrefs tweaks = no-op (не переносится), затем
/// удаление невидимых символов и нормализация типографских дефисов.
/// Словарные проходы (лексикон/числа/ударения) НЕ переносятся (SPEC.md §4.1).
fn normalize_lite(text: &str) -> String {
    let t = strip_footnote_marks(text);
    let t = INVISIBLE_CHARS.replace_all(&t, "").into_owned();
    ODD_HYPHEN.replace_all(&t, "-").into_owned()
}

// ============================================================================
// Kotlin-стадия K1: paragraphUnits без fastStart (KT:240-265 минус :250-257)
// ============================================================================

/// Сцена-разделитель (Supertonic…kt SCENE_BREAK; тер-вариант идентичен).
pub const SCENE_BREAK: &str = "⁂";

/// KT:261-265 `isSceneBreak` (== tera::is_scene_break для сцен).
fn is_scene_break(p: &str) -> bool {
    let marks = p.chars().filter(|c| !c.is_whitespace()).count();
    let ornament = p.chars().all(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '*' | '⁂'
                    | '#'
                    | '~'
                    | '—'
                    | '–'
                    | '-'
                    | '_'
                    | '='
                    | '•'
                    | '·'
                    | '◆'
                    | '◇'
                    | '❖'
                    | '§'
            )
    });
    ornament && (marks >= 3 || p.contains('⁂') || p.contains('❖'))
}

/// `paragraphUnits` (порт KT:240-278 ДОСЛОВНО, включая fastStart).
/// Quirk KT:247-249: если НЕ-сценовых юнитов < 2 — у последнего флаг
/// конца абзаца сбрасывается (одно-предложенческий вызов — не абзац).
///
/// fastStart (KT:267-278) переносится, вопреки SPEC.md §5 отклонению №1:
/// эталон 99 чанков СГЕНЕРИРОВАН с fastStart (первый юнит — УПАКОВАННАЯ
/// фраза вызова, у эталона 63 симв. > 55 — вырез даёт чанки 0/1 эталона;
/// обоснование спецификации «первое предложение 38 ≤ 55» измеряло не тот
/// объект). Реальное поведение TeraTTS 1.59 и нормативный эталон важнее
/// умозрительного «лишнего стыка»; решение зафиксировано в
/// docs/G7_SPEC_CORRECTIONS.md (0.8).
fn paragraph_units(raw_text: &str, call_chunk_limit: usize) -> Vec<(String, bool)> {
    let mut units: Vec<(String, bool)> = Vec::new();
    for p in raw_text
        .split('\n')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        if is_scene_break(p) {
            units.push((SCENE_BREAK.to_string(), false));
            continue;
        }
        let ss: Vec<String> = split_into_sentences(p, call_chunk_limit)
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        for (i, s) in ss.iter().enumerate() {
            units.push((s.clone(), i + 1 == ss.len()));
        }
    }
    if units.iter().filter(|(u, _)| u != SCENE_BREAK).count() < 2 && !units.is_empty() {
        let n = units.len();
        let last = units[n - 1].clone();
        units[n - 1] = (last.0, false);
    }
    // fast start (KT:250-257): разрез первого юнита > 55 симв. у ~40-го
    if let Some(first) = units.first().cloned() {
        if first.0 != SCENE_BREAK {
            let parts = fast_start(&[first.0]);
            if parts.len() == 2 {
                units[0] = (parts[1].clone(), first.1);
                units.insert(0, (parts[0].clone(), false));
            }
        }
    }
    units
}

/// KT:40 FAST_START_CHARS.
const FAST_START_CHARS: usize = 40;

/// KT:267-278 `fastStart`: первое предложение > FAST_START_CHARS+15 симв.
/// режется после последней `, ; : —` в окне 40 симв. (если позиция ≥ 15),
/// иначе после последнего пробела окна.
fn fast_start(sentences: &[String]) -> Vec<String> {
    let first = &sentences[0];
    if first.chars().count() <= FAST_START_CHARS + 15 {
        return sentences.to_vec();
    }
    let window: Vec<char> = first.chars().take(FAST_START_CHARS).collect();
    let mut cut: i64 = -1;
    for (i, c) in window.iter().enumerate() {
        if matches!(c, ',' | ';' | ':' | '—') {
            cut = i as i64;
        }
    }
    let cut = if cut >= 15 {
        (cut + 1) as usize
    } else {
        match window.iter().rposition(|c| *c == ' ') {
            Some(pos) => pos,
            None => return sentences.to_vec(),
        }
    };
    if cut < 15 {
        return sentences.to_vec();
    }
    let head: String = first.chars().take(cut).collect();
    let tail: String = first.chars().skip(cut).collect();
    let head = head.trim().to_string();
    let tail = tail.trim().to_string();
    if head.is_empty() || tail.is_empty() {
        return sentences.to_vec();
    }
    let mut out = vec![head, tail];
    out.extend_from_slice(&sentences[1..]);
    out
}

// ============================================================================
// Rust-стадия R6-фронтэнд: tera.rs (Pocket-релевантное подмножество)
// ============================================================================

/// Пауза смены aside-сегмента, с (tera.rs:91).
pub const ASIDE_PAUSE_S: f32 = 0.25;
pub const PAUSE_COMMA: f32 = 0.15;
pub const PAUSE_DASH: f32 = 0.25;
pub const PAUSE_SEMICOLON: f32 = 0.30;
pub const PAUSE_COLON: f32 = 0.35;
pub const PAUSE_SENTENCE: f32 = 0.50;
pub const PAUSE_ELLIPSIS: f32 = 0.65;
/// Сцена-разделитель: 1.3 с (tera.rs:105).
pub const PAUSE_SCENE: f32 = 1.3;
/// U+2029 PARAGRAPH SEPARATOR — маркер конца абзаца (Kotlin → движок).
pub const PARAGRAPH_MARK: char = '\u{2029}';

/// Скобочная ремарка короче 4 букв остаётся inline (tera.rs:175).
const ASIDE_MIN_LETTERS: usize = 4;

const RU_VOWELS: &str = "аеёиоуыэюяАЕЁИОУЫЭЮЯ";

fn is_cyr(c: char) -> bool {
    matches!(c, 'А'..='я' | 'Ё' | 'ё')
}

static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static DASH_SPACED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+[—–]\s+").unwrap());
static DASH_LEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|\n)\s*[—–]\s*").unwrap());
static COMMA_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r",(\s*,)+").unwrap());
static STOP_COMMA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([.!?;:])\s*,").unwrap());
static DIGIT_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)(?:([.,])(\d+))?").unwrap());
static LATIN_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[A-Za-z]+\b").unwrap());

/// «+» — знак ударения только перед русской гласной (tera.rs:199).
fn clean_plus(s: &str) -> String {
    let ch: Vec<char> = s.chars().collect();
    ch.iter()
        .enumerate()
        .filter(|&(i, &c)| c != '+' || ch.get(i + 1).is_some_and(|n| RU_VOWELS.contains(*n)))
        .map(|(_, &c)| c)
        .collect()
}

/// Все формы ударений → «+гласная» (tera.rs:221).
fn acute_to_plus(s: &str) -> String {
    let src: Vec<char> = s.chars().collect();
    let mut ch: Vec<char> = Vec::with_capacity(src.len() + 8);
    for (i, &c) in src.iter().enumerate() {
        let near_cyr = (i > 0 && is_cyr(src[i - 1])) || src.get(i + 1).is_some_and(|&n| is_cyr(n));
        let latin = match c {
            'á' => Some('а'),
            'Á' => Some('А'),
            'é' => Some('е'),
            'É' => Some('Е'),
            'ó' => Some('о'),
            'Ó' => Some('О'),
            'ý' => Some('у'),
            'Ý' => Some('У'),
            _ => None,
        };
        let cyr = match c {
            'ѐ' => Some('е'),
            'Ѐ' => Some('Е'),
            'ѝ' => Some('и'),
            'Ѝ' => Some('И'),
            _ => None,
        };
        match (latin, cyr) {
            (Some(b), _) if near_cyr => {
                ch.push(b);
                ch.push('\u{301}');
            }
            (_, Some(b)) => {
                ch.push(b);
                ch.push('\u{301}');
            }
            _ => ch.push(if c == '\u{300}' { '\u{301}' } else { c }),
        }
    }
    let mut out: Vec<char> = Vec::with_capacity(ch.len() + 8);
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        if c == '\u{301}' {
            i += 1;
            continue;
        }
        if c == '+' {
            let next_vowel = ch.get(i + 1).is_some_and(|n| RU_VOWELS.contains(*n));
            let prev = out.last().copied();
            if !next_vowel && prev.is_some_and(|p| RU_VOWELS.contains(p)) {
                let at = out.len() - 1;
                if at == 0 || out[at - 1] != '+' {
                    out.insert(at, '+');
                }
                i += 1;
                continue;
            }
        }
        if ch.get(i + 1) == Some(&'\u{301}') && RU_VOWELS.contains(c) && out.last() != Some(&'+') {
            out.push('+');
        }
        out.push(c);
        i += 1;
    }
    out.into_iter().collect()
}

/// Тире-диалоги и пробельные тире → паузы-запятые (tera.rs:274).
fn dashes_to_pauses(s: &str) -> String {
    let t = DASH_LEADING.replace_all(s, "$1");
    let t = DASH_SPACED.replace_all(&t, ", ");
    let t = t.replace(['—', '–'], "-");
    let t = COMMA_RUN.replace_all(&t, ",");
    STOP_COMMA.replace_all(&t, "$1").into_owned()
}

/// Пробел после , . ! ? ; : … (кроме знаковых рядов и десятичных) (tera.rs:305).
fn add_punct_spaces(s: &str) -> String {
    let ch: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 16);
    for i in 0..ch.len() {
        let c = ch[i];
        out.push(c);
        if !matches!(c, ',' | '.' | '!' | '?' | ';' | ':' | '…') {
            continue;
        }
        let Some(&next) = ch.get(i + 1) else { continue };
        if next.is_whitespace()
            || next == '<'
            || matches!(next, ',' | '.' | '!' | '?' | ';' | ':' | '…')
        {
            continue;
        }
        let prev_digit = i > 0 && ch[i - 1].is_numeric();
        if matches!(c, '.' | ',') && prev_digit && next.is_numeric() {
            continue;
        }
        out.push(' ');
    }
    out
}

/// «21год» → «21 год» (tera.rs:329).
fn add_number_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if let Some(p) = prev {
            if p.is_numeric() && is_letter_ru_en(c) {
                out.push(' ');
            }
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

fn is_letter_ru_en(c: char) -> bool {
    c.is_ascii_alphabetic() || matches!(c, 'А'..='я' | 'Ё' | 'ё')
}

// ---- FIXED_STRESS (tera.rs:404) ----------------------------------------------

const FIXED_STRESS: &[(&str, &str)] = &[
    ("один", "од+ин"),
    ("одна", "одн+а"),
    ("одно", "одн+о"),
    ("одни", "одн+и"),
    ("после", "п+осле"),
    ("лет", "л+ет"),
    ("кому", "ком+у"),
    ("какая", "как+ая"),
    ("времени", "вр+емени"),
    ("тому", "том+у"),
    ("части", "ч+асти"),
    ("нем", "н+ём"),
    ("написать", "напис+ать"),
    ("написал", "напис+ал"),
    ("летом", "л+етом"),
    ("небе", "н+ебе"),
    ("неба", "н+еба"),
    ("метод", "м+етод"),
    ("любая", "люб+ая"),
    ("отчего", "отчег+о"),
    ("берет", "бер+ёт"),
    ("голубой", "голуб+ой"),
    ("выходит", "вых+одит"),
    ("выходят", "вых+одят"),
    ("черных", "ч+ёрных"),
    ("долги", "долг+и"),
];

/// Пиннинг ударений слов FIXED_STRESS без имеющейся метки (tera.rs:423).
fn pin_fixed_stress(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        let plain: String = word.chars().filter(|&c| c != '+').collect();
        let lower: String = plain.chars().flat_map(char::to_lowercase).collect();
        let already_marked = word.contains('+');
        match FIXED_STRESS
            .iter()
            .find(|(w, _)| *w == lower && !already_marked)
        {
            Some((_, stressed)) => {
                let src: Vec<char> = plain.chars().collect();
                let mut i = 0;
                for c in stressed.chars() {
                    if c == '+' {
                        out.push('+');
                    } else {
                        let cased = match src.get(i) {
                            Some(s) if s.is_uppercase() => c.to_uppercase().collect::<String>(),
                            _ => c.to_string(),
                        };
                        out.push_str(&cased);
                        i += 1;
                    }
                }
            }
            None => out.push_str(word),
        }
        word.clear();
    };
    for c in text.chars() {
        if matches!(c, 'А'..='я' | 'Ё' | 'ё' | '+') {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// NFC + acute→plus + пиннинг (tera.rs:468).
fn pre_normalize(text: &str) -> String {
    let t: String = text.nfc().collect();
    pin_fixed_stress(&acute_to_plus(&t))
}

// ---- цифры словами (tera.rs:478-554) -------------------------------------------

const RU_ONES_M: [&str; 10] = [
    "",
    "один",
    "два",
    "три",
    "четыре",
    "пять",
    "шесть",
    "семь",
    "восемь",
    "девять",
];
const RU_ONES_F: [&str; 10] = [
    "",
    "одна",
    "две",
    "три",
    "четыре",
    "пять",
    "шесть",
    "семь",
    "восемь",
    "девять",
];
const RU_TEENS: [&str; 10] = [
    "десять",
    "одиннадцать",
    "двенадцать",
    "тринадцать",
    "четырнадцать",
    "пятнадцать",
    "шестнадцать",
    "семнадцать",
    "восемнадцать",
    "девятнадцать",
];
const RU_TENS: [&str; 10] = [
    "",
    "",
    "двадцать",
    "тридцать",
    "сорок",
    "пятьдесят",
    "шестьдесят",
    "семьдесят",
    "восемьдесят",
    "девяносто",
];
const RU_HUNDREDS: [&str; 10] = [
    "",
    "сто",
    "двести",
    "триста",
    "четыреста",
    "пятьсот",
    "шестьсот",
    "семьсот",
    "восемьсот",
    "девятьсот",
];
const RU_DIGITS: [&str; 10] = [
    "ноль",
    "один",
    "два",
    "три",
    "четыре",
    "пять",
    "шесть",
    "семь",
    "восемь",
    "девять",
];

fn ru_plural<'a>(n: u64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let (d, h) = (n % 10, n % 100);
    if (11..=14).contains(&h) {
        many
    } else if d == 1 {
        one
    } else if (2..=4).contains(&d) {
        few
    } else {
        many
    }
}

fn ru_triad(n: u64, feminine: bool, out: &mut Vec<&'static str>) {
    let (h, t, o) = (
        (n / 100) as usize,
        (n / 10 % 10) as usize,
        (n % 10) as usize,
    );
    if h > 0 {
        out.push(RU_HUNDREDS[h]);
    }
    if t == 1 {
        out.push(RU_TEENS[o]);
        return;
    }
    if t > 1 {
        out.push(RU_TENS[t]);
    }
    if o > 0 {
        out.push(if feminine { RU_ONES_F[o] } else { RU_ONES_M[o] });
    }
}

/// 0..=999 999 999 999 словами (мужской род) (tera.rs:506).
fn spell_ru(n: u64) -> String {
    if n == 0 {
        return "ноль".into();
    }
    let mut out: Vec<&'static str> = Vec::new();
    let groups = [
        (
            1_000_000_000u64,
            false,
            ("миллиард", "миллиарда", "миллиардов"),
        ),
        (1_000_000, false, ("миллион", "миллиона", "миллионов")),
        (1_000, true, ("тысяча", "тысячи", "тысяч")),
    ];
    let mut rest = n;
    for (unit, fem, (one, few, many)) in groups {
        let k = rest / unit;
        if k > 0 {
            ru_triad(k, fem, &mut out);
            out.push(ru_plural(k, one, few, many));
            rest %= unit;
        }
    }
    ru_triad(rest, false, &mut out);
    out.join(" ")
}

fn spell_run(run: &str) -> String {
    let digit_by_digit = run.len() > 12 || (run.len() > 1 && run.starts_with('0'));
    if digit_by_digit {
        return run
            .bytes()
            .map(|b| RU_DIGITS[(b - b'0') as usize])
            .collect::<Vec<_>>()
            .join(" ");
    }
    spell_ru(run.parse().unwrap_or(0))
}

/// «3.14» → «три точка четырнадцать» (tera.rs:541).
fn spell_leftover_digits(s: &str) -> String {
    DIGIT_RUN
        .replace_all(s, |c: &regex::Captures| {
            let mut w = format!(" {} ", spell_run(&c[1]));
            if let (Some(sep), Some(frac)) = (c.get(2), c.get(3)) {
                let word = match sep.as_str() {
                    "." => "точка",
                    _ => "запятая",
                };
                w = format!(
                    " {} {} {} ",
                    spell_run(&c[1]),
                    word,
                    spell_run(frac.as_str())
                );
            }
            w
        })
        .into_owned()
}

// ---- латиница КАПС в русском контексте (tera.rs:561-651) ------------------------

const LETTER_NAMES: [&str; 26] = [
    "эй",
    "би",
    "си",
    "ди",
    "и",
    "эф",
    "джи",
    "эйч",
    "ай",
    "джей",
    "кей",
    "эл",
    "эм",
    "эн",
    "оу",
    "пи",
    "кью",
    "ар",
    "эс",
    "ти",
    "ю",
    "ви",
    "дабл-ю",
    "экс",
    "уай",
    "зед",
];

const WORD_ACRONYMS: &[(&str, &str)] = &[
    ("OK", "ок+ей"),
    ("NASA", "н+аса"),
    ("NATO", "н+ато"),
    ("UNESCO", "юн+еско"),
    ("FIFA", "ф+ифа"),
    ("UEFA", "у+ефа"),
    ("IKEA", "ик+еа"),
    ("LEGO", "л+его"),
    ("BMW", "бэ-эм-в+э"),
    ("WiFi", "вайф+ай"),
    ("WIFI", "вайф+ай"),
    ("LED", "л+ед"),
    ("RAM", "р+ам"),
    ("ROM", "р+ом"),
    ("SIM", "с+им"),
    ("ISO", "+исо"),
    ("USA", "ю-эс-+эй"),
    ("PIN", "п+ин"),
    ("CAPS", "к+апс"),
];

const NOT_ROMAN: &[&str] = &[
    "CD", "DC", "MC", "MD", "CV", "CL", "DM", "LCD", "MIC", "MIX", "DIV", "CIV", "LI", "MI", "DI",
    "MM", "CC",
];

fn roman_value(s: &str) -> Option<u64> {
    let val = |c: char| match c {
        'I' => 1,
        'V' => 5,
        'X' => 10,
        'L' => 50,
        'C' => 100,
        'D' => 500,
        'M' => 1000,
        _ => 0,
    };
    if s.is_empty() || s.chars().any(|c| val(c) == 0) {
        return None;
    }
    let (mut total, mut prev) = (0i64, 0i64);
    for c in s.chars().rev() {
        let v = val(c);
        if v < prev {
            total -= v
        } else {
            total += v;
            prev = v;
        }
    }
    let mut n = total;
    let mut canon = String::new();
    for (v, r) in [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ] {
        while n >= v {
            canon.push_str(r);
            n -= v;
        }
    }
    (total > 0 && total < 4000 && canon == s).then_some(total as u64)
}

fn spell_acronym(token: &str) -> String {
    let names: Vec<&str> = token
        .bytes()
        .map(|b| LETTER_NAMES[(b.to_ascii_uppercase() - b'A') as usize])
        .collect();
    let mut out = names.join("-");
    if let Some(pos) = out
        .char_indices()
        .rev()
        .find(|(_, c)| RU_VOWELS.contains(*c))
        .map(|(i, _)| i)
    {
        out.insert(pos, '+');
    }
    out
}

/// Ближайшая буква вокруг [start, end) — кириллица ли (tera.rs:607).
fn cyrillic_neighbour(s: &str, start: usize, end: usize) -> bool {
    let near = |it: &mut dyn Iterator<Item = char>| -> Option<bool> {
        for c in it.take(4) {
            if c.is_alphabetic() {
                return Some(matches!(c, 'А'..='я' | 'Ё' | 'ё'));
            }
        }
        None
    };
    let left = near(&mut s[..start].chars().rev());
    let right = near(&mut s[end..].chars());
    left == Some(true) || right == Some(true) || (left.is_none() && right.is_none())
}

fn latin_caps_to_russian(s: &str) -> String {
    LATIN_WORD
        .replace_all(s, |c: &regex::Captures| {
            let m = c.get(0).unwrap();
            let tok = m.as_str();
            if !cyrillic_neighbour(s, m.start(), m.end()) {
                return tok.to_string();
            }
            if let Some((_, w)) = WORD_ACRONYMS.iter().find(|(a, _)| *a == tok) {
                return w.to_string();
            }
            if tok.len() >= 3
                && tok.len() <= 7
                && tok.ends_with('s')
                && tok[..tok.len() - 1].bytes().all(|b| b.is_ascii_uppercase())
            {
                let mut sp = spell_acronym(&tok[..tok.len() - 1]);
                sp.push('с');
                return sp;
            }
            let all_caps = tok.bytes().all(|b| b.is_ascii_uppercase());
            if !all_caps || tok.len() > 6 {
                return tok.to_string();
            }
            if !NOT_ROMAN.contains(&tok) && (tok.len() >= 2 || matches!(tok, "I" | "V" | "X")) {
                if let Some(n) = roman_value(tok) {
                    return n.to_string();
                }
            }
            if tok.len() >= 2 {
                spell_acronym(tok)
            } else {
                tok.to_string()
            }
        })
        .into_owned()
}

// ---- знаки №/§/℃ (tera.rs:818) ---------------------------------------------------

fn spell_signs(s: &str) -> String {
    s.replace("№№", " номера ")
        .replace('№', " номер ")
        .replace('℃', " градусов Цельсия ")
        .replace('§', " параграф ")
}

/// Общий фронтэнд (common_front, tera.rs:841; lang всегда «ru» в режиме).
fn common_front(text: &str) -> String {
    let t: String = text.nfc().collect();
    let t = t.replace(['<', '>'], " ");
    let t = spell_signs(&t);
    let t = acute_to_plus(&t);
    let t = clean_plus(&t);
    let t = dashes_to_pauses(&t);
    let t = add_punct_spaces(&t);
    let t = add_number_spaces(&t);
    let t = latin_caps_to_russian(&t);
    spell_leftover_digits(&t)
}

/// «+гласная» → «гласная»+U+0301 (ё — без акута) (tera.rs:876).
fn plus_to_acute(s: &str, keep: bool) -> String {
    let ch: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < ch.len() {
        if ch[i] == '+' && i + 1 < ch.len() && RU_VOWELS.contains(ch[i + 1]) {
            out.push(ch[i + 1]);
            if keep && !matches!(ch[i + 1], 'ё' | 'Ё') {
                out.push('\u{301}');
            }
            i += 2;
            continue;
        }
        if ch[i] != '+' {
            out.push(ch[i]);
        }
        i += 1;
    }
    out
}

/// Фронтэнд модельного текста Pocket (tera.rs:858). None — нечего
/// произносить (нет кириллицы): Android молча пропускает фразу.
pub fn prepare_pocket(text: &str) -> Option<String> {
    let t = common_front(text);
    let t = plus_to_acute(&t, true);
    let t: String = t.chars().filter(|c| !c.is_ascii_alphabetic()).collect();
    let t = COMMA_RUN.replace_all(&t, ",");
    let t = WS.replace_all(&t, " ").trim().to_string();
    let t = t
        .trim_start_matches(|c: char| ",;:".contains(c) || c.is_whitespace())
        .to_string();
    if !t.chars().any(|c| matches!(c, 'А'..='я' | 'Ё' | 'ё')) {
        return None;
    }
    Some(t)
}

// ---- pause_split (tera.rs:695) ---------------------------------------------------

/// Аббревиатуры, после которых «.» не конец предложения (tera.rs:656).
const ABBREV: &[&str] = &[
    "г", "гг", "т", "д", "др", "пр", "им", "ул", "стр", "рис", "см", "ср", "тыс", "млн", "млрд",
    "руб", "коп", "св", "ст", "проф", "акад", "доц", "ок", "mr", "mrs", "ms", "dr", "vs", "etc",
    "e", "i",
];

/// Пауза знака в конце куска (tera.rs:660).
pub fn trailing_pause(s: &str) -> f32 {
    let t = s.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '»' | '"' | '“' | '”' | '\'' | ')')
    });
    if t.ends_with("...") || t.ends_with('…') {
        return PAUSE_ELLIPSIS;
    }
    match t.chars().last() {
        Some('.' | '!' | '?') => PAUSE_SENTENCE,
        Some(':') => PAUSE_COLON,
        Some(';') => PAUSE_SEMICOLON,
        Some('—' | '–') => PAUSE_DASH,
        Some(',') => PAUSE_COMMA,
        _ => 0.0,
    }
}

/// Слово перед позицией `dot` — инициал или аббревиатура (tera.rs:676).
fn abbrev_before(ch: &[char], dot: usize) -> bool {
    let mut b = dot;
    while b > 0 && ch[b - 1].is_alphabetic() {
        b -= 1;
    }
    let word: String = ch[b..dot].iter().collect();
    if word.is_empty() {
        return false;
    }
    if word.chars().count() == 1 && word.chars().all(char::is_uppercase) {
        return true;
    }
    let lw: String = word.chars().flat_map(char::to_lowercase).collect();
    ABBREV.contains(&lw.as_str())
}

/// Разрез по знакам с паузами (tera.rs:695). Boundary для `.!?…`: после
/// знака (и жадного хвоста `.!?…»"”`) — пробел и ЗАГЛАВНАЯ/цифра/тире/
/// `« " “`; иначе склейка (эталонное поведение, не «чинить»).
pub fn pause_split(text: &str) -> Vec<(String, f32)> {
    let ch: Vec<char> = text.chars().collect();
    let mut out: Vec<(String, f32)> = Vec::new();
    let push = |piece: String, pause: f32, out: &mut Vec<(String, f32)>| {
        let t = piece.trim().to_string();
        if t.chars().any(|c| c.is_alphanumeric()) {
            out.push((t, pause));
        } else if let Some(last) = out.last_mut() {
            last.1 = last.1.max(pause);
        }
    };
    let mut start = 0;
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        let ws_after = ch.get(i + 1).is_some_and(|n| n.is_whitespace());
        match c {
            ':' | ';' if ws_after => {
                let piece: String = ch[start..=i].iter().collect();
                let p = trailing_pause(&piece);
                push(piece, p, &mut out);
                start = i + 1;
            }
            '—' | '–' | '-' if i > 0 && ch[i - 1].is_whitespace() && ws_after => {
                let piece: String = ch[start..i].iter().collect();
                let p = trailing_pause(&piece).max(PAUSE_DASH);
                push(piece, p, &mut out);
                start = i + 1;
            }
            '.' | '!' | '?' | '…' => {
                let mut j = i;
                while j + 1 < ch.len()
                    && matches!(ch[j + 1], '.' | '!' | '?' | '…' | '»' | '"' | '”')
                {
                    j += 1;
                }
                let mut k = j + 1;
                while k < ch.len() && ch[k].is_whitespace() {
                    k += 1;
                }
                let boundary = k > j + 1
                    && k < ch.len()
                    && (ch[k].is_uppercase()
                        || ch[k].is_numeric()
                        || matches!(ch[k], '—' | '–' | '-' | '«' | '"' | '“'));
                let is_abbrev = c == '.' && j == i && abbrev_before(&ch, i);
                if boundary && !is_abbrev {
                    let piece: String = ch[start..=j].iter().collect();
                    let p = trailing_pause(&piece);
                    push(piece, p, &mut out);
                    start = j + 1;
                }
                i = j;
            }
            _ => {}
        }
        i += 1;
    }
    let tail: String = ch[start..].iter().collect();
    let p = trailing_pause(&tail);
    push(tail, p, &mut out);
    out
}

/// Скобочные ремарки (tera.rs:758): «(…)» ≥4 букв — отдельный aside-сегмент;
/// короче — inline; скобки всегда удаляются; вложенные — внутри внешней;
/// непарная «(» — до конца.
pub fn split_asides(text: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut normal = String::new();
    let mut aside = String::new();
    let mut depth = 0usize;
    let push = |out: &mut Vec<(String, bool)>, s: &str, is_aside: bool| {
        let t = s.trim();
        if t.is_empty() {
            return;
        }
        match out.last_mut() {
            Some(last) if last.1 == is_aside => {
                last.0.push(' ');
                last.0.push_str(t);
            }
            _ => out.push((t.to_string(), is_aside)),
        }
    };
    let close_aside = |out: &mut Vec<(String, bool)>, normal: &mut String, aside: &mut String| {
        let letters = aside.chars().filter(|c| c.is_alphabetic()).count();
        if letters >= ASIDE_MIN_LETTERS {
            push(out, normal, false);
            normal.clear();
            push(out, aside, true);
        } else {
            normal.push_str(aside);
        }
        aside.clear();
    };
    for c in text.chars() {
        match c {
            '(' => {
                if depth > 0 {
                    aside.push(' ');
                }
                depth += 1;
            }
            ')' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    close_aside(&mut out, &mut normal, &mut aside);
                } else {
                    aside.push(' ');
                }
            }
            ')' => {}
            _ if depth > 0 => aside.push(c),
            _ => normal.push(c),
        }
    }
    if depth > 0 {
        close_aside(&mut out, &mut normal, &mut aside);
    }
    push(&mut out, &normal, false);
    out
}

// ============================================================================
// Rust-стадия R4: helper.rs:371-528 (chunk_text, split_sentences)
// ============================================================================

/// helper.rs:377 — английские аббревиатуры для split_sentences.
const HELPER_ABBREVIATIONS: &[&str] = &[
    "Dr.", "Mr.", "Mrs.", "Ms.", "Prof.", "Sr.", "Jr.", "St.", "Ave.", "Rd.", "Blvd.", "Dept.",
    "Inc.", "Ltd.", "Co.", "Corp.", "etc.", "vs.", "i.e.", "e.g.", "Ph.D.",
];

/// Разбиение фразы на чанки ≤ `max_len` СИМВОЛОВ (helper.rs:389; фикс
/// byte-length и reordering уже в ec0537f). Неделимое слово длиннее
/// лимита — чанк-монолит СВЕРХ лимита (эталонная семантика, обрезки нет).
pub fn chunk_text(text: &str, max_len: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return vec![String::new()];
    }
    let clen = |s: &str| s.chars().count();

    let para_re = Regex::new(r"\n\s*\n").unwrap();
    let mut chunks: Vec<String> = Vec::new();

    for para in para_re.split(text) {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if clen(para) <= max_len {
            chunks.push(para.to_string());
            continue;
        }

        let mut current = String::new();
        let flush = |current: &mut String, chunks: &mut Vec<String>| {
            if !current.trim().is_empty() {
                chunks.push(current.trim().to_string());
            }
            current.clear();
        };

        for sentence in split_sentences(para) {
            let sentence = sentence.trim();
            if sentence.is_empty() {
                continue;
            }
            if clen(sentence) > max_len {
                flush(&mut current, &mut chunks);
                let parts: Vec<&str> = sentence
                    .split(',')
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty())
                    .collect();
                let last_part = parts.len().saturating_sub(1);
                for (k, part) in parts.iter().enumerate() {
                    // запятая ВОЗВРАЩАЕТСЯ в конец всех частей, кроме последней
                    let piece = if k < last_part {
                        format!("{},", part)
                    } else {
                        part.to_string()
                    };
                    if clen(&piece) > max_len {
                        flush(&mut current, &mut chunks);
                        let mut words_chunk = String::new();
                        for word in piece.split_whitespace() {
                            if !words_chunk.is_empty()
                                && clen(&words_chunk) + 1 + clen(word) > max_len
                            {
                                chunks.push(words_chunk.clone());
                                words_chunk.clear();
                            }
                            if !words_chunk.is_empty() {
                                words_chunk.push(' ');
                            }
                            words_chunk.push_str(word);
                        }
                        if !words_chunk.is_empty() {
                            chunks.push(words_chunk);
                        }
                        continue;
                    }
                    if !current.is_empty() && clen(&current) + 1 + clen(&piece) > max_len {
                        flush(&mut current, &mut chunks);
                    }
                    if !current.is_empty() {
                        current.push(' ');
                    }
                    current.push_str(&piece);
                }
                flush(&mut current, &mut chunks);
                continue;
            }
            if !current.is_empty() && clen(&current) + 1 + clen(sentence) > max_len {
                flush(&mut current, &mut chunks);
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(sentence);
        }
        flush(&mut current, &mut chunks);
    }

    if chunks.is_empty() {
        vec![String::new()]
    } else {
        chunks
    }
}

/// helper.rs:480 — разрез `([.!?][кавычка/скобка]?)\s+` без
/// boundary-проверки (регистр следующего не важен); англ. аббревиатуры
/// защищены. Предложение включает матч (знак и пробелы).
fn split_sentences(text: &str) -> Vec<String> {
    let re = Regex::new(r"([.!?]['\u{2019}\u{201D}\u{0022}\)\}\]]?)\s+").unwrap();
    let matches: Vec<_> = re.find_iter(text).collect();
    if matches.is_empty() {
        return vec![text.to_string()];
    }

    let mut sentences = Vec::new();
    let mut last_end = 0;

    for m in matches {
        let before_punc = &text[last_end..m.start()];
        let mut is_abbrev = false;
        for abbrev in HELPER_ABBREVIATIONS {
            let combined = format!("{}{}", before_punc.trim(), &text[m.start()..m.start() + 1]);
            if combined.ends_with(abbrev) {
                is_abbrev = true;
                break;
            }
        }
        if !is_abbrev {
            sentences.push(text[last_end..m.end()].to_string());
            last_end = m.end();
        }
    }
    if last_end < text.len() {
        sentences.push(text[last_end..].to_string());
    }
    if sentences.is_empty() {
        vec![text.to_string()]
    } else {
        sentences
    }
}

// ============================================================================
// Стадия R5: план чанков и пауз вызова (helper.rs:826-952, Pocket-ветка)
// ============================================================================

/// Один чанк вызова до построения плана пауз.
struct CallChunk {
    text: String,
    is_aside: bool,
    /// Пауза фразы — только последнему чанку фразы (иначе 0).
    owed_pause_s: f32,
}

/// План ОДНОГО вызова (фразы-юнита): чанк-кандидаты до модельного текста
/// и пауз. Сцена-разделитель обрабатывается уровнем файла; хвост вызова —
/// уровнем файла (нужно знать, последний ли чанк В ФАЙЛЕ). Маркер U+2029
/// уже заменён пробелом уровнем выше (K4→R1: `contains` → флаг `para_end`).
fn call_plan(text: &str, opts: &AndroidChunkOptions) -> Vec<CallChunk> {
    let mut chunks: Vec<CallChunk> = Vec::new();
    for (segment, aside) in split_asides(text) {
        for (phrase, pause) in pause_split(&segment) {
            let parts = chunk_text(&phrase, opts.max_chunk_chars);
            let n = parts.len();
            for (k, c) in parts.into_iter().enumerate() {
                chunks.push(CallChunk {
                    text: c,
                    is_aside: aside,
                    owed_pause_s: if k + 1 == n { pause } else { 0.0 },
                });
            }
        }
    }
    chunks
}

/// Главный вход: полный план чанков файла (TTS-путь, fastStart/U+2063 off).
///
/// Возвращает плоский список: чанки вызовов с паузами (стык → хвост
/// вызова → хвост файла) и юниты-сцены (пауза PAUSE_SCENE×scale, без
/// модельного текста). Глобально последний элемент файла — `pause_s=None`.
pub fn android_chunk_pipeline(text: &str, opts: &AndroidChunkOptions) -> Vec<AndroidChunk> {
    let mut out: Vec<AndroidChunk> = Vec::new();
    let scale = opts.pause_scale;
    let units = paragraph_units(text, opts.call_chunk_limit);
    for (unit_raw, para_end) in units {
        if is_scene_break(&unit_raw) {
            out.push(AndroidChunk {
                text: SCENE_BREAK.to_string(),
                model_text: None,
                pause_s: Some(PAUSE_SCENE * scale),
                is_aside: false,
                paragraph_end: false,
                boundary_kind: AndroidBoundary::SceneBreak,
            });
            continue;
        }
        // K4→R1: маркер конца абзаца → флаг вызова; в тексте — пробел
        let unit = unit_raw.replace(PARAGRAPH_MARK, " ");
        // K3: словарно-независимая normalize() на фразе-юните
        let unit = normalize_lite(&unit);
        let chunks = call_plan(&unit, opts);
        let n = chunks.len();
        for i in 0..n {
            let c = &chunks[i];
            let model_text = prepare_pocket(&pre_normalize(&c.text));
            let (pause_s, boundary_kind) = if i + 1 < n {
                // стык ВНУТРИ вызова (helper.rs:898-908): пауза фразы /
                // aside-переход / пол 0.02, всё ×scale (кроме пола)
                let mut gap = c.owed_pause_s * scale;
                if chunks[i + 1].is_aside != c.is_aside {
                    gap = gap.max(ASIDE_PAUSE_S * scale);
                }
                gap = gap.max(SILENCE_JOINT_S);
                (Some(gap), AndroidBoundary::IntraPhrase)
            } else if para_end {
                // хвост вызова конца абзаца: max(знак, 0.9)×scale, БЕЗ пола
                let tail = c.owed_pause_s.max(opts.paragraph_pause_s) * scale;
                (Some(tail), AndroidBoundary::CallTail { paragraph: true })
            } else {
                let tail = c.owed_pause_s * scale;
                (Some(tail), AndroidBoundary::CallTail { paragraph: false })
            };
            out.push(AndroidChunk {
                text: c.text.clone(),
                model_text,
                pause_s,
                is_aside: c.is_aside,
                paragraph_end: para_end,
                boundary_kind,
            });
        }
    }
    // Глобально последний чанк файла: пауза не вставляется (Windows-сборка;
    // Android вставил бы хвост вызова — в файле это хвост файла)
    if let Some(last) = out.last_mut() {
        last.pause_s = None;
        last.boundary_kind = AndroidBoundary::EndOfFile;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> AndroidChunkOptions {
        AndroidChunkOptions::default()
    }

    // ---- 1. split_into_sentences (Kotlin, лимит 300) --------------------------

    #[test]
    fn kt_split_packs_short_sentences_into_one_call_phrase() {
        // разрез есть, но склейка ≤300 возвращает одну фразу вызова
        let out = split_into_sentences("Раз. Два. Три. Вот.", 300);
        assert_eq!(out, vec!["Раз. Два. Три. Вот."]);
    }

    #[test]
    fn kt_split_packs_by_limit() {
        let s1 = format!("{}.", "а".repeat(199));
        let s2 = format!("{}.", "б".repeat(199));
        let out = split_into_sentences(&format!("{s1} {s2}"), 300);
        assert_eq!(out, vec![s1, s2]);
    }

    #[test]
    fn kt_comma_split_removes_closing_comma() {
        let p = |c: char| format!("{c}{}", "л".repeat(98)); // 99 симв.
        let text = format!("{}, {}, {}, {}", p('а'), p('б'), p('в'), p('г'));
        // 4 куска по 99 симв. через «, » — суммарно 402 > 300
        let out = split_into_sentences(&text, 300);
        assert_eq!(
            out,
            vec![
                format!("{}, {}", p('а'), p('б')),
                format!("{}, {}", p('в'), p('г'))
            ]
        );
    }

    #[test]
    fn kt_abbreviations_protected() {
        let out = split_into_sentences("Mr. Smith came. Он ушёл.", 300);
        assert_eq!(out, vec!["Mr. Smith came. Он ушёл."]);
        let out = split_into_sentences("Он жил в U.S. Army долго.", 300);
        assert_eq!(out, vec!["Он жил в U.S. Army долго."]);
    }

    #[test]
    fn kt_volatile_space_before_final_mark() {
        let out = split_into_sentences("Он сказ+ал? Да.", 300);
        assert_eq!(out, vec!["Он сказ+ал ? Да."]);
        // «»» НЕ входит в класс кавычек разреза: после «?»» разреза НЕТ —
        // вся строка одно предложение, volatile к «?» не применяется (не конец)
        let out = split_into_sentences("Он сказ+ал?» Да.", 300);
        assert_eq!(out, vec!["Он сказ+ал?» Да."]);
        // «”» (U+201D) в классе есть: разрез после «?”», volatile вставляет
        // пробел перед «?», закрывающая кавычка сохраняется
        let out = split_into_sentences("Он сказ+ал?” Да.", 300);
        assert_eq!(out, vec!["Он сказ+ал ?” Да."]);
    }

    #[test]
    fn kt_semicolon_split_packs_back() {
        // разрез по «;» есть, но ≤300 склеены в одну фразу вызова;
        // volatile-пробел вставляется ПЕРЕД конечной «;» первой фразы
        let out = split_into_sentences("Первое; второе.", 300);
        assert_eq!(out, vec!["Первое ; второе."]);
    }

    // ---- 2. paragraph_units ----------------------------------------------

    #[test]
    fn fast_start_cuts_first_packed_unit_over_55() {
        // первый юнит — упакованная фраза 59 симв. > 55: разрез ПОСЛЕ запятой
        // в окне 40 симв. (позиция ≥ 15); голова — отдельный вызов без флага
        // абзаца, хвост наследует флаг (KT:250-257)
        let out = android_chunk_pipeline(
            "Неуж+ели пр+авда, не м+ожет быть? поинтересов+ался Г+ари.",
            &opts(),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "Неуж+ели пр+авда,");
        assert_eq!(out[0].pause_s, Some(PAUSE_COMMA)); // хвост вызова: знак «,»
                                                       // volatile-пробел перед «?» вставлен ДО упаковки (Kotlin)
        assert_eq!(out[1].text, "не м+ожет быть ? поинтересов+ался Г+ари.");
        assert_eq!(out[1].pause_s, None);
        // короткий первый юнит (≤55) не режется
        let out2 = android_chunk_pipeline("Неуж+ели пр+авда? Поинтересов+ался Г+ари.", &opts());
        assert_eq!(out2.len(), 2);
        assert!(out2[0].text.starts_with("Неуж+ели"));
    }

    #[test]
    fn paragraph_units_flags_last_sentence_of_paragraph() {
        let u = paragraph_units("Первое. Второе.\n\nТретье.", 300);
        assert_eq!(
            u,
            vec![
                ("Первое. Второе.".to_string(), true),
                ("Третье.".to_string(), true),
            ]
        );
    }

    #[test]
    fn paragraph_units_quirk_single_unit_resets_flag() {
        let u = paragraph_units("Одно предложение на весь файл.", 300);
        assert_eq!(
            u,
            vec![("Одно предложение на весь файл.".to_string(), false)]
        );
        // одно предложение на абзац, но юнитов ≥ 2 — флаги остаются
        let u = paragraph_units("Один.\n\nДва.", 300);
        assert_eq!(
            u,
            vec![("Один.".to_string(), true), ("Два.".to_string(), true)]
        );
    }

    #[test]
    fn paragraph_units_scene_and_crlf_and_empty_lines() {
        let u = paragraph_units("А.\n\n* * *\n\nБ.", 300);
        assert_eq!(
            u,
            vec![
                ("А.".into(), true),
                (SCENE_BREAK.into(), false),
                ("Б.".into(), true)
            ]
        );
        // CRLF: trim съедает \r; пустые/пробельные абзацы пропускаются
        let u = paragraph_units("Первый.\r\n\r\n\r\n   \r\nВторой.\r\n", 300);
        assert_eq!(u, vec![("Первый.".into(), true), ("Второй.".into(), true)]);
    }

    // ---- 3. pause_split ---------------------------------------------------------

    #[test]
    fn pause_split_boundary_matrix() {
        // ЗАГЛАВНАЯ — разрез
        let p = pause_split("П+ервое. Втор+ое.");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], ("П+ервое.".into(), PAUSE_SENTENCE));
        // строчная — склейка (эталон!)
        assert_eq!(pause_split("П+ервое. втор+ое.").len(), 1);
        // «+» — склейка (ударение не проходит boundary)
        assert_eq!(pause_split("Завед+ение. +если бы").len(), 1);
        // цифра / тире / « — разрез
        assert_eq!(pause_split("Аа. 1бб").len(), 2);
        assert_eq!(pause_split("Аа. — Бб").len(), 2);
        assert_eq!(pause_split("Аа. «Бб»").len(), 2);
        // конец строки — не разрез
        assert_eq!(pause_split("Просто текст.").len(), 1);
    }

    #[test]
    fn pause_split_greedy_tail_and_abbrev() {
        let p = pause_split("Что?! Пр+авда!.. Да...");
        let texts: Vec<&str> = p.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(texts, vec!["Что?!", "Пр+авда!..", "Да..."]);
        assert_eq!(p[0].1, PAUSE_SENTENCE);
        assert_eq!(p[1].1, PAUSE_SENTENCE); // срез кавычек → последний знак «.»
        assert_eq!(p[2].1, PAUSE_ELLIPSIS);
        // русская аббревиатура и инициалы не режут
        assert_eq!(pause_split("Он жив+ет в г. Москв+а.").len(), 1);
        assert_eq!(pause_split("А. С. П+ушкин род+ился.").len(), 1);
    }

    #[test]
    fn pause_split_colon_semicolon_dash() {
        let p = pause_split("П+ервое; втор+ое. Подсказ+ка: вот так.");
        let texts: Vec<&str> = p.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(
            texts,
            vec!["П+ервое;", "втор+ое.", "Подсказ+ка:", "вот так."]
        );
        assert_eq!(p[0].1, PAUSE_SEMICOLON);
        assert_eq!(p[1].1, PAUSE_SENTENCE);
        assert_eq!(p[2].1, PAUSE_COLON);
        // тире УДАЛЯЕТСЯ; пауза = max(знак, 0.25)
        let p = pause_split("Он+а сказ+ала — и замолч+ала.");
        assert_eq!(p[0], ("Он+а сказ+ала".into(), PAUSE_DASH));
        assert_eq!(p[1], ("и замолч+ала.".into(), PAUSE_SENTENCE));
        // запятые НЕ режут
        assert_eq!(pause_split("Снач+ала, пот+ом, зат+ем, вс+ё.").len(), 1);
    }

    #[test]
    fn pause_split_hanging_punctuation_gives_pause_to_previous() {
        let p = pause_split("Слов+о. »...");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].1, PAUSE_ELLIPSIS);
    }

    // ---- 4. chunk_text ----------------------------------------------------------

    #[test]
    fn chunk_text_short_is_single_and_empty() {
        let s = format!("{} {}", "слово", "а".repeat(10));
        assert_eq!(chunk_text(&s, 150).len(), 1);
        assert_eq!(chunk_text("", 150), vec![String::new()]);
    }

    #[test]
    fn chunk_text_comma_word_and_monolith_paths() {
        // запятая ВОЗВРАЩАЕТСЯ в конец частей (кроме последней)
        let long = format!("{}, {}, {}", "а".repeat(80), "б".repeat(80), "в".repeat(80));
        let out = chunk_text(&long, 150);
        assert!(out.len() >= 2);
        assert!(out[0].ends_with(','));
        assert!(!out.last().unwrap().ends_with(','));
        // пословная упаковка без запятых — каждый чанк ≤150
        let noc = format!("{} {} {}", "а".repeat(80), "б".repeat(80), "в".repeat(80));
        let out = chunk_text(&noc, 150);
        assert!(out.len() >= 2);
        assert!(out.iter().all(|c| c.chars().count() <= 150));
        // монолит: слово длиннее лимита — чанк СВЕРХ лимита, обрезки нет
        let mono = "длинноеслово".repeat(30);
        let out = chunk_text(&mono, 150);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chars().count(), mono.chars().count());
    }

    #[test]
    fn chunk_text_intra_phrase_lowercase_sentence_cut() {
        // единственная стадия, где режется «точка + строчная»
        let a = "П+ервое предлож+ение о́чень дл+инное и содержи́т мн+ого слов для превыши́ния лим+ита ча́нка в сто пя́тдесят symb.";
        let b = "пот+ом втор+ое предлож+ение идёт со строчн+ой буквы и glue.";
        let t = format!("{a} {b}"); // > 150, склеены pause_split'ом
        let out = chunk_text(&t, 150);
        assert_eq!(out.len(), 2);
    }

    // ---- 5. split_asides ----------------------------------------------------------

    #[test]
    fn asides_rules() {
        let a = split_asides("В сфере изучения мозга доктора Наумова (как будто фамилия должна была что-то сказать) работали многие.");
        assert_eq!(
            a,
            vec![
                ("В сфере изучения мозга доктора Наумова".to_string(), false),
                (
                    "как будто фамилия должна была что-то сказать".to_string(),
                    true
                ),
                ("работали многие.".to_string(), false),
            ]
        );
        let b = split_asides("Пункт (1) и знак (с) остаются.");
        assert_eq!(b, vec![("Пункт 1 и знак с остаются.".to_string(), false)]);
        let c = split_asides("Текст (вставка (вложенная) тут) конец");
        assert_eq!(c[1], ("вставка  вложенная  тут".to_string(), true));
        let d = split_asides("Обрыв (без закрывающей скобки");
        assert_eq!(d.len(), 2);
        assert!(d[1].1);
        assert_eq!(
            split_asides("(целиком в скобках)"),
            vec![("целиком в скобках".to_string(), true)]
        );
    }

    // ---- 6. prepare_pocket / фронтэнд ----------------------------------------------

    #[test]
    fn front_end_stress_and_dashes() {
        assert_eq!(
            prepare_pocket("Стр+елки — 12 часов!").unwrap(),
            "Стре\u{301}лки, двенадцать часов!"
        );
        assert_eq!(prepare_pocket("Замо́к").unwrap(), "Замо\u{301}к");
        assert!(prepare_pocket("Hello world").is_none());
        // латиница удалена, дефис от тире остаётся
        assert_eq!(prepare_pocket("слово x-ray").unwrap(), "слово -");
        assert_eq!(prepare_pocket("Слов+о").unwrap(), "Слово\u{301}");
    }

    #[test]
    fn front_end_fixed_stress_and_caps() {
        assert_eq!(
            pre_normalize("Глава один. Один из нас, ОДИН!"),
            "Глава од+ин. Од+ин из нас, ОД+ИН!"
        );
        assert_eq!(pre_normalize("Глава +один"), "Глава +один"); // метка уже есть
        let nasa = prepare_pocket("Он откр+ыл NASA и Google").unwrap();
        assert!(nasa.contains("на\u{301}са"), "{nasa}"); // NASA → «н+аса» → на́са
        assert!(!nasa.contains("Google"), "{nasa}");
    }

    #[test]
    fn front_end_punct_spaces_and_digits() {
        assert_eq!(
            prepare_pocket("Цена:100 рублей!Он").unwrap(),
            "Цена: сто рублей! Он"
        );
        assert_eq!(
            prepare_pocket("Пи 3.14!").unwrap(),
            "Пи три точка четырнадцать !"
        );
    }

    // ---- 7. план пауз (pipeline) -------------------------------------------------

    #[test]
    fn pipeline_question_attribution() {
        let out = android_chunk_pipeline("Неуж+ели пр+авда? Поинтересов+ался Г+ари.", &opts());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].pause_s, Some(0.5));
        assert_eq!(out[0].boundary_kind, AndroidBoundary::IntraPhrase);
        assert_eq!(out[1].pause_s, None);
        assert_eq!(out[1].boundary_kind, AndroidBoundary::EndOfFile);
        // модельный текст с U+0301 и volatile-пробелом (ударение «Неуж+ели»
        // стоит на «е» начала «ели» — переносится как есть)
        assert_eq!(
            out[0].model_text.as_deref(),
            Some("Неуже\u{301}ли пра\u{301}вда ?")
        );
    }

    #[test]
    fn pipeline_tail_without_floor_is_zero() {
        // хвост вызова БЕЗ знака и НЕ конец абзаца → Some(0.0), пол 0.02 НЕ
        // применяется. Абзац >300 с запятыми: commaSplit закрывает кусок,
        // конечная «,» снимается → первая фраза вызова кончается буквой.
        let p = |c: char| format!("{c}{}", "л".repeat(98)); // 99 симв.
        let para = format!("{}, {}, {}, {}", p('а'), p('б'), p('в'), p('г'));
        let out = android_chunk_pipeline(&format!("{para}\n\nКон+ец."), &opts());
        // фраза вызова 1 → 2 чанка (199 симв. > 150), фраза 2 → 2, абзац 2 → 1
        assert_eq!(out.len(), 5);
        assert_eq!(out[0].pause_s, Some(SILENCE_JOINT_S)); // стык внутри фразы
        assert_eq!(out[1].pause_s, Some(0.0)); // хвост вызова без знака, не абзац
        assert_eq!(out[2].pause_s, Some(SILENCE_JOINT_S));
        assert_eq!(out[3].pause_s, Some(0.9)); // конец абзаца
        assert_eq!(out[4].pause_s, None); // конец файла
    }

    #[test]
    fn pipeline_paragraph_floor_and_scene() {
        let out = android_chunk_pipeline(
            "Абз+ац без коне́чной т+очки\n\n* * *\n\nНач+ало новой глав+ы.",
            &opts(),
        );
        // [хвост-абзац 0.9][сцена 1.3][последний None]
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].pause_s, Some(0.9));
        assert!(out[1].is_scene());
        assert_eq!(out[1].pause_s, Some(1.3));
        assert_eq!(out[2].pause_s, None);
        // scale умножает всё
        let o2 = AndroidChunkOptions {
            pause_scale: 2.0,
            ..opts()
        };
        let out2 = android_chunk_pipeline("Абз+ац без т+очки\n\n* * *\n\nНач+ало.", &o2);
        assert_eq!(out2[0].pause_s, Some(1.8));
        assert_eq!(out2[1].pause_s, Some(2.6));
    }

    #[test]
    fn pipeline_aside_transitions_get_min_pause() {
        // вход ≤ 55 симв. — fastStart не срабатывает
        let out = android_chunk_pipeline("Стар+ик (а ем+у за восемьдесят) молч+а.", &opts());
        assert_eq!(out.len(), 3);
        assert!(out[1].is_aside);
        assert_eq!(out[0].pause_s, Some(0.25)); // смена aside при owed=0
        assert_eq!(out[1].pause_s, Some(0.25)); // возврат из aside
        assert_eq!(out[2].pause_s, None);
    }

    #[test]
    fn pipeline_pause_scale_multiplies_sign_pauses() {
        let o2 = AndroidChunkOptions {
            pause_scale: 2.0,
            ..opts()
        };
        let out = android_chunk_pipeline("Неуж+ели пр+авда? Поинтересов+ался Г+ари.", &o2);
        assert_eq!(out[0].pause_s, Some(1.0));
        // пол стыка не масштабируется
        let out2 = android_chunk_pipeline("Слов+о (отв+лечённая рем+арка) кон+ец.", &o2);
        assert_eq!(out2[0].pause_s, Some(ASIDE_PAUSE_S * 2.0));
    }

    // ---- 8. K3-чистки ----------------------------------------------------------------

    #[test]
    fn k3_footnotes_urls_invisible_odd_hyphen() {
        assert_eq!(normalize_lite("слово [12] и"), "слово и");
        assert_eq!(normalize_lite("Слово¹² конец"), "Слово конец");
        assert_eq!(
            normalize_lite("Слово², но м² остаётся"),
            "Слово, но м² остаётся"
        );
        // glued: снимается ТОЛЬКО в конце строки (quirk оригинала,
        // проверен на JVM; см. комментарий у FOOTNOTE_GLUED)
        assert_eq!(normalize_lite("приклеено1"), "приклеено");
        assert_eq!(normalize_lite("приклеено1, дальше"), "приклеено1, дальше");
        assert_eq!(normalize_lite("км2"), "км2");
        assert_eq!(
            normalize_lite("см. https://example.com/x конец"),
            "см.  конец"
        );
        assert_eq!(
            normalize_lite("письмо user@mail.ru пришло"),
            "письмо  пришло"
        );
        assert_eq!(normalize_lite("сло\u{00AD}во"), "слово");
        assert_eq!(normalize_lite("какие\u{2010}то"), "какие-то");
        // приклеенная «*»-сноска после знака (ASCII-кавычка в классе lookbehind)
        assert_eq!(normalize_lite("слово\"* хвост"), "слово\" хвост");
        assert_eq!(normalize_lite("слово.* дальше"), "слово. дальше");
    }

    #[test]
    fn k3_noop_on_plain_russian() {
        assert_eq!(
            normalize_lite("Обычное русское предлож+ение с ударениями."),
            "Обычное русское предлож+ение с ударениями."
        );
    }

    // ---- golden: 38 кейсов cases.json ----------------------------------------------

    #[derive(serde::Deserialize)]
    struct GoldenRec {
        text: String,
        #[allow(dead_code)]
        chars: usize,
        pause_s: Option<f32>,
        pause_kind: String,
        aside: bool,
        paragraph_end: bool,
        model_text: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct GoldenCase {
        name: String,
        input: String,
        android: Vec<GoldenRec>,
    }

    fn boundary_of(kind: &str) -> AndroidBoundary {
        if kind == "scene_break" {
            AndroidBoundary::SceneBreak
        } else if kind == "end_of_file" {
            AndroidBoundary::EndOfFile
        } else if kind.starts_with("paragraph=") {
            AndroidBoundary::CallTail { paragraph: true }
        } else if kind.starts_with("utterance_tail=") {
            AndroidBoundary::CallTail { paragraph: false }
        } else {
            // "joint" | "sign=…"
            AndroidBoundary::IntraPhrase
        }
    }

    #[test]
    fn golden_android_chunking_cases() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/golden/android_chunking_cases.json"
        );
        let raw = std::fs::read_to_string(path).expect("golden cases file");
        let cases: Vec<GoldenCase> = serde_json::from_str(&raw).expect("golden cases json");
        assert_eq!(cases.len(), 38, "ожидается 38 кейсов");
        let mut failures: Vec<String> = Vec::new();
        for case in &cases {
            let got = android_chunk_pipeline(&case.input, &opts());
            if got.len() != case.android.len() {
                failures.push(format!(
                    "{}: чанков {} ≠ {}",
                    case.name,
                    got.len(),
                    case.android.len()
                ));
                continue;
            }
            for (i, (g, e)) in got.iter().zip(case.android.iter()).enumerate() {
                if g.text != e.text {
                    failures.push(format!(
                        "{}[{}]: текст {:?} ≠ {:?}",
                        case.name, i, g.text, e.text
                    ));
                }
                if g.model_text != e.model_text {
                    failures.push(format!(
                        "{}[{}]: model_text {:?} ≠ {:?}",
                        case.name, i, g.model_text, e.model_text
                    ));
                }
                match (g.pause_s, e.pause_s) {
                    (Some(a), Some(b)) if (a - b).abs() < 1e-6 => {}
                    (None, None) => {}
                    (a, b) => {
                        failures.push(format!("{}[{}]: pause_s {:?} ≠ {:?}", case.name, i, a, b))
                    }
                }
                if g.is_aside != e.aside {
                    failures.push(format!("{}[{}]: aside", case.name, i));
                }
                if g.paragraph_end != e.paragraph_end {
                    failures.push(format!("{}[{}]: paragraph_end", case.name, i));
                }
                if g.boundary_kind != boundary_of(&e.pause_kind) {
                    failures.push(format!(
                        "{}[{}]: boundary {:?} ≠ {:?} (kind {})",
                        case.name,
                        i,
                        g.boundary_kind,
                        boundary_of(&e.pause_kind),
                        e.pause_kind
                    ));
                }
            }
        }
        // первые расхождения — понятный diff
        assert!(
            failures.is_empty(),
            "golden-несовпадения ({}):\n{}",
            failures.len(),
            failures
                .iter()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn scenes_consecutive_start_and_end() {
        // сцены подряд — каждая отдельной записью по 1.3 с (интеграция
        // суммирует их в pause_s предыдущего чанка)
        let out = android_chunk_pipeline("А.\n\n* * *\n\n\u{2042}\n\nБ.", &opts());
        let scenes: Vec<&AndroidChunk> = out.iter().filter(|c| c.is_scene()).collect();
        assert_eq!(scenes.len(), 2);
        assert!(scenes.iter().all(|s| s.pause_s == Some(PAUSE_SCENE)));
        // сцена в начале файла — запись есть (пауза без предыдущего чанка
        // теряется на интеграции с заметкой — WINDOWS_INTEGRATION §4)
        let out = android_chunk_pipeline("* * *\n\nБ.", &opts());
        assert!(out[0].is_scene());
        assert_eq!(out[0].pause_s, Some(PAUSE_SCENE));
        // сцена — последний юнит файла: пауза не вставляется
        let out = android_chunk_pipeline("А.\n\n* * *", &opts());
        assert!(out.last().unwrap().is_scene());
        assert_eq!(out.last().unwrap().pause_s, None);
    }

    // ---- эталон: 99 чанков эталонного TXT --------------------------------------------

    #[derive(serde::Deserialize)]
    struct ReferenceRec {
        chunk_text: String,
        #[serde(rename = "pause_after_s")]
        pause_s: Option<f32>,
        pause_kind: String,
        aside: bool,
        paragraph_end: bool,
        model_text: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct ReferenceFile {
        #[allow(dead_code)]
        input_sha256: String,
        chunks: Vec<ReferenceRec>,
    }

    #[test]
    fn golden_android_reference_99_chunks() {
        let input = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/golden/android_reference_input.txt"
        ))
        .expect("reference input");
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/golden/android_reference_chunks.json"
        ))
        .expect("reference chunks");
        let reference: ReferenceFile = serde_json::from_str(&raw).expect("reference json");
        assert_eq!(reference.chunks.len(), 99);

        let got = android_chunk_pipeline(&input, &opts());
        let mut failures: Vec<String> = Vec::new();
        if got.len() != 99 {
            failures.push(format!("чанков {} ≠ 99", got.len()));
        }
        for (i, (g, e)) in got.iter().zip(reference.chunks.iter()).enumerate() {
            if g.text != e.chunk_text {
                failures.push(format!("[{i}] текст {:?} ≠ {:?}", g.text, e.chunk_text));
            }
            if g.model_text != e.model_text {
                failures.push(format!(
                    "[{i}] model_text {:?} ≠ {:?}",
                    g.model_text, e.model_text
                ));
            }
            match (g.pause_s, e.pause_s) {
                (Some(a), Some(b)) if (a - b).abs() < 1e-6 => {}
                (None, None) => {}
                (a, b) => failures.push(format!("[{i}] pause_s {a:?} ≠ {b:?}")),
            }
            if g.is_aside != e.aside {
                failures.push(format!("[{i}] aside"));
            }
            if g.paragraph_end != e.paragraph_end {
                failures.push(format!("[{i}] paragraph_end"));
            }
            if g.boundary_kind != boundary_of(&e.pause_kind) {
                failures.push(format!(
                    "[{i}] boundary {:?} ≠ kind {}",
                    g.boundary_kind, e.pause_kind
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "эталон 99 чанков (первые расхождения):\n{}",
            failures
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );

        // агрегаты ВЫЧИСЛЯЮТСЯ из per-chunk данных (SPEC_CORRECTIONS 0.3):
        // summary.paragraph_pauses=70 — счётчик флага, НЕ число пауз абзаца
        let pause_sum: f32 = got.iter().filter_map(|c| c.pause_s).sum();
        assert!((pause_sum - 56.98).abs() < 1e-3, "сумма пауз {pause_sum}");
        let paragraph_pauses = got
            .iter()
            .filter(|c| {
                matches!(
                    c.boundary_kind,
                    AndroidBoundary::CallTail { paragraph: true }
                )
            })
            .count();
        assert_eq!(paragraph_pauses, 38, "фактических пауз абзаца");
        assert_eq!(got.iter().filter(|c| c.is_aside).count(), 0);
        assert!(got.iter().all(|c| c.is_scene() || c.model_text.is_some()));
        // отброшенных нет: все чанки имеют модельный текст
        assert_eq!(
            got.iter()
                .filter(|c| c.model_text.is_none() && !c.is_scene())
                .count(),
            0
        );
    }

    #[test]
    fn chunking_mode_serde_contract() {
        // SPEC_CORRECTIONS 0.1: нормативный JSON-контракт
        assert_eq!(
            serde_json::to_string(&ChunkingMode::Standard).unwrap(),
            "\"standard\""
        );
        assert_eq!(
            serde_json::to_string(&ChunkingMode::TeraTtsAndroid).unwrap(),
            "\"teratts_android\""
        );
        assert_eq!(
            serde_json::from_str::<ChunkingMode>("\"standard\"").unwrap(),
            ChunkingMode::Standard
        );
        assert_eq!(
            serde_json::from_str::<ChunkingMode>("\"teratts_android\"").unwrap(),
            ChunkingMode::TeraTtsAndroid
        );
        // «terattsandroid» и неизвестные значения не десериализуются
        assert!(serde_json::from_str::<ChunkingMode>("\"terattsandroid\"").is_err());
        assert!(serde_json::from_str::<ChunkingMode>("\"android\"").is_err());
    }
}
