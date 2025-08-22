// lyrics.rs: Lyric fetching, parsing, and time-synced logic

use reqwest::Client;
use regex::Regex;
use thiserror::Error;
use serde::Deserialize;
use once_cell::sync::Lazy;
use std::env;
use serde_json::Value;

// Shared HTTP client with reasonable defaults for timeouts
static HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .user_agent("LyricsMPRIS/1.0")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client")
});

/// Provider result: parsed lines plus optional raw LRC string for DB storage
pub type ProviderResult = Result<(Vec<LyricLine>, Option<String>), LyricsError>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LyricLine {
    pub time: f64,
    pub text: String,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct LrcLibResp {
    syncedLyrics: Option<String>,
}

#[derive(Error, Debug)]
pub enum LyricsError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("API error: {0}")]
    Api(String),
    #[error("Serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Fetch lyrics from lrclib for a given artist and title.
pub async fn fetch_lyrics_from_lrclib(artist: &str, title: &str) -> ProviderResult {
    let client = &*HTTP_CLIENT;
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
        return Ok((Vec::new(), None));
    }
    if !resp.status().is_success() {
        return Err(LyricsError::Api(format!("lrclib: unexpected status {}", resp.status())));
    }
    let api: LrcLibResp = resp.json().await?;
    let synced = api.syncedLyrics.unwrap_or_default();
    if synced.is_empty() {
        Ok((Vec::new(), None))
    } else {
        let parsed = parse_synced_lyrics(&synced);
        Ok((parsed, Some(synced)))
    }
}

// Musixmatch API-key based implementation removed in favor of the desktop
// usertoken-based provider. See `fetch_lyrics_from_musixmatch_usertoken`.

/// Fetch lyrics using Musixmatch desktop "usertoken" (apic-desktop.musixmatch.com).
///
/// This mirrors the JS provider flow: call macro.subtitles.get, check the
/// matcher.track.get status, then prefer richsync -> subtitles.
/// Returns an LRC-formatted string for synced results. If no synced data is
/// available this returns an empty string (we deliberately do not return
/// plain/unsynced lyrics).
#[allow(dead_code)]
pub async fn fetch_lyrics_from_musixmatch_usertoken(
    artist: &str,
    title: &str,
) -> ProviderResult {
    // Read token from env; treat missing token as "no provider configured"
    let token = match env::var("MUSIXMATCH_USERTOKEN").ok() {
        Some(t) if !t.is_empty() => t,
        _ => return Ok((Vec::new(), None)),
    };

    let client = &*HTTP_CLIENT;

    let base_url = "https://apic-desktop.musixmatch.com/ws/1.1/macro.subtitles.get?format=json&namespace=lyrics_richsynched&subtitle_format=mxm&app_id=web-desktop-app-v1.0&";

    let params = [
        ("q_artist", artist),
        ("q_track", title),
        ("usertoken", &token),
    ];

    let final_url = base_url.to_string()
        + &params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

    let resp = client
        .get(&final_url)
        // Some desktop endpoints expect the token also as a cookie header
        .header("Cookie", format!("x-mxm-token-guid={}", token))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(LyricsError::Api(format!(
            "musixmatch desktop macro.subtitles.get: {}",
            resp.status()
        )));
    }

    let json: Value = resp.json().await?;

    let macro_calls = json
        .pointer("/message/body/macro_calls")
        .cloned()
        .unwrap_or(Value::Null);

    if macro_calls.is_null() {
        return Ok((Vec::new(), None));
    }

    // Ensure matcher.track.get succeeded (status_code == 200)
    let matcher_status = macro_calls
        .pointer("/matcher.track.get/message/header/status_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if matcher_status != 200 {
        return Ok((Vec::new(), None));
    }

    // Helper to format an array of lines into LRC string and parsed Vec<LyricLine>
    let make_lrc_from_array = |arr: &Vec<Value>, time_key: &str| -> (Vec<LyricLine>, String) {
        let mut out = String::new();
        let mut parsed = Vec::new();
        for line in arr {
            let t = line.pointer(time_key).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let text = line.get("text").and_then(|v| v.as_str()).unwrap_or("\u{266a}");
            let ms = (t * 1000.0).round() as u64;
            let minutes = ms / 60000;
            let seconds = (ms % 60000) / 1000;
            let centi = ms % 1000 / 10;
            out.push_str(&format!("[{:02}:{:02}.{:02}]{}\n", minutes, seconds, centi, text));
            parsed.push(LyricLine { time: t, text: text.to_string() });
        }
        (parsed, out)
    };

    // 1) Try richsync (word-level karaoke)
    if let Some(rich) = macro_calls.get("track.richsync.get") {
        let status = rich
            .pointer("/message/header/status_code")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if status == 200 {
            if let Some(body) = rich.pointer("/message/body") {
                if let Some(richsync_body) = body
                    .get("richsync")
                    .and_then(|r| r.get("richsync_body"))
                    .and_then(|v| v.as_str())
                {
                    if let Ok(lines_val) = serde_json::from_str::<Value>(richsync_body) {
                        if let Some(arr) = lines_val.as_array() {
                            // create parsed lines using ts as total seconds
                            let (parsed, raw) = make_lrc_from_array(&arr.to_vec(), "/ts");
                            return Ok((parsed, Some(raw)));
                        }
                    }
                }
            }
        }
    }

    // 2) Try standard synced subtitles
    if let Some(subs) = macro_calls.get("track.subtitles.get") {
        let status = subs
            .pointer("/message/header/status_code")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if status == 200 {
            if let Some(list) = subs.pointer("/message/body/subtitle_list").and_then(|v| v.as_array()) {
                if let Some(first) = list.get(0) {
                    if let Some(sub_body) = first
                        .pointer("/subtitle/subtitle_body")
                        .and_then(|v| v.as_str())
                    {
                        if let Ok(lines_val) = serde_json::from_str::<Value>(sub_body) {
                            if let Some(arr) = lines_val.as_array() {
                                let (parsed, raw) = make_lrc_from_array(&arr.to_vec(), "/time/total");
                                return Ok((parsed, Some(raw)));
                            }
                        }
                    }
                }
            }
        }
    }

    // No synced content (richsync or subtitles) was available.
    Ok((Vec::new(), None))
}

static SYNCED_LYRICS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[(\d{1,2}):(\d{2})[.](\d{1,2})\]").unwrap()
});

/// Parse time-synced lyrics into LyricLine structs.
pub fn parse_synced_lyrics(synced: &str) -> Vec<LyricLine> {
    let re = &SYNCED_LYRICS_RE;
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
