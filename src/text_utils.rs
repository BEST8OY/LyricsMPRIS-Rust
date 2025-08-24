// src/text_utils.rs
// Utility functions for text formatting

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
