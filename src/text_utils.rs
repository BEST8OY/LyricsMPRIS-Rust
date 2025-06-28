// src/text_utils.rs
// Utility functions for text formatting

use textwrap;
use unicode_width::UnicodeWidthStr;

/// Center a string within a given width, with proper CJK and emoji support
pub fn pad_centered(text: &str, width: usize) -> String {
    let text_width = UnicodeWidthStr::width(text);
    if width <= text_width {
        return text.to_string();
    }
    let pad_total = width - text_width;
    let pad_left = (pad_total + 1) / 2; // Extra space goes to the left
    let pad_right = pad_total - pad_left;
    format!("{0}{1}{2}", " ".repeat(pad_left), text, " ".repeat(pad_right))
}

/// Wrap text to a given width, preserving empty lines and not splitting words
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            result.push(String::new());
            continue;
        }
        let wrapped = textwrap::wrap(line, width);
        for w in wrapped {
            result.push(w.to_string());
        }
    }
    result
}
