#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Charts for docs/results.md, drawn from bench/results/*.csv (the rows, not hand-typed numbers).

    python3 -I bench/report/plots.py            # writes docs/img/<chart>-{light,dark}.svg

Standard library only. Every chart is static SVG in a light and a dark variant (docs/results.md
picks one with <picture>), and docs/results.md carries each chart's numbers as a table. Rows
are selected by the `notes` column's step= tag and oracle=ok; rows a note marks GATED are left
out and named under the chart. Palette and chrome follow the reference data-viz palette
(categorical slots 1-3, sequential blue); the 2- and 3-series sets pass the palette validator
in both modes.
"""
import csv
import math
import os
import re
import statistics
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
RES = os.path.join(ROOT, "bench", "results")
OUT = os.path.join(ROOT, "docs", "img")

THEMES = {
    "light": dict(surface="#fcfcfb", ink="#0b0b0b", ink2="#52514e", muted="#898781", grid="#e1e0d9",
                  axis="#c3c2b7", series=["#2a78d6", "#eb6834", "#1baf7a"],
                  seq=["#cde2fb", "#9ec5f4", "#6da7ec", "#3987e5", "#256abf", "#184f95", "#0d366b"]),
    "dark": dict(surface="#1a1a19", ink="#f0efec", ink2="#c3c2b7", muted="#898781", grid="#2c2c2a",
                 axis="#383835", series=["#3987e5", "#d95926", "#199e70"],
                 seq=["#0d366b", "#184f95", "#256abf", "#3987e5", "#6da7ec", "#9ec5f4", "#cde2fb"]),
}
FONT = "font-family='system-ui,-apple-system,Segoe UI,Helvetica,Arial,sans-serif'"


def rows(name):
    with open(os.path.join(RES, name)) as f:
        return list(csv.DictReader(line for line in f if not line.startswith("#")))


def tag(r, key):
    m = re.search(rf"\b{key}=(\S+?)[;\s)]", r["notes"] + " ")
    return m.group(1) if m else None


def esc(s):
    return str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


class Svg:
    def __init__(self, w, h, t, title, desc):
        self.t, self.w, self.h = t, w, h
        self.parts = [f"<svg xmlns='http://www.w3.org/2000/svg' width='{w}' height='{h}' viewBox='0 0 {w} {h}' role='img' aria-labelledby='t d'>",
                      f"<title id='t'>{esc(title)}</title><desc id='d'>{esc(desc)}</desc>",
                      f"<rect width='{w}' height='{h}' fill='{t['surface']}' rx='8'/>"]

    def text(self, x, y, s, size=12, fill=None, anchor="start", weight=400):
        self.parts.append(f"<text x='{x:.1f}' y='{y:.1f}' {FONT} font-size='{size}' font-weight='{weight}' fill='{fill or self.t['ink2']}' text-anchor='{anchor}'>{esc(s)}</text>")

    def line(self, x1, y1, x2, y2, stroke, width=1, dash=None):
        d = f" stroke-dasharray='{dash}'" if dash else ""
        self.parts.append(f"<line x1='{x1:.1f}' y1='{y1:.1f}' x2='{x2:.1f}' y2='{y2:.1f}' stroke='{stroke}' stroke-width='{width}'{d}/>")

    def raw(self, s):
        self.parts.append(s)

    def save(self, path):
        with open(path, "w") as f:
            f.write("\n".join(self.parts + ["</svg>"]) + "\n")


def nice_max(v):
    e = 10 ** math.floor(math.log10(v))
    for m in (1, 1.2, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10):
        if m * e >= v:
            return m * e
    return 10 * e


def frame(s, x0, y0, x1, y1, ymax, step, unit):
    t = s.t
    v = 0
    while v <= ymax + 1e-9:
        y = y1 - (y1 - y0) * v / ymax
        s.line(x0, y, x1, y, t["grid"] if v else t["axis"], 1)
        s.text(x0 - 8, y + 4, f"{v:,.0f}", 11, t["muted"], "end")
        v += step
    s.text(x0 - 8, y0 - 14, unit, 11, t["muted"], "end")


# --- scaling: Gb/s against proxy count, one series per session ------------------------------------
def scaling(t, mode):
    sessions = [("paths-g261.csv", "2026-11-01"), ("paths-g2117.csv", "2026-11-09")]
    data, gated = [], []
    for f, label in sessions:
        pts = {}
        for r in rows(f):
            if r["path"] != "E" or tag(r, "step") != "ladder":
                continue
            if r["oracle"] != "ok" or "GATED" in r["notes"]:
                if "GATED" in r["notes"]:
                    gated.append((label, int(r["proxies"])))
                continue
            pts.setdefault(int(r["proxies"]), []).append(float(r["gbps"]))
        data.append((label, pts))
    W, H, x0, y0, x1, y1 = 760, 480, 70, 88, 600, 370
    s = Svg(W, H, t, "adit throughput against proxy count",
            "Gb/s from S3 into 8 H200 GPUs on one p5en.48xlarge, T2 (1,000 x 1 GiB), by number of c8gn.48xlarge proxies; dots are runs, lines join means; dashed is perfect scaling from each session's 1-proxy mean.")
    s.text(24, 30, "Throughput into 8 GPUs vs. proxy count", 16, t["ink"], weight=600)
    s.text(24, 50, "p5en.48xlarge client, T2 (1,000 × 1 GiB, synthetic), 64 objects in flight per proxy · calibration", 12, t["ink2"])
    ks = [1, 2, 4, 8]
    xs = {k: x0 + 30 + (x1 - x0 - 60) * i / 3 for i, k in enumerate(ks)}
    ymax = nice_max(max(max(v) for _, p in data for v in p.values()) * 1.1)
    frame(s, x0, y0, x1, y1, ymax, ymax / 5, "Gb/s")
    for k in ks:
        s.text(xs[k], y1 + 20, f"{k}", 12, t["ink2"], "middle")
    s.text((x0 + x1) / 2, y1 + 38, "proxies (c8gn.48xlarge)", 12, t["muted"], "middle")
    Y = lambda v: y1 - (y1 - y0) * v / ymax
    for i, (label, pts) in enumerate(data):
        c = t["series"][i]
        e1 = statistics.mean(pts[1])
        ideal = ' '.join(f'{xs[k]:.1f},{Y(e1 * k):.1f}' for k in ks)
        s.raw(f"<polyline points='{ideal}' fill='none' stroke='{c}' stroke-width='1.5' stroke-dasharray='4 4' opacity='0.7'/>")
        means = [(xs[k], Y(statistics.mean(pts[k]))) for k in ks if k in pts]
        s.raw(f"<polyline points='{' '.join(f'{x:.1f},{y:.1f}' for x, y in means)}' fill='none' stroke='{c}' stroke-width='2'/>")
        for k in ks:
            for j, v in enumerate(pts.get(k, [])):
                s.raw(f"<circle cx='{xs[k] + (j - 1) * 5 + (i * 2 - 1) * 9:.1f}' cy='{Y(v):.1f}' r='4' fill='{c}' stroke='{t['surface']}' stroke-width='2'/>")
        last = max(k for k in ks if k in pts)
        s.text(xs[last] + 30, Y(statistics.mean(pts[last])) + 4, f"{statistics.mean(pts[last]):,.0f} Gb/s", 12, t["ink"], weight=600)
    lx = x0
    for i, (label, _) in enumerate(data):
        s.raw(f"<circle cx='{lx + 6}' cy='{H - 50}' r='5' fill='{t['series'][i]}'/>")
        s.text(lx + 16, H - 46, f"session {label}", 12, t["ink2"])
        lx += 150
    s.line(lx, H - 50, lx + 22, H - 50, t["muted"], 1.5, "4 4")
    s.text(lx + 28, H - 46, "perfect scaling from that session's 1-proxy mean", 12, t["ink2"])
    s.text(24, H - 26, "Dots are runs (n = 3; n = 2 for the 11-01 E8), lines join means. Proxy counts are spaced by doubling.", 11, t["muted"])
    if gated:
        s.text(24, H - 8, "The 2026-11-09 E8 runs are omitted: a degraded proxy pair gated them (#114).", 11, t["muted"])
    return s, data


# --- ceiling: B against E per interleaved cycle ----------------------------------------------------
def ceiling(t, mode):
    by = {}
    for r in rows("paths-g2117.csv"):
        if tag(r, "step") == "ceiling" and r["oracle"] == "ok":
            by.setdefault(tag(r, "arm"), {})[int(tag(r, "cycle"))] = float(r["gbps"])
    arms = [("E7", "adit, 7 proxies (E7)", 0), ("B512", "KvikIO RemoteFile, best tuned (B)", 1)]
    W, H, x0, y0, x1, y1 = 720, 430, 70, 88, 690, 340
    s = Svg(W, H, t, "adit against the best tuned GPU-host path, per interleaved cycle",
            "Gb/s into 8 GPUs on T2-64 for adit with 7 proxies and KvikIO RemoteFile at 512 threads, 16 MiB tasks, 64 in flight, in three interleaved cycles.")
    s.text(24, 30, "adit vs. the best tuned GPU-host path (same box, interleaved)", 16, t["ink"], weight=600)
    s.text(24, 50, "p5en.48xlarge, T2-64 (64 × 1 GiB, synthetic), 2026-11-09 · calibration", 12, t["ink2"])
    ymax = nice_max(max(v for a in by.values() for v in a.values()) * 1.1)
    frame(s, x0, y0, x1, y1, ymax, ymax / 5, "Gb/s")
    Y = lambda v: y1 - (y1 - y0) * v / ymax
    cycles = sorted(by["E7"])
    gw = (x1 - x0) / len(cycles)
    bw = 46
    for ci, c in enumerate(cycles):
        cx = x0 + gw * (ci + 0.5)
        s.text(cx, y1 + 20, f"cycle {c}", 12, t["ink2"], "middle")
        for j, (a, _, slot) in enumerate(arms):
            v = by[a][c]
            x = cx + (j - 0.5) * (bw + 2) - bw / 2
            h = y1 - Y(v)
            s.raw(f"<path d='M{x:.1f},{y1:.1f} v{-h + 4:.1f} q0,-4 4,-4 h{bw - 8} q4,0 4,4 v{h - 4:.1f} z' fill='{t['series'][slot]}'/>")
            s.text(x + bw / 2, Y(v) - 6, f"{v:,.0f}", 12, t["ink"], "middle", 600)
        s.text(cx, y1 + 38, f"adit / B = {by['E7'][c] / by['B512'][c]:.1f}×", 12, t["ink2"], "middle")
    lx = x0
    for _, label, slot in arms:
        s.raw(f"<rect x='{lx}' y='{H - 32}' width='12' height='12' rx='2' fill='{t['series'][slot]}'/>")
        s.text(lx + 18, H - 22, label, 12, t["ink2"])
        lx += 260
    return s, by


# --- bgrid: path B tuning grid (heatmap) -----------------------------------------------------------
def bgrid(t, mode):
    cells = {}
    for r in rows("paths-g2117.csv"):
        if r["path"] == "B" and tag(r, "step") == "bsweep" and r["oracle"] == "ok":
            c = dict(kv.split("=", 1) for kv in r["config"].split(";") if "=" in kv)
            cells[(int(c["threads"]), (int(c["task_size_mib"]), int(c["concurrency"])))] = float(r["gbps"])
    ths = sorted({k[0] for k in cells})
    cols = sorted({k[1] for k in cells})
    W, H = 720, 120 + 56 * len(ths)
    x0, y0, cw, ch = 130, 92, (690 - 130) / len(cols), 52
    s = Svg(W, H, t, "Path B tuning grid", "KvikIO RemoteFile Gb/s into 8 GPUs on T2-64 by thread count, task size and objects in flight; one cold read per cell.")
    s.text(24, 30, "Path B (KvikIO RemoteFile) tuning grid, Gb/s", 16, t["ink"], weight=600)
    s.text(24, 50, "p5en.48xlarge, T2-64, one cold read per cell (n = 1), 2026-11-09 · client ENA is 200 Gb/s", 12, t["ink2"])
    vmin, vmax = min(cells.values()), max(cells.values())
    for j, (task, conc) in enumerate(cols):
        s.text(x0 + cw * (j + 0.5), y0 - 8, f"{task} MiB · {conc} in flight", 11, t["muted"], "middle")
    for i, th in enumerate(ths):
        y = y0 + ch * i
        s.text(x0 - 10, y + ch / 2 + 4, f"{th} threads", 12, t["ink2"], "end")
        for j, col in enumerate(cols):
            v = cells[(th, col)]
            k = round((v - vmin) / (vmax - vmin) * (len(t["seq"]) - 1))
            fill = t["seq"][k]
            dark_cell = (k >= 4) if mode == "light" else (k <= 2)
            s.raw(f"<rect x='{x0 + cw * j + 1:.1f}' y='{y + 1:.1f}' width='{cw - 2:.1f}' height='{ch - 2}' rx='4' fill='{fill}'/>")
            s.text(x0 + cw * (j + 0.5), y + ch / 2 + 5, f"{v:.1f}", 13, "#ffffff" if dark_cell else "#0b0b0b", "middle", 600)
    return s, cells


# --- cpu: GPU-host CPU-seconds per read -------------------------------------------------------------
def cpu(t, mode):
    vals = {"B512": [], "E7": []}
    for r in rows("paths-g2117.csv"):
        a = tag(r, "arm")
        if tag(r, "step") == "ceiling" and r["oracle"] == "ok" and a in vals:
            vals[a].append(float(r["host_cpu_s"]))
    W, H, x0, x1 = 720, 200, 250, 650
    s = Svg(W, H, t, "GPU-host CPU per 64 GiB read", "Mean GPU-host CPU-seconds per 64 GiB read: KvikIO RemoteFile against adit; adit's counter undercounts (#27).")
    s.text(24, 30, "GPU-host CPU-seconds per 64 GiB read", 16, t["ink"], weight=600)
    s.text(24, 50, "p5en.48xlarge, T2-64, mean of 3 interleaved reads, 2026-11-09 · adit's figure undercounts (#27)", 12, t["ink2"])
    vmax = nice_max(max(statistics.mean(v) for v in vals.values()))
    for i, (a, label, slot) in enumerate([("B512", "KvikIO RemoteFile (B)", 1), ("E7", "adit, 7 proxies (E7)", 0)]):
        y = 80 + i * 56
        m = statistics.mean(vals[a])
        w = max(4, (x1 - x0) * m / vmax)
        s.text(x0 - 12, y + 24, label, 13, t["ink2"], "end")
        s.raw(f"<path d='M{x0},{y + 8} h{w - 4:.1f} q4,0 4,4 v20 q0,4 -4,4 h{-(w - 4):.1f} z' fill='{t['series'][slot]}'/>")
        s.text(x0 + w + 8, y + 26, f"{m:,.1f} s", 13, t["ink"], weight=600)
    s.line(x0, 76, x0, 176, t["axis"], 1)
    return s, {a: statistics.mean(v) for a, v in vals.items()}


def main():
    os.makedirs(OUT, exist_ok=True)
    summary = {}
    for name, fn in [("scaling", scaling), ("ceiling", ceiling), ("bgrid", bgrid), ("cpu", cpu)]:
        for mode, t in THEMES.items():
            s, data = fn(t, mode)
            s.save(os.path.join(OUT, f"{name}-{mode}.svg"))
        summary[name] = data
    for name, data in summary.items():
        print(name, data if not isinstance(data, list) else [(l, {k: round(statistics.mean(v), 1) for k, v in p.items()}) for l, p in data])


if __name__ == "__main__":
    sys.exit(main())
