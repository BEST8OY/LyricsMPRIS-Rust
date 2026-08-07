//! Styling configuration for TUI lyrics display.
//!
//! This module defines the visual styles used for different lyric states:
//! - **Before**: Lines that have already been sung (dimmed/italic)
//! - **Current**: The currently active line (bold/green, respecting user theme)
//! - **After**: Upcoming lines (normal terminal styling)

use ratatui::style::{Color, Modifier, Style};

/// Style configuration for lyrics rendering in TUI mode.
pub struct LyricStyles {
    /// Style for lines that have already passed (dimmed, italic)
    pub before: Style,
    /// Style for the currently active line (bold, green)
    pub current: Style,
    /// Style for upcoming lines (normal text)
    pub after: Style,
}

impl Default for LyricStyles {
    fn default() -> Self {
        Self {
            // Past lines: subtle, de-emphasized using terminal DIM + ITALIC
            before: Style::default().add_modifier(Modifier::ITALIC | Modifier::DIM),
            // Current line: prominent, easy to read with terminal theme Green
            current: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            // Future lines: normal terminal theme styling
            after: Style::default(),
        }
    }
}

impl LyricStyles {
    /// Compute distance-faded style for a line based on its vertical distance `dist` from current active index.
    pub fn get_line_style_by_distance(&self, dist: usize, is_before: bool) -> Style {
        if dist == 0 {
            return self.current;
        }

        if is_before {
            self.before
        } else if dist == 1 {
            self.after
        } else {
            self.after.add_modifier(Modifier::DIM)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_line_style_by_distance() {
        let styles = LyricStyles::default();
        let dist0 = styles.get_line_style_by_distance(0, false);
        assert_eq!(dist0, styles.current);

        let dist1_before = styles.get_line_style_by_distance(1, true);
        assert!(dist1_before.add_modifier.contains(Modifier::ITALIC));
    }
}
