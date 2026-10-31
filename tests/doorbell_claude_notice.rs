//! `inbox/doorbell/036`: Claude Code's notice above the main composer, told
//! from a suggestion list by its measured shape (`src/doorbell_screen.rs`).
//! Every near-miss is derived from a real recorded frame, so only the row
//! under test differs from a screen Claude Code actually drew.
use std::path::PathBuf;

use dot_agent_deck::doorbell_screen::{CLAUDE_PROFILE, Grid, GridCell};
use spec::spec;

/// The grid of the recorded `stem` at its named `point`. The menu captures
/// have no resize, so the prefix parses at the recorded size.
fn frame(stem: &str, point: &str) -> Grid {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-inbox/r5wake");
    let bytes = std::fs::read(base.join(format!("{stem}.bin"))).expect("fixture bytes");
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(base.join(format!("{stem}.json"))).expect("index"))
            .expect("index json");
    assert!(
        index["edges"].as_array().unwrap().is_empty(),
        "{stem} resizes"
    );
    let upto = index["points"][point].as_u64().unwrap() as usize;
    Grid::parse(
        &bytes[..upto],
        index["rows"].as_u64().unwrap() as u16,
        index["cols"].as_u64().unwrap() as u16,
    )
}

/// The row of the main composer's upper rule.
fn upper_rule(grid: &Grid) -> u16 {
    (1..grid.rows)
        .rev()
        .find(|&r| {
            grid.cell(r, 0).is_some_and(|c| c.text == "❯")
                && grid.cells[r as usize - 1].iter().all(|c| c.text == "─")
        })
        .expect("a main composer")
        - 1
}

fn blank() -> GridCell {
    GridCell {
        text: String::new(),
        bold: false,
        dim: false,
        inverse: false,
        fg: vt100::Color::Default,
        bg: vt100::Color::Default,
    }
}

/// Row `row` cleared, then `text` drawn from column `start` in `style`.
fn draw(grid: &mut Grid, row: u16, start: usize, text: &str, style: &GridCell) {
    let cols = usize::from(grid.cols);
    let cells = &mut grid.cells[row as usize];
    *cells = vec![blank(); cols];
    for (i, ch) in text.chars().enumerate() {
        cells[start + i] = GridCell {
            text: if ch == ' ' {
                String::new()
            } else {
                ch.to_string()
            },
            ..style.clone()
        };
    }
}

/// The column that ends `text` two cells short of the right edge.
fn right_aligned(grid: &Grid, text: &str) -> usize {
    usize::from(grid.cols) - 2 - text.chars().count()
}

/// Scenario: Take Claude Code 2.1.310's and 2.1.312's recorded 24×80 and 40×120 main composers under the quota notice, which the profile accepts, and redraw only the row above the upper rule into near-misses: the notice moved off its two-cell right inset, a list entry or wrapped description ending at that same column, a right-aligned row with a list's column gap, a notice part-coloured or emboldened, a notice with list rows above it, a list row under a notice, and a left-indented entry ending at the inset. On the empty composer, whose effort notice starts in the right half, redraw that row as a right-aligned list entry, and the notice with a second colour or a column gap. Every near-miss is refused; the unchanged frames, a blank row and a single run starting one column right of the list indent stay ready.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_a_notice_is_told_from_a_list_by_its_shape() {
    let mut wrong = Vec::new();
    for release in ["2.1.310", "2.1.312"] {
        for size in ["24x80", "40x120"] {
            let stem = format!("claude-{release}-menus-{size}");
            let real = frame(&stem, "transcript-quotation");
            let upper = upper_rule(&real);
            let notice_row = upper - 1;
            let row = &real.cells[notice_row as usize];
            let first = row.iter().position(|c| !c.blank()).unwrap();
            let last = row.iter().rposition(|c| !c.blank()).unwrap();
            assert!(
                first < usize::from(real.cols / 2) && last + 3 == usize::from(real.cols),
                "{stem}: the recorded notice starts left of the midpoint and ends two cells short"
            );
            let notice: String = row[first..=last].iter().map(GridCell::shown).collect();
            let style = row[first].clone();
            let mut other = style.clone();
            other.fg = vt100::Color::Idx(160);
            // The slash list's own rows at this size, as Claude Code drew them.
            let slash = frame(&stem, "slash");
            let slash_upper = upper_rule(&slash);
            let list_rows: Vec<_> = (slash_upper - 3..slash_upper)
                .map(|r| slash.cells[r as usize].clone())
                .collect();

            let mut cases: Vec<(&str, bool, Grid)> = Vec::new();
            cases.push(("the recorded notice", true, real.clone()));
            let mut g = real.clone();
            g.cells[notice_row as usize] = vec![blank(); usize::from(g.cols)];
            cases.push(("a blank row", true, g));

            let mut g = real.clone();
            draw(&mut g, notice_row, first + 1, &notice, &style);
            cases.push(("the notice one cell further right", false, g));
            let mut g = real.clone();
            draw(&mut g, notice_row, first - 1, &notice, &style);
            cases.push(("the notice one cell further left", false, g));

            let entry = "/advisor                        Let Claude consult a stronger model";
            let mut g = real.clone();
            let at = right_aligned(&g, entry);
            draw(&mut g, notice_row, at, entry, &style);
            cases.push(("a right-aligned row with a column gap", false, g));

            let long_entry = format!("+ {}", "a".repeat(usize::from(real.cols) - 2 - 4 - 2));
            let mut g = real.clone();
            draw(&mut g, notice_row, 4, &long_entry, &style);
            cases.push((
                "a file entry at the list indent ending at the inset",
                false,
                g,
            ));

            let continuation = "w".repeat(usize::from(real.cols) - 2 - 36);
            let mut g = real.clone();
            draw(&mut g, notice_row, 36, &continuation, &style);
            g.cells[notice_row as usize - 1] = list_rows[1].clone();
            cases.push((
                "a wrapped description ending at the inset under its entry",
                false,
                g,
            ));

            let mut g = real.clone();
            draw(&mut g, notice_row, first, &notice, &style);
            g.cells[notice_row as usize][last].fg = other.fg;
            cases.push(("the notice with a second colour", false, g));
            let mut g = real.clone();
            draw(&mut g, notice_row, first, &notice, &style);
            g.cells[notice_row as usize][first].bold = !style.bold;
            cases.push(("the notice with a bold character", false, g));
            let mut g = real.clone();
            draw(&mut g, notice_row, first, &notice, &style);
            g.cells[notice_row as usize][first].bg = vt100::Color::Idx(237);
            cases.push(("the notice with a background", false, g));

            let mut g = real.clone();
            for (i, list_row) in list_rows.iter().enumerate() {
                g.cells[notice_row as usize - 3 + i] = list_row.clone();
            }
            cases.push(("the notice with list rows above it", false, g));

            let mut g = real.clone();
            g.cells[notice_row as usize - 1] = real.cells[notice_row as usize].clone();
            g.cells[notice_row as usize] = list_rows[2].clone();
            cases.push(("a list row under the notice", false, g));

            let mut g = real.clone();
            g.cells[notice_row as usize] = list_rows[2].clone();
            cases.push(("the slash list's last row", false, g));

            // The indent boundary: one column right of where a list entry
            // can start, the same run is a notice.
            let mut g = real.clone();
            let run = "a".repeat(usize::from(real.cols) - 2 - 5);
            draw(&mut g, notice_row, 5, &run, &style);
            cases.push(("a notice starting right of the list indent", true, g));

            // The effort notice on the empty composer starts in the right
            // half; a list-shaped row there is refused too.
            let empty = frame(&stem, "empty");
            let effort_row = upper_rule(&empty) - 1;
            let row = &empty.cells[effort_row as usize];
            let effort_first = row.iter().position(|c| !c.blank()).unwrap();
            assert!(effort_first >= usize::from(empty.cols / 2), "{stem}");
            let effort: String = row[effort_first..]
                .iter()
                .map(GridCell::shown)
                .collect::<String>()
                .trim_end()
                .to_string();
            let effort_style = row[effort_first].clone();
            cases.push(("the effort notice", true, empty.clone()));
            let mut g = empty.clone();
            let short_entry = "/add-dir    Add a directory";
            let at = right_aligned(&g, short_entry);
            draw(&mut g, effort_row, at, short_entry, &effort_style);
            cases.push(("a right-half row with a column gap", false, g));
            let mut g = empty.clone();
            g.cells[effort_row as usize][effort_first].fg = other.fg;
            cases.push(("the effort notice with a second colour", false, g));
            let mut g = empty.clone();
            let split = effort.replacen(' ', "  ", 1);
            let at = right_aligned(&g, &split);
            draw(&mut g, effort_row, at, &split, &effort_style);
            cases.push(("the effort notice with a column gap", false, g));

            for (name, expected, grid) in cases {
                let ready = (CLAUDE_PROFILE.ready)(&grid);
                let empty_and_ready = CLAUDE_PROFILE.empty_and_ready(&grid);
                eprintln!("Claude {release} {size} {name}: ready={ready}, expected {expected}");
                if ready != expected || empty_and_ready != expected {
                    wrong.push(format!(
                        "Claude {release} {size} {name}: ready={ready}, empty_and_ready={empty_and_ready}, expected {expected}\n{}",
                        (0..grid.rows)
                            .map(|r| grid.text_from(r, 0))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "the notice must be told from a list by its shape:\n{}",
        wrong.join("\n\n")
    );
}
