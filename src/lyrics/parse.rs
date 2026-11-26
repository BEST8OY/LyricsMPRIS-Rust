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

/// Parse Musixmatch subtitle_body (either LRC text or JSON array) into lyric lines.
pub fn parse_subtitle_body(subtitle_body: &str) -> Option<Vec<LyricLine>> {
    let trimmed = subtitle_body.trim();
    if trimmed.starts_with('[') && trimmed.as_bytes().get(1).is_some_and(u8::is_ascii_digit) {
        let parsed = parse_synced_lyrics(trimmed);
        return (!parsed.is_empty()).then_some(parsed);
    }

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
    (!parsed.is_empty()).then_some(parsed)
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
        let x_opt = line.get("x").and_then(|v| v.as_str());
        let raw_text = x_opt
            .or_else(|| line.get("text").and_then(|v| v.as_str()))
            .unwrap_or("♪");

        let words = line
            .get("l")
            .and_then(|v| v.as_array())
            .and_then(|char_arr| {
                parse_character_array(
                    &char_arr[..char_arr.len().min(MAX_WORDS_PER_LINE)],
                    line_start,
                    line_end,
                    x_opt,
                )
            });
        let text = if let Some(ref ws) = words
            && !ws.is_empty()
        {
            join_words(ws)
        } else {
            raw_text.to_string()
        };
        if text.is_empty() {
            continue;
        }
        parsed.push(
            LyricLine::new(line_start, text)
                .with_end(Some(line_end))
                .with_words(words),
        );
    }

    Some(parsed)
}

fn join_words(words: &[WordTiming]) -> String {
    let mut out = String::new();
    for (i, w) in words.iter().enumerate() {
        out.push_str(&w.text);
        if w.ends_with_space && i + 1 < words.len() {
            out.push(' ');
        }
    }
    out
}

fn match_x_cursor(x: &str, cursor: &mut usize, chunk: &str) -> Option<(String, bool)> {
    while *cursor < x.len() {
        let Some(ch) = x[*cursor..].chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            *cursor += ch.len_utf8();
        } else {
            break;
        }
    }
    if *cursor >= x.len() || chunk.is_empty() {
        return None;
    }

    let mut x_chars = x[*cursor..].char_indices();
    let mut match_end_rel = 0usize;
    for c_chunk in chunk.chars() {
        let (_, c_x) = x_chars.next()?;
        if !c_x.to_lowercase().eq(c_chunk.to_lowercase()) {
            return None;
        }
        match_end_rel += c_x.len_utf8();
    }

    let end_abs = *cursor + match_end_rel;
    let matched = x[*cursor..end_abs].to_string();
    let next_is_space = x[end_abs..].chars().next().is_some_and(char::is_whitespace);
    *cursor = end_abs;
    Some((matched, next_is_space))
}

fn parse_character_array(
    char_arr: &[Value],
    line_start: f64,
    line_end: f64,
    x_text: Option<&str>,
) -> Option<Vec<WordTiming>> {
    let mut word_timings: Vec<WordTiming> = Vec::new();
    let mut x_cursor = 0usize;

    for (i, elem) in char_arr.iter().enumerate() {
        let raw_chunk = elem.get("c").and_then(|v| v.as_str()).unwrap_or("");
        if raw_chunk.trim().is_empty() {
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

        let raw_ends_with_space = raw_chunk.ends_with([' ', '\t']);
        let stripped = raw_chunk.trim_end_matches([' ', '\t']);
        let (final_text, mut ends_with_space) = if let Some(x) = x_text
            && !x.is_empty()
            && let Some((matched, x_space)) = match_x_cursor(x, &mut x_cursor, stripped)
        {
            (matched, x_space)
        } else {
            (stripped.to_string(), raw_ends_with_space)
        };

        if !ends_with_space
            && char_arr
                .get(i + 1)
                .and_then(|next| next.get("c").and_then(|v| v.as_str()))
                .is_some_and(|s| s.chars().any(char::is_whitespace))
        {
            ends_with_space = true;
        }

        word_timings
            .push(create_word_timing(start, final_end, &final_text).with_space(ends_with_space));
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

    #[test]
    fn test_richsync_reconciles_chunk_casing_and_spacing_from_x() {
        let json = r#"[{"ts":10.0,"te":12.0,"l":[{"c":"hello","o":0.0},{"c":"world","o":0.5}],"x":"Hello World"}]"#;
        let parsed = parse_richsync_body(json).unwrap();
        assert_eq!(parsed[0].text, "Hello World");
        let words = parsed[0].words.as_ref().unwrap();
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "Hello");
        assert!(words[0].ends_with_space);
        assert_eq!(words[1].text, "World");
        assert!(!words[1].ends_with_space);
    }

    #[test]
    fn test_parse_subtitle_body_supports_lrc_and_json() {
        let lrc_sub = "[00:10.50] First line\n[00:15.25] Second line";
        let parsed_lrc = parse_subtitle_body(lrc_sub).unwrap();
        assert_eq!(parsed_lrc.len(), 2);
        assert_eq!(parsed_lrc[0].text, "First line");

        let json_sub = r#"[{"text":"Line A","time":{"total":5.5}}]"#;
        let parsed_json = parse_subtitle_body(json_sub).unwrap();
        assert_eq!(parsed_json.len(), 1);
        assert_eq!(parsed_json[0].text, "Line A");
        assert!((parsed_json[0].time - 5.5).abs() < 1e-6);
    }
}
