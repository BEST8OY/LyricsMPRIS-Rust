use crate::lyrics::types::LyricLine;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

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
/// Returns (parsed_lines, lrc_string) or None if parsing fails.
pub fn parse_subtitle_body(subtitle_body: &str) -> Option<(Vec<LyricLine>, String)> {
    let lines_val = serde_json::from_str::<Value>(subtitle_body).ok()?;
    let arr = lines_val.as_array()?;

    let mut parsed = Vec::new();
    let mut lrc_output = String::new();

    for line in arr {
        let time = line.pointer("/time/total").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let text = line.get("text").and_then(|v| v.as_str()).unwrap_or("♪");

        // Generate LRC timestamp
        lrc_output.push_str(&format_lrc_timestamp(time, text));

        parsed.push(LyricLine {
            time,
            text: text.to_string(),
            words: None, // No word-level timing in subtitle format
        });
    }

    Some((parsed, lrc_output))
}

/// Parse Musixmatch richsync_body JSON into lyric lines with word-level timing.
///
/// Supports two formats:
/// 1. Word array: `{"ts": 29.26, "te": 31.59, "x": "text", "words": [{start, end, text}]}`
/// 2. Character array: `{"ts": 29.26, "te": 31.59, "x": "text", "l": [{c, o}]}`
///
/// Returns (parsed_lines, lrc_with_richsync_marker) or None if parsing fails.
pub fn parse_richsync_body(richsync_body: &str) -> Option<(Vec<LyricLine>, String)> {
    let lines_val = serde_json::from_str::<Value>(richsync_body).ok()?;
    let arr = lines_val.as_array()?;

    let mut parsed = Vec::new();
    let mut lrc_output = String::new();

    for line in arr {
        let line_start = line.pointer("/ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let line_end = line.pointer("/te").and_then(|v| v.as_f64()).unwrap_or(line_start + 3.0);
        let text = line
            .get("x")
            .or_else(|| line.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("♪");

        // Generate LRC timestamp
        lrc_output.push_str(&format_lrc_timestamp(line_start, text));

        // Parse word-level timings (if available)
        let words = parse_word_timings(line, line_start, line_end);

        parsed.push(LyricLine {
            time: line_start,
            text: text.to_string(),
            words,
        });
    }

    let output_with_marker = format!(";;richsync=1\n{}", lrc_output);
    Some((parsed, output_with_marker))
}

/// Format a timestamp and text into LRC format: [MM:SS.CC]text
fn format_lrc_timestamp(time: f64, text: &str) -> String {
    let ms = (time * 1000.0).round() as u64;
    let minutes = ms / 60000;
    let seconds = (ms % 60000) / 1000;
    let centiseconds = (ms % 1000) / 10;
    format!("[{:02}:{:02}.{:02}]{}\n", minutes, seconds, centiseconds, text)
}

/// Parse word timings from a richsync line object.
/// Returns None if no word timing data is present.
fn parse_word_timings(line: &Value, line_start: f64, line_end: f64) -> Option<Vec<crate::lyrics::types::WordTiming>> {
    // Try explicit words array first
    if let Some(words_arr) = line.get("words").and_then(|v| v.as_array()) {
        return parse_explicit_word_array(words_arr, line_start, line_end);
    }

    // Fall back to character-level array
    if let Some(char_arr) = line.get("l").and_then(|v| v.as_array()) {
        return parse_character_array(char_arr, line_start, line_end);
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

/// Create a WordTiming struct with precomputed grapheme data.
fn create_word_timing(start: f64, end: f64, text: &str) -> crate::lyrics::types::WordTiming {
    // Precompute grapheme clusters for Unicode-aware rendering
    let graphemes: Vec<String> = text.graphemes(true).map(String::from).collect();
    
    // Precompute byte offsets for efficient string slicing
    let grapheme_byte_offsets: Vec<usize> = graphemes
        .iter()
        .scan(0, |offset, g| {
            let current = *offset;
            *offset += g.len();
            Some(current)
        })
        .collect();

    crate::lyrics::types::WordTiming {
        start,
        end,
        text: text.to_string(),
        graphemes,
        grapheme_byte_offsets,
    }
}
