# -*- coding: utf-8 -*-
"""График памяти стресс-тестов из CSV bench (--csv). Анализический инструмент
(запускается venv-питоном; рантайм приложения от Python не зависит).

Рисует для каждого CSV: private MB, working set MB и GPU total MB от номера
чанка (steady-фаза), вертикальную линию конца прогрева не нужно — CSV уже
делит фазы. Выход — SVG рядом с CSV + PNG при наличии matplotlib.
"""
import csv
import io
import os
import sys

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", errors="replace")


def read_csv(path):
    rows = []
    with open(path, newline="", encoding="utf-8") as f:
        for r in csv.DictReader(f):
            if r["phase"] == "steady":
                rows.append((int(r["chunk"]), float(r["ws_mb"]),
                             float(r["private_mb"]), float(r["gpu_total_mb"])))
    rows.sort()
    return rows


def svg_plot(series, out_path, title):
    # series: {label: [(x, y)]}
    colors = ["#1f77b4", "#d62728", "#2ca02c", "#9467bd", "#8c564b"]
    allpts = [p for pts in series.values() for p in pts]
    if not allpts:
        print("нет точек"); return
    x0, x1 = min(p[0] for p in allpts), max(p[0] for p in allpts)
    y0 = 0.0
    y1 = max(p[1] for p in allpts) * 1.08
    W, H, ML, MB = 860, 480, 70, 50
    def sx(x): return ML + (x - x0) / max(x1 - x0, 1) * (W - ML - 20)
    def sy(y): return H - MB - (y - y0) / (y1 - y0) * (H - MB - 20)
    parts = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" font-family="Segoe UI, sans-serif" font-size="12">']
    parts.append(f'<text x="{ML}" y="24" font-size="15" font-weight="bold">{title}</text>')
    # сетка + ось Y
    for gy in range(0, int(y1) + 1, max(1, int(y1 // 6))):
        parts.append(f'<line x1="{ML}" y1="{sy(gy):.1f}" x2="{W-20}" y2="{sy(gy):.1f}" stroke="#dddddd"/>')
        parts.append(f'<text x="{ML-6}" y="{sy(gy)+4:.1f}" text-anchor="end">{gy}</text>')
    parts.append(f'<text x="{ML-50}" y="{(H-MB)//2}" transform="rotate(-90 {ML-50} {(H-MB)//2})" text-anchor="middle">МБ</text>')
    parts.append(f'<line x1="{ML}" y1="{H-MB}" x2="{W-20}" y2="{H-MB}" stroke="#333"/>')
    for i in range(0, x1 - x0 + 1, max(1, (x1 - x0) // 8)):
        parts.append(f'<text x="{sx(x0+i):.0f}" y="{H-MB+18}" text-anchor="middle">{x0+i}</text>')
    parts.append(f'<text x="{(W-20+ML)//2}" y="{H-8}" text-anchor="middle">чанк</text>')
    for (label, pts), color in zip(series.items(), colors):
        d = " ".join(f"{sx(x):.1f},{sy(y):.1f}" for x, y in pts)
        parts.append(f'<polyline points="{d}" fill="none" stroke="{color}" stroke-width="1.6"/>')
        lx, ly = pts[-1]
        parts.append(f'<text x="{sx(lx)-6:.0f}" y="{sy(ly)-6:.1f}" text-anchor="end" fill="{color}">{label}</text>')
    parts.append("</svg>")
    with open(out_path, "w", encoding="utf-8") as f:
        f.write("\n".join(parts))
    print("svg:", out_path)


def main():
    base = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "out")
    runs = {}
    for name in sys.argv[1:]:
        p = os.path.join(base, name) if not os.path.isfile(name) else name
        runs[name.replace(".csv", "")] = read_csv(p)
    if not runs:
        print("usage: plot_memory.py stress_cuda_slow.csv [ещё.csv ...]"); return
    # один график: private по всем прогонам
    series = {k: [(x, priv) for x, ws, priv, gpu in v] for k, v in runs.items()}
    svg_plot(series, os.path.join(base, "memory_private.svg"), "Стресс 1000 чанков: private bytes")
    series = {k: [(x, ws) for x, ws, priv, gpu in v] for k, v in runs.items()}
    svg_plot(series, os.path.join(base, "memory_ws.svg"), "Стресс 1000 чанков: working set")
    series = {k: [(x, gpu) for x, ws, priv, gpu in v] for k, v in runs.items()}
    svg_plot(series, os.path.join(base, "memory_gpu.svg"), "Стресс 1000 чанков: GPU total (МБ)")
    # png-вариант, если есть matplotlib
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        fig, ax = plt.subplots(figsize=(10, 5.5))
        for k, v in runs.items():
            ax.plot([x for x, *_ in v], [priv for _, _, priv, _ in v], label=f"{k} private")
            ax.plot([x for x, *_ in v], [ws for _, ws, *_ in v], label=f"{k} WS", alpha=0.6)
        ax.set_xlabel("чанк"); ax.set_ylabel("МБ"); ax.legend(); ax.grid(alpha=0.3)
        ax.set_title("Стресс 1000 чанков: память")
        out = os.path.join(base, "memory.png")
        fig.tight_layout(); fig.savefig(out, dpi=120)
        print("png:", out)
    except ImportError:
        print("matplotlib недоступен — только SVG")


if __name__ == "__main__":
    main()
