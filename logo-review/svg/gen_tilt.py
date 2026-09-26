"""Faithful redraw of the GPT reference: a trapezoid front window (vertical
sides, flat bottom, sloped top) and two back windows fanning out behind it.
Coordinates are traced from the reference, then fitted into a 512 box.

    python3 gen_tilt.py <out-dir>
"""
import math, sys

INK, BODY, TITLE, TEAL, VIOLET, WHITE = "#0e1418", "#18232c", "#0a7c73", "#4ecdc4", "#6248d6", "#ffffff"

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

def lerp_y(p, q, x):
    return p[1] + (q[1] - p[1]) * (x - p[0]) / (q[0] - p[0])

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

def full(dots=True):
    s = []
    s += window((20, 68), (358, 22), (380, 330), (52, 362), VIOLET, 34,
                [(60, 80), (100, 75), (140, 70)] if dots else [], 14)
    s += window((65, 118), (413, 72), (430, 390), (88, 410), TEAL, 34,
                [(106, 135), (146, 130), (186, 125)] if dots else [], 14)
    s += window((120, 168), (503, 118), (503, 461), (120, 461), TITLE, 38,
                [(162, 193), (208, 187), (254, 181)] if dots else [], 16, body=BODY, band=72)
    s.append(f'<path d="M212 276l76 54-76 54" fill="none" stroke="{WHITE}" stroke-width="36" stroke-linecap="round" stroke-linejoin="round"/>')
    s.append(f'<path d="M322 389h92" fill="none" stroke="{WHITE}" stroke-width="30" stroke-linecap="round"/>')
    return s, (20, 20, 503, 461)

def small():
    """16-32px variant: two windows, no dots, oversized prompt."""
    s = []
    s += window((20, 60), (380, 12), (400, 330), (48, 370), VIOLET, 44, [], 0)
    s += window((110, 150), (503, 98), (503, 461), (110, 461), TITLE, 48, [], 0, body=BODY, band=78)
    s.append(f'<path d="M198 272l96 70-96 70" fill="none" stroke="{WHITE}" stroke-width="56" stroke-linecap="round" stroke-linejoin="round"/>')
    s.append(f'<path d="M338 404h104" fill="none" stroke="{WHITE}" stroke-width="48" stroke-linecap="round"/>')
    return s, (20, 12, 503, 461)

def svg(parts, box, fit=440, tile=None, rim=None):
    x0, y0, x1, y1 = box
    k = fit / max(x1 - x0, y1 - y0)
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    t = ""
    if tile:
        t = f'<rect width="512" height="512" rx="112" fill="{tile}"/>\n  '
        if rim:
            t += f'<rect x="5" y="5" width="502" height="502" rx="107" fill="none" stroke="{rim}" stroke-width="10"/>\n  '
    body = "\n    ".join(parts)
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">\n  {t}'
            f'<g transform="translate(256 256) scale({k:.4f}) translate({-cx} {-cy})">\n    {body}\n  </g>\n</svg>\n')

out = sys.argv[1]
f, fb = full(); s, sb = small()
open(f"{out}/tilt.svg", "w").write(svg(f, fb, fit=470))
open(f"{out}/tilt-nodots.svg", "w").write(svg(full(False)[0], fb, fit=470))
open(f"{out}/tilt-icon-light.svg", "w").write(svg(f, fb, fit=370, tile="#f5f7f9"))
open(f"{out}/tilt-icon-ink.svg", "w").write(svg(f, fb, fit=370, tile=INK, rim="#2c3a46"))
open(f"{out}/tilt-fav16.svg", "w").write(svg(s, sb, fit=500))
