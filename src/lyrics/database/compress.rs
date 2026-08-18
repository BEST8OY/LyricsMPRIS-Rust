//! Compression and string normalization utilities for database storage.

use std::io::Cursor;

/// Normalizes a string for case-insensitive matching.
pub(crate) fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

pub(crate) fn normalize_isrc(s: &str) -> Option<String> {
    let trimmed = s.trim().to_uppercase();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
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

pub(crate) fn decompress_raw_lyrics(raw: Vec<u8>) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }

    let decoded = zstd::stream::decode_all(Cursor::new(&raw)).ok()?;
    String::from_utf8(decoded).ok()
}
