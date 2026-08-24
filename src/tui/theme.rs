//! Shared semantic color palettes for the terminal UI.
//!
//! A [`Theme`] keeps rendering code independent of a particular palette.  The
//! role styles intentionally retain the modifiers used by the original TUI:
//! selected and button states are reversed, rows are underlined on hover, and
//! prominent labels are bold.

use std::fmt;

use clap::ValueEnum;
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

/// A named TUI palette.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ThemeName {
    /// Dark slate palette used by default.
    Dark,
    /// Light palette for bright terminal backgrounds.
    Light,
    /// Catppuccin Mocha palette.
    Catppuccin,
    /// Nord palette.
    Nord,
    /// Dracula palette.
    Dracula,
}

impl ThemeName {
    /// Every palette in CLI/display order.
    pub const ALL: [Self; 5] = [
        Self::Dark,
        Self::Light,
        Self::Catppuccin,
        Self::Nord,
        Self::Dracula,
    ];

    /// The stable command-line spelling of this palette.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Catppuccin => "catppuccin",
            Self::Nord => "nord",
            Self::Dracula => "dracula",
        }
    }
}

impl Default for ThemeName {
    fn default() -> Self {
        Self::Dark
    }
}

impl fmt::Display for ThemeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str((*self).as_str())
    }
}

/// Semantic styles and terminal colors for one complete TUI palette.
///
/// Foreground and background are colors rather than incremental styles so a
/// caller can compose them with a widget's existing style.  The remaining
/// fields are role styles and can be applied directly to spans, rows, and
/// widgets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub accent: Style,
    pub muted: Style,
    pub info: Style,
    pub success: Style,
    pub warning: Style,
    pub error: Style,
    pub label: Style,
    pub header: Style,
    pub border: Style,
    pub selected: Style,
    pub hover_button: Style,
    pub hover_row: Style,
    pub hover_danger: Style,
    pub background: Color,
    pub foreground: Color,
}

impl Theme {
    /// Build the complete semantic palette for `name`.
    #[must_use]
    pub fn for_name(name: ThemeName) -> Self {
        match name {
            ThemeName::Dark => Self::from_colors(
                Color::Rgb(15, 17, 22),
                Color::Rgb(226, 232, 240),
                Color::Rgb(96, 165, 250),
                Color::Rgb(148, 163, 184),
                Color::Rgb(147, 197, 253),
                Color::Rgb(74, 222, 128),
                Color::Rgb(251, 191, 36),
                Color::Rgb(248, 113, 113),
                Color::Rgb(165, 180, 252),
                Color::Rgb(203, 213, 225),
                Color::Rgb(71, 85, 105),
            ),
            ThemeName::Light => Self::from_colors(
                Color::Rgb(248, 250, 252),
                Color::Rgb(23, 32, 51),
                Color::Rgb(29, 78, 216),
                Color::Rgb(71, 85, 105),
                Color::Rgb(30, 64, 175),
                Color::Rgb(4, 120, 87),
                Color::Rgb(161, 98, 7),
                Color::Rgb(185, 28, 28),
                Color::Rgb(51, 65, 85),
                Color::Rgb(15, 23, 42),
                Color::Rgb(148, 163, 184),
            ),
            // Catppuccin Mocha: https://github.com/catppuccin/catppuccin
            ThemeName::Catppuccin => Self::from_colors(
                Color::Rgb(30, 30, 46),
                Color::Rgb(205, 214, 244),
                Color::Rgb(137, 180, 250),
                Color::Rgb(147, 153, 178),
                Color::Rgb(137, 220, 235),
                Color::Rgb(166, 227, 161),
                Color::Rgb(249, 226, 175),
                Color::Rgb(243, 139, 168),
                Color::Rgb(186, 194, 222),
                Color::Rgb(205, 214, 244),
                Color::Rgb(88, 91, 112),
            ),
            ThemeName::Nord => Self::from_colors(
                Color::Rgb(46, 52, 64),
                Color::Rgb(216, 222, 233),
                Color::Rgb(136, 192, 208),
                Color::Rgb(123, 136, 161),
                Color::Rgb(129, 161, 193),
                Color::Rgb(163, 190, 140),
                Color::Rgb(235, 203, 139),
                Color::Rgb(191, 97, 106),
                Color::Rgb(216, 222, 233),
                Color::Rgb(229, 233, 240),
                Color::Rgb(76, 86, 106),
            ),
            ThemeName::Dracula => Self::from_colors(
                Color::Rgb(40, 42, 54),
                Color::Rgb(248, 248, 242),
                Color::Rgb(139, 233, 253),
                Color::Rgb(98, 114, 164),
                Color::Rgb(139, 233, 253),
                Color::Rgb(80, 250, 123),
                Color::Rgb(241, 250, 140),
                Color::Rgb(255, 85, 85),
                Color::Rgb(189, 147, 249),
                Color::Rgb(248, 248, 242),
                Color::Rgb(68, 71, 90),
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn from_colors(
        background: Color,
        foreground: Color,
        accent: Color,
        muted: Color,
        info: Color,
        success: Color,
        warning: Color,
        error: Color,
        label: Color,
        header: Color,
        border: Color,
    ) -> Self {
        Self {
            accent: Style::new().fg(accent).add_modifier(Modifier::BOLD),
            muted: Style::new().fg(muted),
            info: Style::new().fg(info),
            success: Style::new().fg(success),
            warning: Style::new().fg(warning),
            error: Style::new().fg(error),
            label: Style::new().fg(label),
            header: Style::new().fg(header).add_modifier(Modifier::BOLD),
            border: Style::new().fg(border),
            selected: Style::new().add_modifier(Modifier::REVERSED),
            hover_button: Style::new()
                .fg(accent)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            hover_row: Style::new().add_modifier(Modifier::UNDERLINED),
            hover_danger: Style::new()
                .fg(error)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            background,
            foreground,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::for_name(ThemeName::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_order_and_default_are_stable() {
        assert_eq!(ThemeName::default(), ThemeName::Dark);
        assert_eq!(
            ThemeName::ALL,
            [
                ThemeName::Dark,
                ThemeName::Light,
                ThemeName::Catppuccin,
                ThemeName::Nord,
                ThemeName::Dracula,
            ]
        );
        assert_eq!(ThemeName::Dark.to_string(), "dark");
        assert_eq!(ThemeName::Catppuccin.to_string(), "catppuccin");
    }

    #[test]
    fn palettes_are_distinct_and_keep_state_modifiers() {
        let themes: Vec<_> = ThemeName::ALL.into_iter().map(Theme::for_name).collect();
        for (index, theme) in themes.iter().enumerate() {
            assert!(
                themes[index + 1..]
                    .iter()
                    .all(|other| theme.background != other.background),
                "palette backgrounds must be distinct"
            );
            assert!(theme.selected.add_modifier.contains(Modifier::REVERSED));
            assert!(theme.hover_button.add_modifier.contains(Modifier::REVERSED));
            assert!(theme.hover_button.add_modifier.contains(Modifier::BOLD));
            assert!(theme.hover_row.add_modifier.contains(Modifier::UNDERLINED));
            assert!(theme.hover_danger.add_modifier.contains(Modifier::REVERSED));
            assert!(theme.hover_danger.add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn foreground_background_contrast_is_readable() {
        for name in ThemeName::ALL {
            let theme = Theme::for_name(name);
            let foreground = rgb(theme.foreground);
            let background = rgb(theme.background);
            let foreground_luminance = luminance(foreground);
            let background_luminance = luminance(background);
            let (lighter, darker) = if foreground_luminance > background_luminance {
                (foreground_luminance, background_luminance)
            } else {
                (background_luminance, foreground_luminance)
            };
            let ratio = (lighter + 0.05) / (darker + 0.05);
            assert!(ratio >= 4.5, "{name} contrast ratio was {ratio:.2}");
        }
    }

    fn rgb(color: Color) -> (u8, u8, u8) {
        match color {
            Color::Rgb(red, green, blue) => (red, green, blue),
            _ => panic!("themes should use explicit RGB colors"),
        }
    }

    fn luminance((red, green, blue): (u8, u8, u8)) -> f64 {
        fn channel(value: u8) -> f64 {
            let value = f64::from(value) / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }

        0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
    }
}
