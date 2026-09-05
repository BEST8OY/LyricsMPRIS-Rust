//! Compression and string normalization utilities for database storage.

use std::io::Cursor;
use std::sync::LazyLock;

static REMASTER_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)(?:\s*[-–—]\s*|\s+[\(\[])(?:\d{4}\s*[-–—]?\s*|(?:(?:\d+(?:th|nd|rd|st)\s+)?anniversary\s+))?(?:digital\s+)?remaster(?:ed)?(?:\s+(?:version|edition|audio|mix))?(?:\s+\d{4})?[\)\]]?$"#,
    )
    .unwrap()
});

/// Canonicalizes typographic characters (smart quotes, dashes, non-breaking spaces)
/// and converts to lowercase trimmed string for resilient indexing.
pub(crate) fn normalize(s: &str) -> String {
    let mut normalized = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            // Smart single quotes / apostrophes / backticks -> ASCII straight single quote
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '`' => normalized.push('\''),
            // Smart double quotes -> ASCII straight double quote
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => normalized.push('"'),
            // En-dash, Em-dash, horizontal bar -> ASCII hyphen
            '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => normalized.push('-'),
            // Non-breaking space -> regular space
            '\u{00A0}' => normalized.push(' '),
            // Zero-width spaces -> ignore
            '\u{200B}' | '\u{FEFF}' => {}
            other => normalized.push(other),
        }
    }
    normalized.trim().to_lowercase()
}

/// Strips common remaster annotations from track titles, returning the clean base title if modified.
pub(crate) fn strip_remaster_suffix(title: &str) -> Option<String> {
    if REMASTER_RE.is_match(title) {
        let stripped = REMASTER_RE.replace(title, "");
        let clean = stripped.trim();
        if !clean.is_empty() && clean != title {
            return Some(clean.to_string());
        }
    }
    None
}

pub(crate) fn normalize_isrc(s: &str) -> Option<String> {
    let normalized: String = s
        .trim()
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub(crate) fn normalize_track_id(s: &str) -> Option<String> {
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

pub(crate) fn compress_raw_lyrics(raw: &str) -> Result<Vec<u8>, std::io::Error> {
    // Level 3 is zstd's default and a good balance for small payloads.
    zstd::stream::encode_all(Cursor::new(raw.as_bytes()), 3)
}

pub(crate) fn decompress_raw_lyrics(raw: &[u8]) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }

    let decoded = zstd::stream::decode_all(Cursor::new(raw)).ok()?;
    String::from_utf8(decoded).ok()
}
