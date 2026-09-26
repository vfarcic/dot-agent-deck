#!/usr/bin/env python3
"""Draw the Agent Deck master symbol, assets/brand/logo.svg.

    python3 scripts/brand-logo.py

A stack of three terminal windows traced from the reference image in
assets/brand/reference/gpt-original.png: a trapezoid front window (vertical
sides, flat bottom, only the top edge sloped) with two back windows fanning
out behind it, fitted into a 512 box. The front window's body is lifted from
ink to BODY so its edge survives on a dark background. Standard library only.
After running it, run scripts/brand-icons.sh and scripts/brand-lockups.py to
refresh every derived copy (docs/develop/brand-assets.md).
"""
import math
from pathlib import Path

BODY, TITLE, TEAL, VIOLET, WHITE = "#18232c", "#0a7c73", "#4ecdc4", "#6248d6", "#ffffff"

def rounded(pts, r):
    """Closed path through quad corners, each corner rounded by r."""
    n, d = len(pts), []
    for i in range(n):
        p, a, b = pts[i], pts[i - 1], pts[(i + 1) % n]
        def toward(q):
            dx, dy = q[0] - p[0], q[1] - p[1]; L = math.hypot(dx, dy)
            return (p[0] + dx / L * r, p[1] + dy / L * r)
        s, e = toward(a), toward(b)
        d.append(f"{'M' if i == 0 else 'L'}{s[0]:.1f} {s[1]:.1f}Q{p[0]} {p[1]} {e[0]:.1f} {e[1]:.1f}")
    return "".join(d) + "Z"

def window(tl, tr, br, bl, top, r, dots, dot_r, body=None, band=0):
    out = [f'<path d="{rounded([tl, tr, br, bl], r)}" fill="{top}"/>']
    if body:
        # body = the part below a band line parallel to the sloped top edge
        l, rr = (tl[0], tl[1] + band), (tr[0], tr[1] + band)
        out.append(f'<path d="{rounded([l, rr, br, bl], r)}" fill="{body}"/>')
        # square off the body's top corners so it meets the title band flush
        out.append(f'<path d="M{l[0]} {l[1]}L{rr[0]} {rr[1]}L{rr[0]} {rr[1]+r}L{l[0]} {l[1]+r}Z" fill="{body}"/>')
    for (x, y) in dots:
        out.append(f'<circle cx="{x}" cy="{y}" r="{dot_r}" fill="{WHITE}"/>')
    return out

def full():
    s = []
    s += window((20, 68), (358, 22), (380, 330), (52, 362), VIOLET, 34,
                [(60, 80), (100, 75), (140, 70)], 14)
    s += window((65, 118), (413, 72), (430, 390), (88, 410), TEAL, 34,
                [(106, 135), (146, 130), (186, 125)], 14)
    s += window((120, 168), (503, 118), (503, 461), (120, 461), TITLE, 38,
                [(162, 193), (208, 187), (254, 181)], 16, body=BODY, band=72)
    s.append(f'<path d="M212 276l76 54-76 54" fill="none" stroke="{WHITE}" stroke-width="36" stroke-linecap="round" stroke-linejoin="round"/>')
    s.append(f'<path d="M322 389h92" fill="none" stroke="{WHITE}" stroke-width="30" stroke-linecap="round"/>')
    return s, (20, 20, 503, 461)


def svg(parts, box, fit):
    x0, y0, x1, y1 = box
    k = fit / max(x1 - x0, y1 - y0)
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    body = "\n    ".join(parts)
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">\n  '
            f'<g transform="translate(256 256) scale({k:.4f}) translate({-cx} {-cy})">\n    {body}\n  </g>\n</svg>\n')

out = Path(__file__).resolve().parent.parent / "assets" / "brand" / "logo.svg"
parts, box = full()
out.write_text(svg(parts, box, fit=470))
print(f"wrote {out}")
