//! Styling configuration for TUI lyrics display.
//!
//! This module defines the visual styles used for different lyric states:
//! - **Before**: Lines that have already been sung (dimmed/italic)
//! - **Current**: The currently active line (bold/green)
//! - **After**: Upcoming lines (normal styling)

use tui::style::{Color, Modifier, Style};

/// Style configuration for lyrics rendering in TUI mode.
///
/// # Example
/// ```
/// let styles = LyricStyles::default();
/// // Use styles.current for the active line
/// // Use styles.before for past lines
/// // Use styles.after for future lines
/// ```
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
            // Past lines: subtle, de-emphasized
            before: Style::default()
                .add_modifier(Modifier::ITALIC | Modifier::DIM),
            // Current line: prominent, easy to read
            current: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            // Future lines: normal styling
            after: Style::default(),
        }
    }
}

impl LyricStyles {
    /// Create a new style set with custom colors.
    ///
    /// # Arguments
    /// * `current_color` - Color for the active line
    #[allow(dead_code)]
    pub fn with_current_color(current_color: Color) -> Self {
        Self {
            before: Style::default()
                .add_modifier(Modifier::ITALIC | Modifier::DIM),
            current: Style::default()
                .fg(current_color)
                .add_modifier(Modifier::BOLD),
            after: Style::default(),
        }
    }
}
