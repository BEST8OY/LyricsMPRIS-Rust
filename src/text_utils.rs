// src/text_utils.rs
// Utility functions for text formatting

/// Center a string within a given width
pub fn pad_centered(text: &str, width: usize) -> String {
    let line_width = text.chars().count();
    let pad_left = if width > line_width { (width - line_width) / 2 } else { 0 };
    let mut content = String::with_capacity(width.max(line_width));
    for _ in 0..pad_left { content.push(' '); }
    content.push_str(text);
    content
}

/// Wrap text to a given width, breaking at word boundaries
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.chars().count() + word.chars().count() + 1 > width && !current.is_empty() {
            lines.push(current);
            current = String::new();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}
