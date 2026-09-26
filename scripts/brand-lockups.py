#!/usr/bin/env python3
"""Build the Agent Deck lockups (symbol + wordmark) from the master symbol.

    python3 scripts/brand-lockups.py <path/to/InterVariable.ttf>

Writes assets/brand/lockup-light.svg and assets/brand/lockup-dark.svg. The
wordmark is set in Inter Bold and converted to outlines, so the SVGs render
the same everywhere without the font installed. Needs fontTools and
uharfbuzz; docs/develop/brand-assets.md has a one-line way to get both.
Only needed when the symbol or the wordmark changes.
"""
import io
import re
import sys
from pathlib import Path

import uharfbuzz as hb
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

ROOT = Path(__file__).resolve().parent.parent
BRAND = ROOT / "assets" / "brand"
WORDMARK = "Agent Deck"
SIZE = 290          # font size, in symbol units (the symbol is 512 tall)
TRACKING = -0.02    # em; Inter reads better slightly tight at display sizes
GAP = 72            # between the symbol and the wordmark
INK = {"light": "#0e1418", "dark": "#edf2f6"}


def wordmark(font_path):
    var = TTFont(font_path)
    bold = instantiateVariableFont(var, {"wght": 700, "opsz": 32} if "opsz" in [a.axisTag for a in var["fvar"].axes] else {"wght": 700})
    buf = io.BytesIO()
    bold.save(buf)
    face = hb.Face(buf.getvalue())
    font = hb.Font(face)
    upem = face.upem
    text = hb.Buffer()
    text.add_str(WORDMARK)
    text.guess_segment_properties()
    hb.shape(font, text, {"kern": True, "liga": True})

    glyphs = bold.getGlyphSet()
    order = bold.getGlyphOrder()
    scale = SIZE / upem
    cap = bold["OS/2"].sCapHeight * scale
    baseline = 256 + cap / 2          # centre the cap height on the symbol
    x, paths = 512 + GAP, []
    for info, pos in zip(text.glyph_infos, text.glyph_positions):
        pen = SVGPathPen(glyphs)
        glyphs[order[info.codepoint]].draw(
            TransformPen(pen, (scale, 0, 0, -scale, x + pos.x_offset * scale, baseline - pos.y_offset * scale)))
        d = pen.getCommands()
        if d:
            paths.append(d)
        x += pos.x_advance * scale + TRACKING * SIZE
    return " ".join(paths), x - TRACKING * SIZE


def main():
    symbol = (BRAND / "logo.svg").read_text()
    inner = re.search(r"<svg[^>]*>(.*)</svg>", symbol, re.S).group(1).strip()
    d, width = wordmark(sys.argv[1])
    width = round(width + 8)
    for theme, ink in INK.items():
        (BRAND / f"lockup-{theme}.svg").write_text(
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} 512">\n'
            f"  <title>Agent Deck</title>\n  {inner}\n"
            f'  <path fill="{ink}" d="{d}"/>\n</svg>\n')
    print(f"wrote lockup-light.svg and lockup-dark.svg ({width}x512)")


if __name__ == "__main__":
    main()
