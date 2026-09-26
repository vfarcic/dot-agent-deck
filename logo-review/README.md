# Logo review material (temporary — issue #746)

This folder exists only so the logo candidates can be reviewed on the draft PR. It is deleted before the PR leaves draft; the chosen mark lands in its real locations (`site/static/img/`, `desktop/src-tauri/icons/`, `desktop/src/`) with a regeneration step documented under `docs/develop/`.

- `0-gpt-original.png` — the GPT-generated reference image (two variations: full colour, and one colour on a black tile).
- `1-first-concepts.png` — the three concepts drawn first (A deck, B grid, C streams), before the GPT reference.
- `2-redraw-round1.png` — the GPT reference redrawn as clean vector: straight, tilted, no-dots, and the one-colour tile.
- `3-redraw-round2.png` — fixes: front window lifted off near-black, app-icon tile candidates (ink with rim, light), and a simplified 16px variant.
- `4-faithful-tilt.png` — the reference redrawn faithfully: a trapezoid front window (vertical sides, flat bottom, sloped top) with two back windows fanning out, plus its icon tiles and a tilted 16px variant.
- `svg/` — every candidate as SVG, plus `gen.py` (the straight redraw variants) and `gen_tilt.py` (the faithful tilted ones); both take an output directory.

Each sheet shows every candidate at 192px, 32px, 16px and 16px zoomed 10× nearest-neighbour, on the docs site's light (`#ffffff`) and dark (`#0a0e12`) backgrounds.
