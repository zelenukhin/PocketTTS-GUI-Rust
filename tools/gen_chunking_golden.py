# -*- coding: utf-8 -*-
"""Генерация golden-тестов чанкования из ФАКТИЧЕСКОГО поведения Python-кода
ветки comma-semantics (проект PocketTTS-GUI не изменяется: оба файла
извлекаются через `git show` во временную папку).

Выход: tests/golden/chunking_golden.json — корпус {name, text, max_tokens} ->
[{text, is_last, soft_break}] после полного конвейера ветки:
    terminate_bare_chunks(annotate_soft_breaks(chunk_text(text, max_bytes)))
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
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests", "golden",
                   "chunking_golden.json")

# ---------- корпус ----------
BOOK = r"C:\Users\<user>\Documents\Гарри Поттер отрывок 2_accented_processed_normalized.txt"

def corpus():
    cases = []
    cases.append(("и-цепочка без пунктуации",
        "Старый город встретил нас утренней тишиной и узкими переулками и спящими кошками "
        "на тёплых подоконниках и запахом свежего хлеба из маленькой пекарни и далёким "
        "колокольным звоном и множеством непонятных вывесок над лавками со стеклянными дверями"))
    cases.append(("монстр без союзов и запятых",
        "длинноесловораз длинноесловодва длинноесловотри длинноесловочетыре длинноесловопять "
        "длинноесловошесть длинноесловосемь длинноеслововосемь длинноесловодевять длинноесловоодиннадцать"))
    cases.append(("родная пунктуация",
        "Привет, это тест. Второе предложение с запятой, и точкой. Последнее предложение."))
    cases.append(("многоточия и вопрос с точками",
        "Он молчал… очень долго… Потом спросил: «Правда?!..» И снова замолчал… До самого утра."))
    cases.append(("тире и двоеточия",
        "Список был короток: вода; хлеб; соль — всё, что нужно человеку в пути; и немного терпения."))
    cases.append(("абзацы и CRLF",
        "Первый абзац с парой предложений. И ещё одно.\r\n\r\nВторой абзац: он начался с двоеточия, продолжился запятой\r\nи закончился точкой!\r\n\r\n\r\nТретий — короткий."))
    cases.append(("скобки и кавычки",
        "«Цитата (важная!) в кавычках», — сказал он. Ответ пришёл быстро: «нет» (что ожидаемо)."))
    cases.append(("короткий текст", "Ну?"))
    cases.append(("пустой и пробелы", "   \n\n  \n"))
    cases.append(("одна длинная фраза без разрывов",
        "Помещик гордился своим садом и каждое утро обходил его по кругу начиная от старой яблони у ворот мимо пасеки и вдоль речки где росли ивы"))
    cases.append(("ударения и числа",
        "Стр+елки час+ов показ+ывали п+олночь, а стрелк+и заст+ыли на пост+ах. 12 345 678 рублей?"))
    cases.append(("И с заглавной в начале после точки",
        "Всё кончилось. И началось anew — то есть заново. И снова всё пошло по кругу, и так далее."))
    # реальные абзацы книги (первые 12 непустых строк, без изменений)
    if os.path.isfile(BOOK):
        with open(BOOK, "r", encoding="utf-8") as f:
            lines = [l.strip() for l in f if l.strip()]
        for i, l in enumerate(lines[:12]):
            cases.append((f"книга[{i}]", l))
    return cases

def main():
    tmp = tempfile.mkdtemp(prefix="golden_gui_")
    for fname in ("pocket-tts_gui.py", "synthesis_worker.py"):
        blob = subprocess.check_output(
            ["git", "-C", REPO, "show", f"{BRANCH}:PocketTTS-GUI/{fname}"])
        with open(os.path.join(tmp, fname), "wb") as f:
            f.write(blob)
    sys.path.insert(0, tmp)

    spec = importlib.util.spec_from_file_location(
        "golden_gui", os.path.join(tmp, "pocket-tts_gui.py"))
    ptg = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(ptg)

    chunk_text = ptg.chunk_text
    annotate = ptg.annotate_soft_breaks
    terminate = ptg.terminate_bare_chunks
    max_bytes_of = ptg.compute_max_chunk_bytes

    out = []
    for name, text in corpus():
        for max_tokens in (30, 50, 120, 300):
            chunks = chunk_text(text, max_bytes_of(max_tokens))
            chunks = terminate(annotate(chunks))
            out.append({
                "name": name, "max_tokens": max_tokens,
                "max_bytes": max_bytes_of(max_tokens),
                "input": text,
                "chunks": [{"text": c["text"],
                            "is_last": bool(c.get("is_last")),
                            "soft_break": bool(c.get("soft_break"))} for c in chunks],
            })
            # самопроверка сохранности текста: конкатенация чанков покрывает все
            # непробельные символы входа (нарезка не теряет и не дублирует слова)
            src = "".join(text.split())
            dst = "".join("".join(c["text"].rstrip(",").rstrip(".").split()) for c in chunks)
            src_n = "".join(src.split())
            dst_n = "".join(dst.split())
            # terminate может добавить ','/'.' — сравниваем по буквам/цифрам
            import re
            letters = lambda s: re.sub(r"[^0-9A-Za-zА-Яа-яЁё]", "", s)
            assert letters(src_n) == letters(dst_n), f"потеря текста: {name}/{max_tokens}"

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=1)
    print(f"golden: {len(out)} cases -> {OUT}")

if __name__ == "__main__":
    main()
