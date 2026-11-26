//! Top-level lyrics parsing facade (delegating to `parsers::*` + Musixmatch JSON parsers).

#[allow(unused_imports)]
pub use crate::lyrics::parsers::{
    extract_credit_metadata, parse_krc, parse_lyricify_lines, parse_lyricify_syllable,
    parse_lyricify_translation, parse_lyrics_file, parse_plain,
    parse_synced as parse_synced_lyrics, parse_synced_with_credits, parse_ttml,
};

use crate::lyrics::types::{LyricLine, WordTiming, create_word_timing};
use serde_json::Value;

const MAX_LYRIC_LINES: usize = 1000;
const MAX_WORDS_PER_LINE: usize = 100;

/// Parse Musixmatch subtitle_body JSON into lyric lines (line-level timing only).
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
        parsed.push(LyricLine::new(time, text));
    }
    Some(parsed)
}

/// Parse Musixmatch richsync_body JSON into lyric lines with word-level timing.
pub fn parse_richsync_body(richsync_body: &str) -> Option<Vec<LyricLine>> {
    let lines_val = serde_json::from_str::<Value>(richsync_body).ok()?;
    let arr = lines_val.as_array()?;

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

        let words = parse_word_timings(line, line_start, line_end);
        parsed.push(
            LyricLine::new(line_start, text)
                .with_end(Some(line_end))
                .with_words(words),
        );
    }

    Some(parsed)
}

fn parse_word_timings(line: &Value, line_start: f64, line_end: f64) -> Option<Vec<WordTiming>> {
    if let Some(words_arr) = line.get("words").and_then(|v| v.as_array()) {
        return parse_explicit_word_array(
            &words_arr[..words_arr.len().min(MAX_WORDS_PER_LINE)],
            line_start,
            line_end,
        );
    }

    if let Some(char_arr) = line.get("l").and_then(|v| v.as_array()) {
        return parse_character_array(
            &char_arr[..char_arr.len().min(MAX_WORDS_PER_LINE)],
            line_start,
            line_end,
        );
    }

    None
}

fn parse_explicit_word_array(
    words_arr: &[Value],
    line_start: f64,
    line_end: f64,
) -> Option<Vec<WordTiming>> {
    let word_timings: Vec<WordTiming> = words_arr
        .iter()
        .map(|w| {
            let start = w
                .get("start")
                .and_then(|v| v.as_f64())
                .unwrap_or(line_start);
            let end = w.get("end").and_then(|v| v.as_f64()).unwrap_or(start);
            let text = w.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let final_end = if end <= start { line_end } else { end };
            create_word_timing(start, final_end, text)
        })
        .collect();

    (!word_timings.is_empty()).then_some(word_timings)
}

fn parse_character_array(
    char_arr: &[Value],
    line_start: f64,
    line_end: f64,
) -> Option<Vec<WordTiming>> {
    let mut word_timings: Vec<WordTiming> = Vec::new();

    for (i, elem) in char_arr.iter().enumerate() {
        let text = elem.get("c").and_then(|v| v.as_str()).unwrap_or("");
        if text.trim().is_empty() {
            continue;
        }

        let start_offset = elem.get("o").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let start = line_start + start_offset;

        let immediate_next_offset = char_arr
            .get(i + 1)
            .and_then(|next| next.get("o").and_then(|v| v.as_f64()));

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

        const MIN_WORD_DURATION: f64 = 0.040;
        let final_end = if end <= start {
            line_end.max(start + MIN_WORD_DURATION)
        } else {
            end.max(start + MIN_WORD_DURATION)
        };

        let ends_with_space = char_arr
            .get(i + 1)
            .and_then(|next| next.get("c").and_then(|v| v.as_str()))
            .is_some_and(|s| s.chars().any(char::is_whitespace));

        word_timings.push(create_word_timing(start, final_end, text).with_space(ends_with_space));
    }

    (!word_timings.is_empty()).then_some(word_timings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fast_richsync_parsing_and_bridging() {
        let fast_richsync = r#"[{"ts":119.38,"te":120.355,"l":[{"c":"And","o":0},{"c":" ","o":0.068},{"c":"the","o":0.137}],"x":"And the"}]"#;
        let parsed = parse_richsync_body(fast_richsync).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].words.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_parse_synced_lyrics_fraction_digits() {
        let lrc = "[00:10.5]A\n[01:20.25]B\n[02:30.123]C";
        let parsed = parse_synced_lyrics(lrc);
        assert_eq!(parsed.len(), 3);
        assert!((parsed[0].time - 10.5).abs() < 1e-6);
        assert!((parsed[1].time - 80.25).abs() < 1e-6);
        assert!((parsed[2].time - 150.123).abs() < 1e-6);
    }
}
