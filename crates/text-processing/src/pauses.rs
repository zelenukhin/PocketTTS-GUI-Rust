//! Паузы по пунктуации (модель teratts-android) — порт `trailing_pause_s` и
//! `compute_pauses_for_gluing` из PocketTTS-GUI (main; synthesis_worker.py /
//! pocket-tts_gui.py). Эталонные значения сняты с Python (tools/pause_reference.py).
//!
//! Отличие от Python (сознательное, минимальное): приоритет конца абзаца.
//! В Python compute_pauses проверяет soft_break РАНЬШЕ is_last, поэтому чанк,
//! получивший soft_break=True от `terminate_bare_chunks` (голый конец абзаца
//! получает запятую + soft), ТЕРЯЕТ паузу абзаца: замер main даёт 0.02 с
//! вместо max(знак, 0.9) с. Инвариант main («абзацный чанк никогда не мягкий»)
//! нарушается только этим случаем. Здесь is_last (конец абзаца) проверяется
//! первым: запятая из terminate остаётся в тексте (EOS-стабильность
//! сохранена), а пауза абзаца восстанавливается. См. регрессионный тест
//! `paragraph_pause_survives_terminate_bare`.

/// Пауза после знака (тератт-модель, synthesis_worker.py).
pub const PAUSE_COMMA: f32 = 0.15;
pub const PAUSE_DASH: f32 = 0.25;
pub const PAUSE_SEMICOLON: f32 = 0.30;
pub const PAUSE_COLON: f32 = 0.35;
pub const PAUSE_SENTENCE: f32 = 0.50;
pub const PAUSE_ELLIPSIS: f32 = 0.65;

/// Хвостовые закрывающие символы, срезаемые перед определением знака
/// (Python: '»"“”\')' — кавычки, апостроф, обратный слэш, скобка).
const CLOSERS: &str = "»“”\\')\"";

/// Базовая пауза (с) после фразы по её завершающему знаку — 1:1 порт
/// `trailing_pause_s`. Значения сверены с Python (tools/pause_reference.py).
pub fn trailing_pause_s(text: &str) -> f32 {
    let mut t = text.trim_end();
    while let Some(last) = t.chars().last() {
        if CLOSERS.contains(last) {
            t = t[..t.len() - last.len_utf8()].trim_end();
        } else {
            break;
        }
    }
    if t.ends_with("...") || t.ends_with('…') {
        return PAUSE_ELLIPSIS;
    }
    let Some(last) = t.chars().last() else {
        return 0.0;
    };
    match last {
        '.' | '!' | '?' => PAUSE_SENTENCE,
        ':' => PAUSE_COLON,
        ';' => PAUSE_SEMICOLON,
        '—' | '–' => PAUSE_DASH,
        ',' => PAUSE_COMMA,
        _ => 0.0,
    }
}

/// Минимальная вставка на стыке чанков (Python: минимум стыка 0.02 с).
pub const MIN_JOINT_S: f32 = 0.02;

/// Пауза после чанка при склейке (порт compute_pauses_for_gluing с
/// исправленной приоритетностью, см. шапку модуля).
///
/// Возвращает:
/// - `None` — после последнего чанка файла вставки нет;
/// - `Some(gap)` — тишина в секундах: мягкая граница → модельная (минимум
///   стыка), конец абзаца → max(пауза знака, пауза абзаца), иначе пауза
///   знака; всё × pause_scale, но не меньше MIN_JOINT_S.
pub fn chunk_pause_s(
    chunk: &crate::chunking::Chunk,
    is_global_last: bool,
    paragraph_pause_s: f32,
    pause_scale: f32,
) -> Option<f32> {
    if is_global_last {
        return None;
    }
    let base = if chunk.is_last {
        // Конец абзаца: инвариант main восстановлен — пауза абзаца не теряется
        // даже если terminate_bare_chunks пометил чанк мягким.
        trailing_pause_s(&chunk.text).max(paragraph_pause_s)
    } else if chunk.soft_break {
        // Мягкая граница (запятая/«и»): паузу отыграет модель — только стык.
        0.0
    } else {
        trailing_pause_s(&chunk.text)
    };
    Some((base * pause_scale).max(MIN_JOINT_S))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunking::{chunk_pipeline, Chunk};

    /// Эталон с Python (tools/pause_reference.py, main synthesis_worker.py).
    #[test]
    fn trailing_pause_matches_python() {
        let cases: &[(&str, f32)] = &[
            ("Слово.", 0.5),
            ("Слово!", 0.5),
            ("Слово?", 0.5),
            ("Слов…", 0.65),
            ("Слов...", 0.65),
            ("многоточие....", 0.65),
            ("Слово:", 0.35),
            ("Слово;", 0.3),
            ("Слово —", 0.25),
            ("Слово –", 0.25),
            ("Слово,", 0.15),
            ("Слово", 0.0),
            ("«Цитата».", 0.5),
            ("фраза».", 0.5),
            ("Цитата)", 0.0),
            ("итoг'", 0.0),
            ("конеu»", 0.0),
            ("конец\"", 0.0),
            ("   ", 0.0),
            ("", 0.0),
            ("точка.  ", 0.5),
        ];
        for (text, want) in cases {
            let got = trailing_pause_s(text);
            assert!((got - want).abs() < 1e-6, "{text:?}: {got} != {want}");
        }
    }

    /// РЕГРЕССИЯ (найдена при проверке): конец абзаца словом без пунктуации
    /// после terminate_bare_chunks получает «,» + soft_break=True. В Python
    /// compute_pauses при soft_break давал 0.02 с — пауза абзаца ТЕРЯЛАСЬ
    /// (замер: pause: (0.02, 'граница по запятой…')). Здесь конец абзаца
    /// берёт max(запятая 0.15, пауза абзаца 0.90) = 0.90 — инвариант main
    /// («абзацный чанк не мягкий») восстановлен на стороне пауз, чанки
    /// (golden-паритет с Python) не тронуты.
    #[test]
    fn paragraph_pause_survives_terminate_bare() {
        let chunks = chunk_pipeline(
            "Абзац кончается словом без знака\n\nВторой абзац короткий.",
            300,
        );
        assert_eq!(chunks.len(), 2);
        // чанк 0 — то, что сделал terminate_bare_chunks с голым концом абзаца
        assert_eq!(chunks[0].text, "Абзац кончается словом без знака,");
        assert!(chunks[0].is_last, "конец первого абзаца");
        assert!(
            chunks[0].soft_break,
            "terminate пометил мягким (паритет Python)"
        );
        // исправленная пауза: абзацная, а не 0.02
        let pause = chunk_pause_s(&chunks[0], false, 0.9, 1.0).unwrap();
        assert!(
            (pause - 0.9).abs() < 1e-6,
            "пауза абзаца: {pause} (Python-баг давал 0.02)"
        );
        // контроль: чанк с родной точкой — совпадает с Python (0.90);
        // глобально последний — без вставки
        assert_eq!(chunk_pause_s(&chunks[1], true, 0.9, 1.0), None);
        let chunks2 = [
            Chunk {
                text: "Абзац со знаком.".into(),
                is_last: true,
                soft_break: false,
            },
            Chunk {
                text: "Дальше.".into(),
                is_last: true,
                soft_break: false,
            },
        ];
        let p = chunk_pause_s(&chunks2[0], false, 0.9, 1.0).unwrap();
        assert!((p - 0.9).abs() < 1e-6, "max(0.5, 0.9) = 0.9, получено {p}");
    }

    /// Мягкая граница в середине — модельная пауза (только минимум стыка),
    /// масштаб пауз действует, глобально последний — None.
    #[test]
    fn soft_break_and_scaling() {
        let mid_soft = Chunk {
            text: "тихо,".into(),
            is_last: false,
            soft_break: true,
        };
        assert_eq!(chunk_pause_s(&mid_soft, false, 0.9, 1.0), Some(0.02));
        assert_eq!(chunk_pause_s(&mid_soft, false, 0.9, 2.0), Some(0.02)); // max(0*2, 0.02)
        let mid_hard = Chunk {
            text: "Точка.".into(),
            is_last: false,
            soft_break: false,
        };
        assert!((chunk_pause_s(&mid_hard, false, 0.9, 1.0).unwrap() - 0.5).abs() < 1e-6);
        assert!((chunk_pause_s(&mid_hard, false, 0.9, 2.0).unwrap() - 1.0).abs() < 1e-6);
        assert_eq!(chunk_pause_s(&mid_hard, true, 0.9, 1.0), None);
        // мягкая граница ниже минимума стыка поднимается до него
        let comma = Chunk {
            text: "фраза,".into(),
            is_last: false,
            soft_break: false,
        };
        assert!((chunk_pause_s(&comma, false, 0.9, 0.0).unwrap() - 0.02).abs() < 1e-6);
    }
}
