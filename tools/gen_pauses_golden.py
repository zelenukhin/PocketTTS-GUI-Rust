# -*- coding: utf-8 -*-
r"""Golden-случаи пауз и склейки A3 из фактического поведения Python GUI.

Чанки: ветка comma-semantics (terminate_bare_chunks(annotate_soft_breaks(
chunk_text))) — как в A1, через `git show`. Паузы: compute_pauses_for_gluing
из MAIN pocket-tts_gui.py (тератт-модель). Проект не изменяется.

Для каждого случая сохраняются:
- chunks: [{text, is_last, soft_break}]
- python_pauses: [(секунды|None, описание)] из main-ветки
- rust_pauses: наш расчёт с ИСПРАВЛЕННЫМ приоритетом (конец абзаца выше
  soft_break — задокументированное расхождение, docs/SAFETY_AUDIT / FEATURE_PARITY)
- divergent[i]: True, где python и rust расходятся (и почему)
- pause_samples (fs=24000), суммарные длительности пауз

Rust-тест обязан: не-divergent узлы == python (побайтно), divergent узлы ==
max(trailing, paragraph)*scale (минимум стыка 0.02), последний чанк — None.
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
FS = 24000
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests",
                   "golden", "pauses_golden.json")

TEXTS = {
    "многоабзацный": (
        "Стр+елки час+ов показ+ывали п+олночь. В д+оме сп+али, и т+олько стар+ые "
        "часы продолжали свой мерный разговор.\n"
        "Она открыла окно, посмотрела на улицу и тихо вздохнула: за стеклом "
        "шумел дождь.\n"
        "Утро пришло быстро."
    ),
    "голый_конец_абзаца": (
        "Первый абзац кончается знаком.\n"
        "Второй абзац кончается словом без знака\n"
        "Третий короткий."
    ),
    "и_границы": (
        "Старый город встретил нас тишиной и узкими переулками и спящими "
        "кошками и запахом хлеба и далёким звоном"
    ),
    "знаки": "Точка. Запятая, двоеточие: точка с запятой; тире — и многоточие… конец?",
    "монстр": (
        "длинноесловораз длинноесловодва длинноесловотри длинноесловочетыре "
        "длинноесловопять длинноесловошесть длинноесловосемь длинноеслововосемь"
    ),
}


def main():
    # чанкинг — из ветки comma-semantics (оба файла ветки: GUI + worker)
    tmp = tempfile.mkdtemp(prefix="golden_pauses_")
    for fname in ("pocket-tts_gui.py", "synthesis_worker.py"):
        blob = subprocess.check_output(
            ["git", "-C", REPO, "show", f"comma-semantics:PocketTTS-GUI/{fname}"])
        with open(os.path.join(tmp, fname), "wb") as f:
            f.write(blob)
    sys.path.insert(0, tmp)
    spec = importlib.util.spec_from_file_location(
        "gui_branch", os.path.join(tmp, "pocket-tts_gui.py"))
    gb = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(gb)

    # Все данные чанкования сняты; веточный worker больше не нужен в кэше
    # модулей — иначе main-GUI возьмёт его же и упадёт на TERATTS_константах.
    sys.modules.pop("synthesis_worker", None)
    sys.path.remove(tmp)

    # паузы — из main (тератт-модель compute_pauses_for_gluing)
    main_dir = r"C:\PocketTTS-GUI\PocketTTS-GUI"
    sys.path.insert(0, main_dir)
    spec2 = importlib.util.spec_from_file_location(
        "gui_main", os.path.join(main_dir, "pocket-tts_gui.py"))
    gm = importlib.util.module_from_spec(spec2)
    spec2.loader.exec_module(gm)
    # main-GUI делает `from synthesis_worker import ...` (имена, не модуль):
    # грузим main-воркер отдельным spec — источник trailing_pause_s.
    spec_sw = importlib.util.spec_from_file_location(
        "sw_main", os.path.join(main_dir, "synthesis_worker.py"))
    sw = importlib.util.module_from_spec(spec_sw)
    spec_sw.loader.exec_module(sw)

    def full_chunks(text, max_tokens):
        return gb.terminate_bare_chunks(
            gb.annotate_soft_breaks(gb.chunk_text(text, gb.compute_max_chunk_bytes(max_tokens))))

    def rust_pauses(chunks, para, scale):
        out = []
        for i, c in enumerate(chunks):
            if i == len(chunks) - 1:
                out.append(None)
            elif c.get("is_last"):
                base = max(sw.trailing_pause_s(c["text"]), para)
                out.append(max(base * scale, 0.02))
            elif c.get("soft_break"):
                out.append(max(0.0 * scale, 0.02))
            else:
                out.append(max(sw.trailing_pause_s(c["text"]) * scale, 0.02))
        return out

    cases = []
    for tname, text in TEXTS.items():
        for max_tokens in (50, 120):
            for scale, para in ((1.0, 0.9), (1.5, 0.5)):
                chunks = full_chunks(text, max_tokens)
                py = gm.compute_pauses_for_gluing(
                    [dict(c) for c in chunks], scale, para)
                ru = rust_pauses(chunks, para, scale)
                divergent = []
                for i, ((pd_, _why), rd_) in enumerate(zip(py, ru)):
                    pd_val = None if pd_ is None else round(pd_, 6)
                    rd_val = None if rd_ is None else round(rd_, 6)
                    if pd_val != rd_val:
                        divergent.append({
                            "i": i,
                            "why": ("конец абзаца + soft_break от terminate: python "
                                    "даёт стык 0.02, rust — max(знак, пауза абзаца)"),
                        })
                cases.append({
                    "name": f"{tname}|mt{max_tokens}|s{scale}|p{para}",
                    "text": text, "max_tokens": max_tokens,
                    "pause_scale": scale, "paragraph_pause_s": para,
                    "chunks": [{"text": c["text"],
                                "is_last": bool(c.get("is_last")),
                                "soft_break": bool(c.get("soft_break"))}
                               for c in chunks],
                    "python_pauses": [None if d is None else round(d, 6) for d, _ in py],
                    "rust_pauses": ru,
                    "divergent": divergent,
                    "fs": FS,
                    "rust_pause_samples": [None if d is None else int(d * FS) for d in ru],
                    "rust_total_pause_s": round(sum(d for d in ru if d), 6),
                })

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(cases, f, ensure_ascii=False, indent=1)
    total_div = sum(len(c["divergent"]) for c in cases)
    print(f"golden pauses: {len(cases)} cases, divergent узлов: {total_div} -> {OUT}")


if __name__ == "__main__":
    main()
