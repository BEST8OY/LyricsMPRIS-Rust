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
                    let minutes = cap
                        .get(1)
                        .and_then(|m| m.as_str().parse::<u32>().ok())
                        .unwrap_or(0);
                    let seconds = cap
                        .get(2)
                        .and_then(|s| s.as_str().parse::<u32>().ok())
                        .unwrap_or(0);
                    let centiseconds = cap
                        .get(3)
                        .and_then(|c| c.as_str().parse::<u32>().ok())
                        .unwrap_or(0);

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
        let time = line
            .pointer("/time/total")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
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
        let line_end = line
            .pointer("/te")
            .and_then(|v| v.as_f64())
            .unwrap_or(line_start + 3.0);
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
fn parse_word_timings(
    line: &Value,
    line_start: f64,
    line_end: f64,
) -> Option<Vec<crate::lyrics::types::WordTiming>> {
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
        return parse_explicit_word_array(
            &words_arr[..words_arr.len().min(MAX_WORDS_PER_LINE)],
            line_start,
            line_end,
        );
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
        return parse_character_array(
            &char_arr[..char_arr.len().min(MAX_WORDS_PER_LINE)],
            line_start,
            line_end,
        );
    }

    None
}

/// Parse explicit word array: [{start, end, text}, ...]
fn parse_explicit_word_array(
    words_arr: &[Value],
    line_start: f64,
    line_end: f64,
) -> Option<Vec<crate::lyrics::types::WordTiming>> {
    let word_timings: Vec<crate::lyrics::types::WordTiming> = words_arr
        .iter()
        .map(|w| {
            let start = w
                .get("start")
                .and_then(|v| v.as_f64())
                .unwrap_or(line_start);
            let end = w.get("end").and_then(|v| v.as_f64()).unwrap_or(start);
            let text = w.get("text").and_then(|v| v.as_str()).unwrap_or("");

            // Validate and fix timing
            let final_end = if end <= start { line_end } else { end };

            create_word_timing(start, final_end, text)
        })
        .collect();

    if word_timings.is_empty() {
        None
    } else {
        Some(word_timings)
    }
}

/// Parse character-level array: [{c: "word", o: offset}, ...]
fn parse_character_array(
    char_arr: &[Value],
    line_start: f64,
    line_end: f64,
) -> Option<Vec<crate::lyrics::types::WordTiming>> {
    let mut word_timings: Vec<crate::lyrics::types::WordTiming> = Vec::new();

    for (i, elem) in char_arr.iter().enumerate() {
        let text = elem.get("c").and_then(|v| v.as_str()).unwrap_or("");

        // Skip whitespace-only entries
        if text.trim().is_empty() {
            continue;
        }

        let start_offset = elem.get("o").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let start = line_start + start_offset;

        // Determine raw end offset from the immediate next element in char_arr (e.g. trailing space)
        let immediate_next_offset = char_arr
            .get(i + 1)
            .and_then(|next| next.get("o").and_then(|v| v.as_f64()));

        // Determine start offset of the next non-whitespace word
        let next_word_offset = char_arr
            .iter()
            .skip(i + 1)
            .find(|next| {
                next.get("c")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
            })
            .and_then(|next| next.get("o").and_then(|v| v.as_f64()));

        let end = match (immediate_next_offset, next_word_offset) {
            (Some(imm), Some(next_w)) => {
                let imm_time = line_start + imm;
                let next_w_time = line_start + next_w;
                // If inter-word pause is short (<= 0.4s), bridge word end to next word start for smooth rendering
                if next_w_time - imm_time <= 0.4 {
                    next_w_time
                } else {
                    imm_time
                }
            }
            (Some(imm), None) => line_start + imm,
            (None, Some(next_w)) => line_start + next_w,
            (None, None) => line_end,
        };

        // Enforce a minimum display duration of 40ms for fast words
        const MIN_WORD_DURATION: f64 = 0.040;
        let final_end = if end <= start {
            line_end.max(start + MIN_WORD_DURATION)
        } else {
            end.max(start + MIN_WORD_DURATION)
        };

        word_timings.push(create_word_timing(start, final_end, text));
    }

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

    // Pre-compute time boundaries for each grapheme transition
    let total = grapheme_boundaries.len().saturating_sub(1);
    let duration = (end - start).max(f64::EPSILON);
    let grapheme_times: Vec<f64> = (1..total)
        .map(|k| start + (k as f64 / total as f64) * duration)
        .collect();

    crate::lyrics::types::WordTiming {
        start,
        end,
        text: text.to_string(),
        grapheme_boundaries,
        grapheme_times,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fast_richsync_parsing_and_bridging() {
        let fast_richsync = r#"[
            {
                "ts": 119.38,
                "te": 120.355,
                "l": [
                    {"c": "And", "o": 0},
                    {"c": " ", "o": 0.068},
                    {"c": "the", "o": 0.137},
                    {"c": " ", "o": 0.154},
                    {"c": "kids", "o": 0.171},
                    {"c": " ", "o": 0.187},
                    {"c": "in", "o": 0.195},
                    {"c": " ", "o": 0.203},
                    {"c": "the", "o": 0.254},
                    {"c": " ", "o": 0.371},
                    {"c": "dark", "o": 0.405},
                    {"c": " ", "o": 0.421},
                    {"c": "that", "o": 0.438},
                    {"c": " ", "o": 0.522},
                    {"c": "were", "o": 0.538},
                    {"c": " ", "o": 0.623},
                    {"c": "doomed", "o": 0.64},
                    {"c": " ", "o": 0.774},
                    {"c": "from", "o": 0.79},
                    {"c": " ", "o": 0.859},
                    {"c": "the", "o": 0.875},
                    {"c": " ", "o": 0.891},
                    {"c": "start", "o": 0.907}
                ],
                "x": "And the kids in the dark that were doomed from the start"
            }
        ]"#;

        let parsed = parse_richsync_body(fast_richsync).expect("Should parse fast richsync body");
        assert_eq!(parsed.len(), 1);

        let line = &parsed[0];
        let words = line.words.as_ref().expect("Should have words timing");
        assert_eq!(words.len(), 12);

        // Verify minimum word duration enforcement
        for word in words {
            let duration = word.end - word.start;
            assert!(
                duration >= 0.039,
                "Word '{}' duration {}s should be >= 0.040s",
                word.text,
                duration
            );
        }

        // Verify bridging between 'And' and 'the'
        let and_word = &words[0];
        let the_word = &words[1];
        assert_eq!(and_word.text, "And");
        assert_eq!(the_word.text, "the");
        assert!(
            (and_word.end - the_word.start).abs() < 1e-4,
            "End of 'And' ({}) should bridge directly to start of 'the' ({})",
            and_word.end,
            the_word.start
        );
    }

    #[test]
    fn test_additional_fast_lines_parsing_and_bridging() {
        let fast_richsync_batch = r#"[
            {"ts":120.96,"te":122.015,"l":[{"c":"The","o":0},{"c":" ","o":0.073},{"c":"child","o":0.146},{"c":" ","o":0.179},{"c":"in","o":0.187},{"c":" ","o":0.196},{"c":"the","o":0.213},{"c":" ","o":0.246},{"c":"basement,","o":0.413},{"c":" ","o":0.58},{"c":"face","o":0.596},{"c":" ","o":0.713},{"c":"to","o":0.73},{"c":" ","o":0.747},{"c":"the","o":0.765},{"c":" ","o":0.8159},{"c":"pavement","o":0.832}],"x":"The child in the basement, face to the pavement"},
            {"ts":122.79,"te":123.744,"l":[{"c":"Oh,","o":0},{"c":" ","o":0.016},{"c":"what","o":0.033},{"c":" ","o":0.066},{"c":"a","o":0.074},{"c":" ","o":0.083},{"c":"statement,","o":0.116},{"c":" ","o":0.534},{"c":"love","o":0.551},{"c":" ","o":0.684},{"c":"is","o":0.701},{"c":" ","o":0.751},{"c":"embracement","o":0.835}],"x":"Oh, what a statement, love is embracement"},
            {"ts":124.27,"te":125.07,"l":[{"c":"Love","o":0},{"c":" ","o":0.138},{"c":"is","o":0.145},{"c":" ","o":0.153},{"c":"a","o":0.162},{"c":" ","o":0.171},{"c":"constant,","o":0.196},{"c":" ","o":0.222},{"c":"love","o":0.239},{"c":" ","o":0.321},{"c":"is","o":0.338},{"c":" ","o":0.406},{"c":"a","o":0.413},{"c":" ","o":0.421},{"c":"basis","o":0.472}],"x":"Love is a constant, love is a basis"},
            {"ts":125.65,"te":127.966,"l":[{"c":"He","o":0},{"c":" ","o":0.053},{"c":"cannot","o":0.106},{"c":" ","o":0.159},{"c":"be,","o":0.177},{"c":" ","o":0.195},{"c":"she","o":0.224},{"c":" ","o":0.308},{"c":"cannot","o":0.359},{"c":" ","o":0.611},{"c":"be,","o":0.626},{"c":" ","o":0.895},{"c":"they","o":0.911},{"c":" ","o":1.213},{"c":"cannot","o":1.314},{"c":" ","o":1.466},{"c":"be","o":1.49},{"c":" ","o":1.5149},{"c":"changed","o":1.549}],"x":"He cannot be, she cannot be, they cannot be changed"}
        ]"#;

        let parsed = parse_richsync_body(fast_richsync_batch)
            .expect("Should parse fast batch richsync body");
        assert_eq!(parsed.len(), 4);

        for line in &parsed {
            let words = line
                .words
                .as_ref()
                .expect("Each line should have word timing data");
            assert!(!words.is_empty());

            // Validate duration and bridging constraints on every word in each fast line
            for i in 0..words.len() {
                let word = &words[i];
                let duration = word.end - word.start;
                assert!(
                    duration >= 0.039,
                    "Line '{}' -> Word '{}' duration {}s should be >= 0.040s",
                    line.text,
                    word.text,
                    duration
                );

                if i + 1 < words.len() {
                    let next_word = &words[i + 1];
                    // Verify bridging when next word starts close by (< 0.4s)
                    if next_word.start - word.start <= 0.4 {
                        assert!(
                            word.end >= next_word.start - 1e-4,
                            "Word '{}' end ({}) should bridge to next word '{}' start ({})",
                            word.text,
                            word.end,
                            next_word.text,
                            next_word.start
                        );
                    }
                }
            }
        }
    }
}
