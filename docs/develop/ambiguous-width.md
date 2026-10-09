# Ambiguous-width characters

**Policy (maintainer decision, 2026-10-08, issue #359): the deck assumes that East-Asian-ambiguous-width characters are one column wide. Terminals set to draw them two columns wide are unsupported.**

## What "ambiguous" means here

Unicode's East Asian Width property ([UAX #11](https://www.unicode.org/reports/tr11/)) classifies some characters as **Ambiguous** (`A`): they existed in both the legacy East Asian encodings, where they were drawn two columns wide, and the Western ones, where they were one column. Unicode leaves the choice to the renderer, and many terminals expose it as a setting — iTerm2's "Ambiguous characters are double-width", GNOME Terminal's "Ambiguous-width characters" under a profile's Compatibility tab, and equivalents elsewhere. It is off in typical Western setups and often switched on by people who read CJK text regularly.

The `unicode-width` crate gives both answers: `UnicodeWidthStr::width()` / `UnicodeWidthChar::width()` count an ambiguous character as **1**, and `width_cjk()` counts it as **2**.

## The policy

- **Width is measured with `width()`, not `width_cjk()`.** That is what the TUI does today (`git grep -n width_cjk -- src` is empty), and it is the column count ratatui itself lays out with, so our measurements and ratatui's agree.
- **New code need not avoid ambiguous characters.** Using `·`, `…`, `—` or a box-drawing glyph in a width-constrained region is fine; measure it with `width()` like everything else.
- **A report of misalignment under the double-width setting is answered with the troubleshooting note**, not a fix: turn the setting off. The user-facing entry is in [`docs/troubleshooting.md`](../troubleshooting.md) under "Panes and the dashboard".

This covers what the TUI draws into a terminal. The desktop app draws its own cards and borders and does not read a terminal's ambiguous-width setting.

## Why not support the double-width setting

Issue #359 was opened about the `·` separators in card titles and the card's bottom-border stats label, and offered three policies: avoid ambiguous characters in width-constrained regions, treat them as wide everywhere, or make it configurable. Re-checked against `main`, `·` is not the only ambiguous character the deck draws. Each of these is class `A` too (verify with `python3 -c "import unicodedata as u; print([u.east_asian_width(c) for c in '·…—─│┌┐└┘'])"`):

- `…`, which truncation appends (`truncate_to_cap` in `src/tab_layout.rs`, `truncate_path_head` in `src/ui.rs`) and budgets one column for;
- `—`, used in status and hint messages;
- the box-drawing glyphs ratatui draws card and pane borders with (`─ │ ┌ ┐ └ ┘`).

So on a terminal with the setting on, the borders themselves misalign. Replacing the separators alone would fix nothing a user could see, and full support would mean reworking border and truncation layout so that ratatui only ever receives strings whose width we have fixed ourselves. Nobody has reported hitting this, so that cost was judged not worth paying. Revisiting the decision means taking on that rework, not swapping a few characters.
