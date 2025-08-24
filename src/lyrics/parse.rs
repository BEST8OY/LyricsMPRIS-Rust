use crate::lyrics::types::LyricLine;
use once_cell::sync::Lazy;
use regex::Regex;

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
            });
        }
    }
    lines
}
