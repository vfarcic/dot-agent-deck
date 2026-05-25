//! Equal-cap tab-label truncation for the top tab bar.
//!
//! The `Tabs` widget renders each label as `" {label} "` and separates
//! adjacent tabs with a single `│` divider. When the rendered total
//! exceeds the available width the widget clips trailing tabs off the
//! right edge — losing click-to-switch and at-a-glance awareness.
//!
//! [`fit_tab_labels`] applies an equal-cap strategy *before* the labels
//! reach the widget: each label longer than `cap` is truncated with a
//! trailing `…`, where `cap = floor((available_width - overhead) / n)`
//! and `overhead = 2*n + (n - 1)` cells for padding and dividers.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate `labels` so the rendered tab bar fits in `available_width` cells.
///
/// Returns labels unchanged when the full set fits. Otherwise every label
/// whose width exceeds the equal cap is truncated with a trailing `…` such
/// that its rendered width (including the ellipsis) is ≤ `cap`.
pub fn fit_tab_labels(labels: &[String], available_width: u16) -> Vec<String> {
    let n = labels.len();
    if n == 0 {
        return Vec::new();
    }

    let overhead = tab_bar_overhead(n);
    let total = labels_total_width(labels) + overhead;
    let avail = available_width as u32;

    if total <= avail {
        return labels.to_vec();
    }

    let usable = avail.saturating_sub(overhead);
    let cap = (usable / n as u32) as usize;

    labels
        .iter()
        .map(|l| {
            let w = UnicodeWidthStr::width(l.as_str());
            if w <= cap {
                l.clone()
            } else {
                truncate_to_cap(l, cap)
            }
        })
        .collect()
}

/// Per-tab padding (`" {l} "` = 2 cells) plus `n - 1` divider cells.
fn tab_bar_overhead(n: usize) -> u32 {
    2 * n as u32 + n.saturating_sub(1) as u32
}

fn labels_total_width(labels: &[String]) -> u32 {
    labels
        .iter()
        .map(|l| UnicodeWidthStr::width(l.as_str()) as u32)
        .sum()
}

/// Truncate `s` so its rendered width including the trailing `…` is ≤ `cap`.
///
/// `cap == 0` collapses to the empty string — even an ellipsis would
/// overflow. Otherwise we reserve 1 cell for `…` and greedily consume
/// characters while their cumulative width stays within `cap - 1`.
fn truncate_to_cap(s: &str, cap: usize) -> String {
    if cap == 0 {
        return String::new();
    }
    let budget = cap - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduce the rendered total width of a tab bar from a label slice,
    /// mirroring exactly what `Tabs` draws: `" {l} "` per tab + `│` dividers.
    fn rendered_total_width(labels: &[String]) -> u32 {
        labels_total_width(labels) + tab_bar_overhead(labels.len())
    }

    #[test]
    fn all_fit_returns_labels_unchanged() {
        let labels = vec!["Dashboard".to_string(), "Modes".to_string()];
        // " Dashboard " + "│" + " Modes " = 11 + 1 + 7 = 19
        let out = fit_tab_labels(&labels, 80);
        assert_eq!(out, labels);
    }

    #[test]
    fn single_overflow_truncates_only_long_tab() {
        let labels = vec![
            "Dashboard".to_string(),
            "dot-agent-deck-prd-112-truncate-tab-names-to-fit-screen".to_string(),
        ];
        // n=2, available=30: overhead = 2*2 + 1 = 5; usable = 25; cap = 12.
        let out = fit_tab_labels(&labels, 30);
        assert_eq!(out[0], "Dashboard"); // 9 ≤ 12, unchanged
        assert!(UnicodeWidthStr::width(out[1].as_str()) <= 12);
        assert!(out[1].ends_with('…'));
        // cap=12 ⇒ 11 ASCII chars + '…' = 12 cells total.
        assert_eq!(out[1], "dot-agent-d…");
    }

    #[test]
    fn all_overflow_caps_every_label() {
        let labels = vec![
            "aaaaaaaaaaaaaaaa".to_string(), // 16
            "bbbbbbbbbbbbbbbb".to_string(), // 16
            "cccccccccccccccc".to_string(), // 16
        ];
        // n=3, available=20: overhead = 6 + 2 = 8; usable = 12; cap = 4.
        let out = fit_tab_labels(&labels, 20);
        for l in &out {
            assert!(UnicodeWidthStr::width(l.as_str()) <= 4, "label = {l:?}");
            assert!(l.ends_with('…'));
        }
        assert_eq!(out[0], "aaa…");
    }

    #[test]
    fn unicode_label_width_uses_cells_not_bytes() {
        // CJK chars are 2 cells each. "你好世界" is 8 cells / 12 bytes.
        let labels = vec!["你好世界你好".to_string(), "Dashboard".to_string()];
        // Full width: 12 + 9 + overhead(2)=5 = 26. Fits in 80.
        assert_eq!(fit_tab_labels(&labels, 80)[0], "你好世界你好");

        // Force truncation: available=15, n=2 → usable=10, cap=5.
        let out = fit_tab_labels(&labels, 15);
        // "你好世界你好" (12 cells) > 5 → truncated.
        // Budget for chars = cap - 1 = 4 cells. "你好" = 4 cells, "你好世" = 6
        // (overshoots). Result is "你好…" (5 cells).
        assert_eq!(out[0], "你好…");
        assert_eq!(UnicodeWidthStr::width(out[0].as_str()), 5);
        // "Dashboard" (9) > 5 → truncated.
        assert!(out[1].ends_with('…'));
        assert!(UnicodeWidthStr::width(out[1].as_str()) <= 5);
    }

    #[test]
    fn single_tab_truncates_or_passes_through() {
        // Fits: available=20, n=1 → overhead=2, usable=18, full width 11. Return as-is.
        let labels = vec!["Dashboard!!".to_string()];
        assert_eq!(fit_tab_labels(&labels, 20), labels);

        // Overflows: available=8, n=1 → overhead=2, usable=6, cap=6.
        let out = fit_tab_labels(&labels, 8);
        assert_eq!(out[0], "Dashb…");
        assert_eq!(UnicodeWidthStr::width(out[0].as_str()), 6);
    }

    #[test]
    fn zero_width_collapses_to_empty_when_cap_is_zero() {
        // available=1, n=2: overhead=5, saturating_sub → usable=0, cap=0.
        // Truncated labels become empty strings (no ellipsis fits).
        let labels = vec!["foo".to_string(), "bar".to_string()];
        let out = fit_tab_labels(&labels, 1);
        assert_eq!(out, vec![String::new(), String::new()]);

        // Empty input is also a no-op.
        let none: Vec<String> = Vec::new();
        assert!(fit_tab_labels(&none, 80).is_empty());
    }

    #[test]
    fn rendered_total_matches_padding_plus_dividers_formula() {
        // Mirror the exact rendering rule: each label is wrapped in
        // " {l} " (2 extra cells) and adjacent tabs are joined by "│"
        // (1 cell). This pins the helper's internal formula against the
        // shape of the data we pass to `Tabs`.
        let labels = vec!["A".to_string(), "BB".to_string(), "CCC".to_string()];
        // Per-label rendered: 3, 4, 5 cells. Dividers: 2 cells. Total: 14.
        let composed = format!(" {} │ {} │ {} ", labels[0], labels[1], labels[2]);
        let composed_width = UnicodeWidthStr::width(composed.as_str()) as u32;
        assert_eq!(rendered_total_width(&labels), composed_width);
        assert_eq!(rendered_total_width(&labels), 14);

        // And when fitted in a width that comfortably exceeds the total,
        // every label survives unchanged.
        let fitted = fit_tab_labels(&labels, composed_width as u16);
        assert_eq!(fitted, labels);
    }

    #[test]
    fn truncated_total_stays_within_available_width() {
        let labels = vec![
            "Dashboard".to_string(),
            "very-long-orchestration-tab-name-here".to_string(),
            "another-long-one".to_string(),
            "x".to_string(),
        ];
        let available = 40u16;
        let out = fit_tab_labels(&labels, available);
        assert!(rendered_total_width(&out) <= available as u32);
    }
}
