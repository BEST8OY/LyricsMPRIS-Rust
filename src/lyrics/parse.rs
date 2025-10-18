use crate::lyrics::types::LyricLine;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

// Limits to prevent excessive memory allocation from malformed/malicious data
const MAX_LYRIC_LINES: usize = 1000;
const MAX_WORDS_PER_LINE: usize = 100;

/// Regex pattern for LRC timestamps: [MM:SS.CC]
static SYNCED_LYRICS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[(\d{1,2}):(\d{2})[.](\d{1,2})\]").unwrap());

/// Parse standard LRC format time-synced lyrics into LyricLine structs.
/// 
/// Example input:
/// ```text
/// [00:29.26]Have you got colour in your cheeks?
/// [00:34.27]Do you ever get that fear
/// ```
pub fn parse_synced_lyrics(synced: &str) -> Vec<LyricLine> {
    synced
        .lines()
        .flat_map(|line| {
            let matches: Vec<_> = SYNCED_LYRICS_RE.captures_iter(line).collect();
            if matches.is_empty() {
                return Vec::new();
            }

            let text = SYNCED_LYRICS_RE.replace_all(line, "").trim().to_string();
            if text.is_empty() {
                return Vec::new();
            }

            matches
                .into_iter()
                .map(|cap| {
                    let minutes = cap.get(1).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(0);
                    let seconds = cap.get(2).and_then(|s| s.as_str().parse::<u32>().ok()).unwrap_or(0);
                    let centiseconds = cap.get(3).and_then(|c| c.as_str().parse::<u32>().ok()).unwrap_or(0);
                    
                    let time = minutes as f64 * 60.0 + seconds as f64 + centiseconds as f64 / 100.0;
                    
                    LyricLine {
                        time,
                        text: text.clone(),
                        words: None,
                    }
                })
                .collect()
        })
        .collect()
}

/// Parse Musixmatch subtitle_body JSON into lyric lines (line-level timing only).
///
/// Format: `[{"text": "lyrics", "time": {"total": 29.26, ...}}, ...]`
///
/// Returns parsed lines or None if parsing fails.
pub fn parse_subtitle_body(subtitle_body: &str) -> Option<Vec<LyricLine>> {
    let lines_val = serde_json::from_str::<Value>(subtitle_body).ok()?;
    let arr = lines_val.as_array()?;

    let mut parsed = Vec::new();

    for line in arr {
        let time = line.pointer("/time/total").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let text = line.get("text").and_then(|v| v.as_str()).unwrap_or("♪");

        parsed.push(LyricLine {
            time,
            text: text.to_string(),
            words: None, // No word-level timing in subtitle format
        });
    }

    Some(parsed)
}

/// Parse Musixmatch richsync_body JSON into lyric lines with word-level timing.
///
/// Supports two formats:
/// 1. Word array: `{"ts": 29.26, "te": 31.59, "x": "text", "words": [{start, end, text}]}`
/// 2. Character array: `{"ts": 29.26, "te": 31.59, "x": "text", "l": [{c, o}]}`
///
/// Returns parsed lines or None if parsing fails.
pub fn parse_richsync_body(richsync_body: &str) -> Option<Vec<LyricLine>> {
    let lines_val = serde_json::from_str::<Value>(richsync_body).ok()?;
    let arr = lines_val.as_array()?;

    // Validate line count to prevent excessive allocation
    if arr.len() > MAX_LYRIC_LINES {
        tracing::warn!(
            "Richsync data has {} lines, exceeds limit of {}, truncating",
            arr.len(),
            MAX_LYRIC_LINES
        );
    }

    let mut parsed = Vec::new();

    for line in arr.iter().take(MAX_LYRIC_LINES) {
        let line_start = line.pointer("/ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let line_end = line.pointer("/te").and_then(|v| v.as_f64()).unwrap_or(line_start + 3.0);
        let text = line
            .get("x")
            .or_else(|| line.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("♪");

        // Parse word-level timings (if available)
        let words = parse_word_timings(line, line_start, line_end);

        parsed.push(LyricLine {
            time: line_start,
            text: text.to_string(),
            words,
        });
    }

    Some(parsed)
}

/// Parse word timings from a richsync line object.
/// Returns None if no word timing data is present.
fn parse_word_timings(line: &Value, line_start: f64, line_end: f64) -> Option<Vec<crate::lyrics::types::WordTiming>> {
    // Try explicit words array first
    if let Some(words_arr) = line.get("words").and_then(|v| v.as_array()) {
        // Validate word count
        if words_arr.len() > MAX_WORDS_PER_LINE {
            tracing::warn!(
                "Line has {} words, exceeds limit of {}, truncating",
                words_arr.len(),
                MAX_WORDS_PER_LINE
            );
        }
        return parse_explicit_word_array(&words_arr[..words_arr.len().min(MAX_WORDS_PER_LINE)], line_start, line_end);
    }

    // Fall back to character-level array
    if let Some(char_arr) = line.get("l").and_then(|v| v.as_array()) {
        // Validate word count (character array typically has more entries)
        if char_arr.len() > MAX_WORDS_PER_LINE {
            tracing::warn!(
                "Line has {} character entries, exceeds limit of {}, truncating",
                char_arr.len(),
                MAX_WORDS_PER_LINE
            );
        }
        return parse_character_array(&char_arr[..char_arr.len().min(MAX_WORDS_PER_LINE)], line_start, line_end);
    }

    None
}

/// Parse explicit word array: [{start, end, text}, ...]
fn parse_explicit_word_array(words_arr: &[Value], line_start: f64, line_end: f64) -> Option<Vec<crate::lyrics::types::WordTiming>> {
    let word_timings: Vec<crate::lyrics::types::WordTiming> = words_arr
        .iter()
        .filter_map(|w| {
            let start = w.get("start").and_then(|v| v.as_f64()).unwrap_or(line_start);
            let end = w.get("end").and_then(|v| v.as_f64()).unwrap_or(start);
            let text = w.get("text").and_then(|v| v.as_str()).unwrap_or("");

            // Validate and fix timing
            let final_end = if end <= start { line_end } else { end };

            Some(create_word_timing(start, final_end, text))
        })
        .collect();

    if word_timings.is_empty() {
        None
    } else {
        Some(word_timings)
    }
}

/// Parse character-level array: [{c: "word", o: offset}, ...]
fn parse_character_array(char_arr: &[Value], line_start: f64, line_end: f64) -> Option<Vec<crate::lyrics::types::WordTiming>> {
    let word_timings: Vec<crate::lyrics::types::WordTiming> = char_arr
        .iter()
        .enumerate()
        .filter_map(|(i, elem)| {
            let text = elem.get("c").and_then(|v| v.as_str()).unwrap_or("");
            
            // Skip whitespace-only entries
            if text.trim().is_empty() {
                return None;
            }

            let start_offset = elem.get("o").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let start = line_start + start_offset;

            // Calculate end time from next element or use line end
            let end = char_arr
                .get(i + 1)
                .and_then(|next| next.get("o").and_then(|v| v.as_f64()))
                .map(|offset| line_start + offset)
                .unwrap_or(line_end);

            // Validate timing
            let final_end = if end <= start { line_end } else { end };

            Some(create_word_timing(start, final_end, text))
        })
        .collect();

    if word_timings.is_empty() {
        None
    } else {
        Some(word_timings)
    }
}

/// Create a WordTiming struct with precomputed grapheme boundary data.
fn create_word_timing(start: f64, end: f64, text: &str) -> crate::lyrics::types::WordTiming {
    // Precompute grapheme cluster boundaries for efficient Unicode-aware rendering
    // This avoids storing each grapheme as a separate String (24 bytes overhead each)
    let mut grapheme_boundaries: Vec<usize> = Vec::new();
    grapheme_boundaries.push(0);
    
    for (byte_offset, _grapheme) in text.grapheme_indices(true) {
        if byte_offset > 0 {
            grapheme_boundaries.push(byte_offset);
        }
    }
    
    // Add final boundary for convenience (allows simple slicing: text[boundaries[i]..boundaries[i+1]])
    grapheme_boundaries.push(text.len());

    crate::lyrics::types::WordTiming {
        start,
        end,
        text: text.to_string(),
        grapheme_boundaries,
    }
}
