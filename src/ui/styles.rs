use tui::style;

pub struct LyricStyles {
    pub before: style::Style,
    pub current: style::Style,
    pub after: style::Style,
}

impl LyricStyles {}

impl Default for LyricStyles {
    fn default() -> Self {
        Self {
            before: style::Style::default()
                .add_modifier(style::Modifier::ITALIC | style::Modifier::DIM),
            current: style::Style::default()
                .fg(style::Color::Green)
                .add_modifier(style::Modifier::BOLD),
            after: style::Style::default(),
        }
    }
}
