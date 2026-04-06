use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Auto,
    Light,
    Dark,
}

impl fmt::Display for Theme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Theme::Auto => write!(f, "auto"),
            Theme::Light => write!(f, "light"),
            Theme::Dark => write!(f, "dark"),
        }
    }
}

impl std::str::FromStr for Theme {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Theme::Auto),
            "light" => Ok(Theme::Light),
            "dark" => Ok(Theme::Dark),
            _ => Err(format!(
                "Invalid theme: {s} (expected auto, light, or dark)"
            )),
        }
    }
}

use std::fmt;

/// A small color palette for neutral text and selection colors.
/// Accent/status ANSI colors (Cyan, Green, Yellow, Red, Blue, Magenta) are
/// remapped by terminal themes and do not need switching.
#[derive(Debug, Clone, Copy)]
pub struct ColorPalette {
    /// Card titles, focused input text (White in dark, Black in light).
    pub text_primary: Color,
    /// Labels (Dir, Last, Tools), hint text, unfocused inputs (Gray in dark, DarkGray in light).
    pub text_secondary: Color,
    /// Separators, tool detail lines (Gray in both — DarkGray is nearly invisible on dark backgrounds).
    pub text_muted: Color,
    /// Background for the selected/active card — a subtle shift from the terminal background.
    pub selected_bg: Color,
    /// Background for the tab bar row — slightly shifted from terminal_bg for visual distinction.
    pub tab_bar_bg: Color,
    /// The terminal's actual background color (used to fill the frame explicitly).
    pub terminal_bg: Color,
}

impl ColorPalette {
    pub fn dark() -> Self {
        Self {
            text_primary: Color::White,
            text_secondary: Color::Gray,
            text_muted: Color::Gray,
            selected_bg: Color::Rgb(25, 30, 50),
            tab_bar_bg: Color::Rgb(20, 20, 25),
            terminal_bg: Color::Rgb(0, 0, 0),
        }
    }

    pub fn light() -> Self {
        Self {
            text_primary: Color::Black,
            text_secondary: Color::DarkGray,
            text_muted: Color::Gray,
            selected_bg: Color::Rgb(225, 230, 240),
            tab_bar_bg: Color::Rgb(235, 235, 240),
            terminal_bg: Color::Rgb(255, 255, 255),
        }
    }
}

/// Resolve a [`Theme`] to a concrete [`ColorPalette`].
///
/// For [`Theme::Auto`], queries the terminal background via OSC 11 and falls
/// back to the dark palette on detection failure.
pub fn resolve_palette(theme: Theme) -> ColorPalette {
    match theme {
        Theme::Dark => ColorPalette::dark(),
        Theme::Light => ColorPalette::light(),
        Theme::Auto => detect_palette(),
    }
}

fn detect_palette() -> ColorPalette {
    let colors = terminal_colorsaurus::color_palette(terminal_colorsaurus::QueryOptions::default());

    let is_light = matches!(
        colors.as_ref().map(|c| c.theme_mode()),
        Ok(terminal_colorsaurus::ThemeMode::Light)
    );
    let mut palette = if is_light {
        ColorPalette::light()
    } else {
        ColorPalette::dark()
    };

    // Derive selected_bg and terminal_bg from the actual terminal background.
    if let Ok(colors) = colors {
        let (r, g, b) = colors.background.scale_to_8bit();
        palette.terminal_bg = Color::Rgb(r, g, b);
        palette.selected_bg = if is_light {
            // Darken slightly for light backgrounds
            Color::Rgb(
                r.saturating_sub(20),
                g.saturating_sub(18),
                b.saturating_sub(10),
            )
        } else {
            // Lighten slightly for dark backgrounds
            Color::Rgb(
                r.saturating_add(20),
                g.saturating_add(22),
                b.saturating_add(35),
            )
        };
        palette.tab_bar_bg = if is_light {
            Color::Rgb(
                r.saturating_sub(12),
                g.saturating_sub(12),
                b.saturating_sub(8),
            )
        } else {
            Color::Rgb(
                r.saturating_add(12),
                g.saturating_add(12),
                b.saturating_add(15),
            )
        };
    }

    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_palette_values() {
        let p = ColorPalette::dark();
        assert_eq!(p.text_primary, Color::White);
        assert_eq!(p.text_secondary, Color::Gray);
        assert_eq!(p.text_muted, Color::Gray);
        assert_eq!(p.selected_bg, Color::Rgb(25, 30, 50));
        assert_eq!(p.tab_bar_bg, Color::Rgb(20, 20, 25));
        assert_eq!(p.terminal_bg, Color::Rgb(0, 0, 0));
    }

    #[test]
    fn light_palette_values() {
        let p = ColorPalette::light();
        assert_eq!(p.text_primary, Color::Black);
        assert_eq!(p.text_secondary, Color::DarkGray);
        assert_eq!(p.text_muted, Color::Gray);
        assert_eq!(p.selected_bg, Color::Rgb(225, 230, 240));
        assert_eq!(p.tab_bar_bg, Color::Rgb(235, 235, 240));
        assert_eq!(p.terminal_bg, Color::Rgb(255, 255, 255));
    }

    #[test]
    fn resolve_explicit_dark() {
        let p = resolve_palette(Theme::Dark);
        assert_eq!(p.text_primary, Color::White);
    }

    #[test]
    fn resolve_explicit_light() {
        let p = resolve_palette(Theme::Light);
        assert_eq!(p.text_primary, Color::Black);
    }

    #[test]
    fn theme_display() {
        assert_eq!(Theme::Auto.to_string(), "auto");
        assert_eq!(Theme::Light.to_string(), "light");
        assert_eq!(Theme::Dark.to_string(), "dark");
    }

    #[test]
    fn theme_from_str() {
        assert_eq!("auto".parse::<Theme>().unwrap(), Theme::Auto);
        assert_eq!("light".parse::<Theme>().unwrap(), Theme::Light);
        assert_eq!("dark".parse::<Theme>().unwrap(), Theme::Dark);
        assert!("invalid".parse::<Theme>().is_err());
    }
}
