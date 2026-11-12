//! Текстовый препроцессинг для Shtorm PocketTTS (порт Python-GUI без изменений семантики).
//!
//! Источник: `C:\PocketTTS-GUI\PocketTTS-GUI\synthesis_worker.py`,
//! `to_model_stress_notation()` — 1:1 перенос:
//! NFC → «+гласная» → «гласная + U+0301» → снятие акута над «ё» → NFC.

pub mod char_rules;
pub mod chunking;
pub mod pauses;
pub use char_rules::{apply_char_rules, CharRules};
pub use chunking::{
    annotate_soft_breaks, chunk_pipeline, chunk_text, compute_max_chunk_bytes,
    terminate_bare_chunks, Chunk,
};
pub use pauses::{chunk_pause_s, trailing_pause_s};

use unicode_normalization::UnicodeNormalization;

const RUSSIAN_VOWELS: &str = "аеёиоуыэюяАЕЁИОУЫЭЮЯ";

/// Приводит текст к нотации модели: «+о» -> «о» + U+0301.
///
/// NFC-нормализация, конвертация префиксного «+» в комбинируемый акут,
/// снятие избыточного акута над «ё» (она всегда ударная). Уже размеченный
/// U+0301 текст проходит без изменений. «+» не перед гласной остаётся как есть.
pub fn to_model_stress_notation(text: &str) -> String {
    let nfc: String = text.nfc().collect();
    let mut out = String::with_capacity(nfc.len() + 8);
    let chars: Vec<char> = nfc.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '+' && i + 1 < chars.len() && RUSSIAN_VOWELS.contains(chars[i + 1]) {
            out.push(chars[i + 1]);
            out.push('\u{0301}');
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // ё всегда ударная — избыточный акут над ней снимается
    let mut no_yo: String = String::with_capacity(out.len());
    let cs: Vec<char> = out.chars().collect();
    let mut j = 0;
    while j < cs.len() {
        if matches!(cs[j], 'ё' | 'Ё') && j + 1 < cs.len() && cs[j + 1] == '\u{0301}' {
            no_yo.push(cs[j]);
            j += 2;
            continue;
        }
        no_yo.push(cs[j]);
        j += 1;
    }
    no_yo.nfc().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_plus_to_acute() {
        assert_eq!(
            to_model_stress_notation("прив+ет, +это т+ест"),
            "приве\u{301}т, э\u{301}то те\u{301}ст"
        );
    }

    #[test]
    fn keeps_native_acute() {
        let s = "замо\u{301}к и за\u{301}мок";
        assert_eq!(to_model_stress_notation(s), s);
    }

    #[test]
    fn removes_acute_over_yo() {
        assert_eq!(
            to_model_stress_notation("ё\u{0301}лка Ё\u{0301}ЛКА"),
            "ёлка ЁЛКА"
        );
        // «+ё» тоже без акута
        assert_eq!(to_model_stress_notation("+ёлка"), "ёлка");
    }

    #[test]
    fn keeps_plus_before_non_vowel() {
        assert_eq!(to_model_stress_notation("2+2 = 4"), "2+2 = 4");
        assert_eq!(to_model_stress_notation("C++ и F#"), "C++ и F#");
    }

    #[test]
    fn nfc_normalizes_decomposed_input() {
        // NFD-форма «е» + U+0301 должна остаться корректным «е́» (NFC)
        let nfd = "приве\u{0301}т";
        let out = to_model_stress_notation(nfd);
        assert_eq!(out, "приве\u{301}т");
        assert_eq!(out.chars().count(), 7);
    }
}
