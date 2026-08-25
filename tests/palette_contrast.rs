//! L1 contrast guard for the centralized palette (issue #579).
//!
//! Every other theme test in the suite asserts which *constant* a role resolves
//! to — `theme/palette/001` reads a rendered border cell and compares it to
//! `Color::Green`, and so on. That is the right shape for "the deck card and the
//! embedded pane agree", and it is exactly why nothing caught `STATUS_WAITING`
//! being `Color::Yellow`: a constant compares equal to itself no matter how
//! unreadable it is against the user's terminal background. Yellow measured
//! **1.70:1** against white, and **1.07:1** once the badge's `Modifier::BOLD`
//! was rendered as the bright variant — with the whole suite green, because no
//! test ever put a foreground and a background in the same equation.
//!
//! This file closes that gap by computing WCAG 2.x contrast from whatever the
//! palette currently holds, so the assertion is about *legibility* rather than
//! about a name. A regression to any of the high-luminance slots
//! (yellow/green/cyan/white) fails it in the fast tier.
//!
//! ## The reference model, and why it is shaped this way
//!
//! A TUI never emits pixels: it emits an ANSI slot number, and the terminal
//! resolves it. So a contrast test has to assume *some* resolution. This file
//! uses the de-facto-standard **xterm** palette, which is both the most widely
//! inherited default and the one the issue's own measurements were taken
//! against — `#CDCD00` on white reproduces 1.70:1 and `#FFFF00` on white
//! reproduces 1.07:1 to the reported precision, which is what pins the model to
//! reality rather than to a guess.
//!
//! Two backgrounds and two slot renderings give four pairings, and they are not
//! all equally likely, so they are not held to the same bar:
//!
//! * **Theme-matched** — a terminal ships a background and a palette *together*,
//!   and tunes the palette to that background: a light profile darkens its
//!   colours, a dark profile brightens them. (That is what the bright half of
//!   the sixteen is for, and why `bold`-as-bright was invented on dark
//!   terminals.) So "light terminal" means the base slot on white and "dark
//!   terminal" means the bright slot on black. These two are the ordinary case
//!   and must clear WCAG AA for text, **4.5:1** (SC 1.4.3).
//! * **Mismatched** — the base palette on a black background, or the bright
//!   palette on a white one. This is the configuration behind the issue's
//!   1.07:1 measurement: a light background with a terminal that still brightens
//!   on bold. It is a real configuration but a self-inflicted one, and no single
//!   colour can clear 4.5:1 in every cell (see `palette::STATUS_WAITING` for the
//!   arithmetic — the ceiling for *any* colour is 4.58:1). These are held to
//!   **3:1**, WCAG SC 1.4.11 non-text contrast, which is the criterion that
//!   actually applies to a border glyph and a status chip.
//!
//! Scope: this guards [`palette::STATUS_WAITING`], the role issue #579 is about
//! and the one status the user is *required* to notice. The other hues have the
//! same class of weakness in the light-terminal direction (`STATUS_WORKING`
//! green measures 2.16:1 on white, `FOCUSED` cyan 1.98:1) and are deliberately
//! not asserted here — fixing them means re-picking colours that are already
//! spoken for, which is a separate decision from unbreaking the one role that
//! was measured as unreadable.

use dot_agent_deck::palette;
use ratatui::style::Color;
use spec::spec;

/// An sRGB triple in the reference palette.
type Rgb = (u8, u8, u8);

/// A pure-white terminal background — the worst realistic light-theme canvas,
/// and the one the issue's measurements were rasterized against.
const LIGHT_BG: Rgb = (0xFF, 0xFF, 0xFF);
/// A pure-black terminal background — the canonical dark-theme canvas.
const DARK_BG: Rgb = (0x00, 0x00, 0x00);

/// WCAG AA for text (SC 1.4.3). Applies to the theme-matched pairings.
const AA_TEXT: f64 = 4.5;
/// WCAG AA for non-text UI components and graphical objects (SC 1.4.11).
/// Applies to the mismatched pairings, where a status chip and a border glyph
/// are the surfaces still carrying the signal.
const AA_NON_TEXT: f64 = 3.0;

/// The reference sRGB rendering of a named ANSI colour: `(base, bright)`, where
/// `bright` is the slot a terminal substitutes when it renders bold as bright —
/// and, for the `Light*`/`DarkGray`/`White` names, the same value, since those
/// already name the bright half.
///
/// Values are the xterm defaults (ANSI 0–15). `Color::Reset`, `Color::Rgb` and
/// `Color::Indexed` return `None`: `Reset` is the terminal's own foreground and
/// so is legible by construction, while the other two are absolute colours the
/// palette forbids outright (`theme/contrast/001`).
fn reference_srgb(color: Color) -> Option<(Rgb, Rgb)> {
    let pair = match color {
        Color::Black => ((0x00, 0x00, 0x00), (0x7F, 0x7F, 0x7F)),
        Color::Red => ((0xCD, 0x00, 0x00), (0xFF, 0x00, 0x00)),
        Color::Green => ((0x00, 0xCD, 0x00), (0x00, 0xFF, 0x00)),
        Color::Yellow => ((0xCD, 0xCD, 0x00), (0xFF, 0xFF, 0x00)),
        Color::Blue => ((0x00, 0x00, 0xEE), (0x5C, 0x5C, 0xFF)),
        Color::Magenta => ((0xCD, 0x00, 0xCD), (0xFF, 0x00, 0xFF)),
        Color::Cyan => ((0x00, 0xCD, 0xCD), (0x00, 0xFF, 0xFF)),
        Color::Gray => ((0xE5, 0xE5, 0xE5), (0xFF, 0xFF, 0xFF)),
        Color::DarkGray => ((0x7F, 0x7F, 0x7F), (0x7F, 0x7F, 0x7F)),
        Color::LightRed => ((0xFF, 0x00, 0x00), (0xFF, 0x00, 0x00)),
        Color::LightGreen => ((0x00, 0xFF, 0x00), (0x00, 0xFF, 0x00)),
        Color::LightYellow => ((0xFF, 0xFF, 0x00), (0xFF, 0xFF, 0x00)),
        Color::LightBlue => ((0x5C, 0x5C, 0xFF), (0x5C, 0x5C, 0xFF)),
        Color::LightMagenta => ((0xFF, 0x00, 0xFF), (0xFF, 0x00, 0xFF)),
        Color::LightCyan => ((0x00, 0xFF, 0xFF), (0x00, 0xFF, 0xFF)),
        Color::White => ((0xFF, 0xFF, 0xFF), (0xFF, 0xFF, 0xFF)),
        _ => return None,
    };
    Some(pair)
}

/// WCAG 2.x relative luminance of an sRGB triple.
fn relative_luminance((r, g, b): Rgb) -> f64 {
    fn channel(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// WCAG 2.x contrast ratio between two sRGB triples, in `[1.0, 21.0]`.
fn contrast_ratio(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// The four `(label, foreground, background, floor)` pairings a named ANSI
/// foreground has to survive, in the order the module doc explains them.
fn pairings(base: Rgb, bright: Rgb) -> [(&'static str, Rgb, Rgb, f64); 4] {
    [
        (
            "light terminal (base slot on white)",
            base,
            LIGHT_BG,
            AA_TEXT,
        ),
        (
            "dark terminal (bright slot on black)",
            bright,
            DARK_BG,
            AA_TEXT,
        ),
        (
            "light terminal rendering bold as bright (bright slot on white)",
            bright,
            LIGHT_BG,
            AA_NON_TEXT,
        ),
        (
            "dark terminal keeping the base palette (base slot on black)",
            base,
            DARK_BG,
            AA_NON_TEXT,
        ),
    ]
}

/// Scenario: Take `palette::STATUS_WAITING` as it stands, resolve it through the
/// reference xterm ANSI palette into its base and bold-as-bright renderings, and
/// compute the WCAG contrast ratio against a white and a black terminal
/// background. The two theme-matched pairings — the base slot on white, the
/// bright slot on black — must clear AA for text (4.5:1), and the two mismatched
/// pairings must clear AA for non-text UI components (3:1). Also assert the role
/// is still a distinct colour from every other palette role, so a future fix for
/// one status cannot quietly merge two of them. Yellow, the colour this guard
/// was written against, fails the very first pairing at 1.70:1.
#[spec("theme/contrast/002")]
#[test]
fn contrast_002_waiting_status_is_legible_on_light_and_dark_terminals() {
    let waiting = palette::STATUS_WAITING;
    let (base, bright) = reference_srgb(waiting).unwrap_or_else(|| {
        panic!(
            "STATUS_WAITING is {waiting:?}, which has no named-ANSI reference rendering — \
             a status role must be a named ANSI colour so the terminal's own theme can \
             remap it (see `theme/contrast/001`)"
        )
    });

    for (label, fg, bg, floor) in pairings(base, bright) {
        let ratio = contrast_ratio(fg, bg);
        assert!(
            ratio >= floor,
            "STATUS_WAITING ({waiting:?}) renders at {ratio:.2}:1 on a {label}, below the \
             {floor}:1 floor — the `Needs Input` badge, the card and pane borders, the \
             stats-bar segment and the orchestration tab label all resolve through \
             `palette::status_color`, so this one colour decides whether the status the \
             user MUST notice is readable (issue #579)"
        );
    }

    // Distinctness: contrast alone would be satisfied by simply reusing another
    // role's colour, which would make "needs input" unreadable in a different
    // way. Every other role must stay a different colour.
    for (name, other) in [
        ("STATUS_WORKING", palette::STATUS_WORKING),
        ("STATUS_THINKING", palette::STATUS_THINKING),
        ("STATUS_ERROR", palette::STATUS_ERROR),
        ("STATUS_IDLE", palette::STATUS_IDLE),
        ("FOCUSED", palette::FOCUSED),
        ("SELECTED", palette::SELECTED),
    ] {
        assert_ne!(
            waiting, other,
            "STATUS_WAITING must stay distinct from {name}; a legible colour that collides \
             with another role still loses the signal"
        );
    }
}

// Unit-guard for the arithmetic above (not a `#[spec]` catalog entry). A
// contrast test is only worth its floors if the ratios it computes are the ones
// a contrast checker would report, and a subtly wrong `relative_luminance` would
// pass every assertion in `contrast_002` while measuring nothing. This pins the
// helpers against three externally-known values and then proves the gate
// actually bites on the colour that caused issue #579 — without that last part,
// a floor that had drifted below the defect would look identical to one that
// holds.
#[test]
fn contrast_helpers_reproduce_known_ratios_and_reject_yellow() {
    // The two endpoints of the WCAG scale.
    let extreme = contrast_ratio(LIGHT_BG, DARK_BG);
    assert!(
        (extreme - 21.0).abs() < 0.01,
        "white-on-black must be 21:1, got {extreme:.4}"
    );
    let identity = contrast_ratio(LIGHT_BG, LIGHT_BG);
    assert!(
        (identity - 1.0).abs() < 1e-9,
        "a colour against itself must be 1:1, got {identity:.4}"
    );

    // The issue's own measurements, reproduced from the reference palette: the
    // plain slot at ~1.70:1 and the bold-as-bright slot at ~1.07:1, both on
    // white. Matching these is what ties the model to the reported defect.
    let (yellow_base, yellow_bright) =
        reference_srgb(Color::Yellow).expect("Yellow is a named ANSI colour");
    let plain = contrast_ratio(yellow_base, LIGHT_BG);
    let bold = contrast_ratio(yellow_bright, LIGHT_BG);
    assert!(
        (plain - 1.70).abs() < 0.01,
        "reference yellow on white must reproduce the reported 1.70:1, got {plain:.2}"
    );
    assert!(
        (bold - 1.07).abs() < 0.01,
        "reference bright yellow on white must reproduce the reported 1.07:1, got {bold:.2}"
    );

    // The gate bites: Yellow fails both floors, so `contrast_002` would have
    // caught issue #579 rather than sailing past it.
    let failures = pairings(yellow_base, yellow_bright)
        .into_iter()
        .filter(|&(_, fg, bg, floor)| contrast_ratio(fg, bg) < floor)
        .count();
    assert_eq!(
        failures, 2,
        "the retired Color::Yellow must fail both light-terminal pairings — if it does not, \
         the floors in `contrast_002` no longer reject the defect they were written for"
    );
}
