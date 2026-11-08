# -*- coding: utf-8 -*-
"""Эталонные значения trailing_pause_s из MAIN synthesis_worker.py (read-only)."""
import sys
import importlib.util
import io

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
spec = importlib.util.spec_from_file_location(
    "sw", r"C:\PocketTTS-GUI\PocketTTS-GUI\synthesis_worker.py")
sw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sw)

cases = [
    "Слово.", "Слово!", "Слово?", "Слов…", "Слов...", "Слово:", "Слово;",
    "Слово —", "Слово –", "Слово,", "Слово", "«Цитата».", "Цитата)",
    "итаг)", "итoг'", 'конеu»', "многоточие....", 'конец"',"фраза».",
    "   ", "", "точка.  ", "скобка)", 'кавыкu"',
]
for c in cases:
    print(repr(c), "->", sw.trailing_pause_s(c))
print("consts:", sw.PAUSE_COMMA, sw.PAUSE_DASH, sw.PAUSE_SEMICOLON,
      sw.PAUSE_COLON, sw.PAUSE_SENTENCE, sw.PAUSE_ELLIPSIS)

# поведение compute_pauses_for_gluing main при soft_break на is_last-чанке
chunks = [
    {"text": "Абзац без знака в конце,", "is_last": True, "soft_break": True},
    {"text": "Второй абзац.", "is_last": True, "soft_break": False},
]
for row in sw.compute_pauses_for_gluing(chunks, 1.0, 0.9):
    print("pause:", row)
