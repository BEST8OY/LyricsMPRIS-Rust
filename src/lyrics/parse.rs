use crate::lyrics::types::LyricLine;
use unicode_segmentation::UnicodeSegmentation;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

static SYNCED_LYRICS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[(\d{1,2}):(\d{2})[.](\d{1,2})\]").unwrap());

/// Parse time-synced lyrics into LyricLine structs.
pub fn parse_synced_lyrics(synced: &str) -> Vec<LyricLine> {
    let re = &SYNCED_LYRICS_RE;
    let mut lines = Vec::new();
    for line in synced.lines() {
        let matches: Vec<_> = re.captures_iter(line).collect();
        if matches.is_empty() {
            continue;
        }
        let text = re.replace_all(line, "").trim().to_string();
        if text.is_empty() {
            continue;
        }
        for cap in matches {
            let min = cap
                .get(1)
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let sec = cap
                .get(2)
                .and_then(|s| s.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let centi = cap
                .get(3)
                .and_then(|c| c.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let time = min as f64 * 60.0 + sec as f64 + centi as f64 / 100.0;
            lines.push(LyricLine {
                time,
                text: text.clone(),
                words: None,
            });
        }
    }
    lines
}

/// Try to parse a musixmatch "richsync_body" JSON string into lyric lines with optional per-word timings.
/// Returns Some((lines, raw_lrc_with_marker)) on success, or None if parsing/shape doesn't match.
pub fn parse_richsync_body(richsync_body: &str) -> Option<(Vec<LyricLine>, String)> {
    if let Ok(lines_val) = serde_json::from_str::<Value>(richsync_body)
        && let Some(arr) = lines_val.as_array() {
            let mut parsed = Vec::new();
            let mut out = String::new();
            for line in arr.iter() {
                let t = line.pointer("/ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let te = line.pointer("/te").and_then(|v| v.as_f64()).unwrap_or(t + 3.0);
                let text = line
                    .get("x")
                    .and_then(|v| v.as_str())
                    .or_else(|| line.get("text").and_then(|v| v.as_str()))
                    .unwrap_or("\u{266a}");
                let ms = (t * 1000.0).round() as u64;
                let minutes = ms / 60000;
                let seconds = (ms % 60000) / 1000;
                let centi = ms % 1000 / 10;
                out.push_str(&format!("[{:02}:{:02}.{:02}]{}\n", minutes, seconds, centi, text));

                // Parse per-word timings. Two possible richsync shapes:
                // - explicit `words` array with {start,end,text}
                // - character-level `l` array with {c, o} items (offsets from ts)
                let words = if let Some(words_arr) = line.get("words").and_then(|v| v.as_array()) {
                    let mut wts = Vec::new();
                    for w in words_arr {
                        let start = w.get("start").and_then(|v| v.as_f64()).unwrap_or(t);
                        let mut end = w.get("end").and_then(|v| v.as_f64()).unwrap_or(start);
                        // Ensure end is after start; if not, fall back to the line end `te`.
                        if end <= start {
                            end = te;
                        }
                        let wtext = w.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        // Precompute grapheme clusters and byte offsets for efficient slicing
                        let graphemes: Vec<String> = UnicodeSegmentation::graphemes(wtext.as_str(), true)
                            .map(|g| g.to_string())
                            .collect();
                        let mut offsets = Vec::with_capacity(graphemes.len());
                        let mut acc = 0usize;
                        for g in &graphemes {
                            offsets.push(acc);
                            acc += g.len();
                        }
                        wts.push(crate::lyrics::types::WordTiming { start, end, text: wtext, graphemes, grapheme_byte_offsets: offsets });
                    }
                    if wts.is_empty() { None } else { Some(wts) }
                } else if let Some(l_arr) = line.get("l").and_then(|v| v.as_array()) {
                    let mut wts = Vec::new();
                    for (i, elem) in l_arr.iter().enumerate() {
                        let word_text = elem.get("c").and_then(|v| v.as_str()).unwrap_or("");
                        if word_text.trim().is_empty() {
                            continue; // Skip spaces
                        }

                        let start_offset = elem.get("o").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let start = t + start_offset;

                        // The end time is the start time of the next element, or the line's end time.
                        let end = l_arr.get(i + 1)
                            .and_then(|next_elem| next_elem.get("o").and_then(|v| v.as_f64()))
                            .map(|end_offset| t + end_offset)
                            .unwrap_or(te);

                        // Ensure end is after start; if not, fall back to the line end `te`.
                        let final_end = if end <= start { te } else { end };

                        let graphemes: Vec<String> = UnicodeSegmentation::graphemes(word_text, true)
                            .map(|g| g.to_string())
                            .collect();
                        let mut offsets = Vec::with_capacity(graphemes.len());
                        let mut acc = 0usize;
                        for g in &graphemes {
                            offsets.push(acc);
                            acc += g.len();
                        }

                        wts.push(crate::lyrics::types::WordTiming {
                            start,
                            end: final_end,
                            text: word_text.to_string(),
                            graphemes,
                            grapheme_byte_offsets: offsets,
                        });
                    }
                    if wts.is_empty() { None } else { Some(wts) }
                } else {
                    None
                };

                parsed.push(LyricLine { time: t, text: text.to_string(), words });
            }
            let out_with_marker = format!(";;richsync=1\n{}", out);
            return Some((parsed, out_with_marker));
        }
    None
}
