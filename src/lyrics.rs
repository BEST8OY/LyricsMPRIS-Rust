// lyrics.rs: Lyric fetching, parsing, and time-synced logic

use reqwest::Client;
use regex::Regex;
use serde::Deserialize;
use std::error::Error;

#[derive(Debug, Clone, Default)]
pub struct LyricLine {
    pub time: f64,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct Lyric {
    #[allow(dead_code)]
    pub lines: Vec<LyricLine>,
}

#[allow(dead_code)]
pub fn is_timesynced(lines: &[LyricLine]) -> bool {
    lines.len() > 1 && (lines[0].time > 0.0 || lines[1].time > 0.0)
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct LrcLibResp {
    plainLyrics: String,
    syncedLyrics: Option<String>,
}

/// Fetch lyrics from lrclib for a given artist and title.
pub async fn fetch_lyrics_from_lrclib(artist: &str, title: &str) -> Result<(String, String), Box<dyn Error>> {
    let client = Client::new();
    let url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        urlencoding::encode(artist),
        urlencoding::encode(title)
    );
    let resp = client.get(&url)
        .header("User-Agent", "LyricsMPRIS/1.0")
        .send().await?;
    if resp.status().as_u16() == 404 {
        // No lyrics found, not an error
        return Ok((String::new(), String::new()));
    }
    if !resp.status().is_success() {
        return Err(format!("lrclib: unexpected status {}", resp.status()).into());
    }
    let api: LrcLibResp = resp.json().await?;
    Ok((api.plainLyrics, api.syncedLyrics.unwrap_or_default()))
}

/// Parse time-synced lyrics into LyricLine structs.
pub fn parse_synced_lyrics(synced: &str) -> Vec<LyricLine> {
    // Correct regex: [mm:ss.xx] or [m:ss.xx]
    let re = Regex::new(r"\[(\d{1,2}):(\d{2})[.](\d{1,2})\]").unwrap();
    let mut lines = Vec::new();
    for line in synced.lines() {
        let matches: Vec<_> = re.captures_iter(line).collect();
        if matches.is_empty() { continue; }
        let text = re.replace_all(line, "").trim().to_string();
        if text.is_empty() { continue; }
        for cap in matches {
            // Defensive: parse as f64, fallback to 0.0
            let min = cap.get(1).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(0);
            let sec = cap.get(2).and_then(|s| s.as_str().parse::<u32>().ok()).unwrap_or(0);
            let centi = cap.get(3).and_then(|c| c.as_str().parse::<u32>().ok()).unwrap_or(0);
            let time = min as f64 * 60.0 + sec as f64 + centi as f64 / 100.0;
            lines.push(LyricLine { time, text: text.clone() });
        }
    }
    lines
}

/// Parse plain (unsynced) lyrics into lines (splits on newlines, trims each line)
pub fn parse_plain_lyrics(plain: &str) -> Vec<String> {
    plain
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}
