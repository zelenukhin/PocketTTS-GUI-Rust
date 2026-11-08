# -*- coding: utf-8 -*-
r"""Golden-корпус apply_char_rules из ФАКТИЧЕСКОГО поведения Python-кода
ветки comma-semantics (synthesis_worker.py извлекается через `git show`,
проект не изменяется). Отличие ветки от main: хвостовая запятая чанка
СОХРАНЯЕТСЯ (нормализация `(?<=\S),(?=\s*$)` удалена).

Выход: tests/golden/char_rules_golden.json — {name, text, rules, expected}.
"""
import io
import json
import os
import subprocess
import sys
import tempfile
import importlib.util

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", errors="replace")

REPO = r"C:\PocketTTS-GUI"
BRANCH = "comma-semantics"
GUI_RUNTIME = r"C:\PocketTTS-GUI\PocketTTS-GUI\runtime.json"
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests",
                   "golden", "char_rules_golden.json")


def real_rules():
    with open(GUI_RUNTIME, "r", encoding="utf-8") as f:
        return json.load(f).get("char_rules", {})


def rule_sets():
    return {
        "real_gui_runtime": real_rules(),
        "char_map_only": {"char_map": {"„": "«", "“": "«", "”": "»", "’": "»"}},
        "regex_rules_only": {"regex_rules": [
            {"pattern": r"([!?])\.{2,}", "replacement": r"\1"},
            {"pattern": r"\.{2,}(?=[,;:!?…])", "replacement": ""},
            {"pattern": r"\.{2,}(?=\s+[а-яёa-z])", "replacement": ""},
            {"pattern": r"\.{2,}", "replacement": "."},
            {"pattern": r"([!?])\1{2,}", "replacement": r"\1"},  # ?!??!
            {"pattern": "[безопасный-невалидный", "replacement": "x"},  # невалидный -> пропускается
        ]},
        "dash_only": {"dash_context": "–−‒‑―"},
        "bracket_only": {"bracket_comma": "()[]{}⟨⟩"},
        "delete_only": {"delete_chars": "%&@#*/_$€£¥§¶†‡•"},
        "combined": {
            "char_map": {"„": "«", "“": "«", "”": "»", "‹": "«", "›": "»"},
            "regex_rules": [{"pattern": r"\.{2,}", "replacement": "."}],
            "dash_context": "–−‒‑―",
            "bracket_comma": "()[]{}⟨⟩",
            "delete_chars": "%@#",
        },
        "empty": {},
        "empty_strings": {"char_map": {}, "regex_rules": [], "dash_context": "",
                          "bracket_comma": "", "delete_chars": ""},
    }


def corpus():
    return [
        # прямые и умные кавычки, направление «"»
        'Он сказа"л „привет“, а она ответила ”нет”.',
        '"В начале строки", потом "в середине" и "в конце"',
        'кавычка"вплотную',
        '„глубокие“ открывающие',
        # тире: внутри слова / отдельно / в начале и конце
        'кто–то пришёл, год – два, длинное — тире, минус −5, ‑дефис, ―фон',
        '– в начале',
        'конец –',
        # скобки: с запятыми и без, перед сильной пунктуацией, вложенность
        'текст, (текст)! и текст(текст).',
        'скобка (внутри) продолжение',
        'скобка (внутри), продолжение',
        '(в начале) предложения',
        'в конце (скобка)',
        'массив [1, 2] и {фигурные} и ⟨угловые⟩',
        # проценты и delete-символы
        'скидка 50% заканчивается',
        'текст % текст',
        '50% скидка',
        'стоимость $100 и €50 и ¥200',
        '#хэш & амперсанд * звёздочка / слэш _ подчёркивание § ¶ † ‡ •',
        'символ%вплотную',
        'конец%',
        '&начало',
        # многоточия и знаки
        'правда?!.. ну да... ага… стоп....',
        'вопрос??! и восклиц!!! и смешанное!?.',
        'точка.. две',
        # запятые и нормализация
        'запятая,перед словом и ;также :двоеточие',
        'перед сильной , . ! ?',
        'конец фразы,' ,  # хвостовая запятая — СОХРАНЯЕТСЯ (семантика ветки)
        'конец фразы,  ',
        # пробелы и переносы
        'много   пробелов \t таб и \n\n переносы',
        ', строка начинается с запятой',
        'строка заканчивается пробелами   \nи следующая   ',
        # ударения U+0301 сохраняются
        'прив+ет это т+ест',
        'приве́т э́то те́ст и ё́лка',
        # всё вместе
        '«Ну и что?!..» —сказал он (тихо), и ушёл... 50% уверенности „точно“',
    ]


def main():
    tmp = tempfile.mkdtemp(prefix="golden_sw_")
    blob = subprocess.check_output(
        ["git", "-C", REPO, "show", f"{BRANCH}:PocketTTS-GUI/synthesis_worker.py"])
    sw_path = os.path.join(tmp, "synthesis_worker.py")
    with open(sw_path, "wb") as f:
        f.write(blob)
    sys.path.insert(0, tmp)
    spec = importlib.util.spec_from_file_location("sw", sw_path)
    sw = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(sw)

    # «+»-нотация -> U+0301: apply_char_rules в конвейере идёт ПОСЛЕ неё
    texts = [sw.to_model_stress_notation(t) for t in corpus()]

    out = []
    for rname, rules in rule_sets().items():
        for i, text in enumerate(texts):
            expected = sw.apply_char_rules(text, rules)
            out.append({
                "name": f"{rname}[{i}]", "text": text, "rules": rules,
                "expected": expected,
            })
    # дополнительно: два прохода правил подряд (идемпотентность на реальных правилах)
    rr = real_rules()
    for i, text in enumerate(texts[:8]):
        once = sw.apply_char_rules(text, rr)
        out.append({
            "name": f"twice[{i}]", "text": once, "rules": rr,
            "expected": sw.apply_char_rules(once, rr),
        })

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=1)
    print(f"golden: {len(out)} cases -> {OUT}")


if __name__ == "__main__":
    main()
