use serde::Deserialize;
use serde_json::Value;
use std::env;

use crate::lyrics::parse::parse_synced_lyrics;
use crate::lyrics::types::{LyricLine, LyricsError, ProviderResult, http_client};

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct LrcLibResp {
    syncedLyrics: Option<String>,
}

/// Fetch lyrics from lrclib for a given artist and title.
pub async fn fetch_lyrics_from_lrclib(artist: &str, title: &str) -> ProviderResult {
    let client = http_client();
    let url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        urlencoding::encode(artist),
        urlencoding::encode(title)
    );
    let resp = client
        .get(&url)
        .header("User-Agent", "LyricsMPRIS/1.0")
        .send()
        .await?;
    if resp.status().as_u16() == 404 {
        return Ok((Vec::new(), None));
    }
    if !resp.status().is_success() {
        return Err(LyricsError::Api(format!(
            "lrclib: unexpected status {}",
            resp.status()
        )));
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

/// Fetch lyrics using Musixmatch desktop "usertoken" (apic-desktop.musixmatch.com).
#[allow(dead_code)]
pub async fn fetch_lyrics_from_musixmatch_usertoken(artist: &str, title: &str) -> ProviderResult {
    let token = match env::var("MUSIXMATCH_USERTOKEN").ok() {
        Some(t) if !t.is_empty() => t,
        _ => return Ok((Vec::new(), None)),
    };

    let client = http_client();

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

    let matcher_status = macro_calls
        .pointer("/matcher.track.get/message/header/status_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if matcher_status != 200 {
        return Ok((Vec::new(), None));
    }

    // Fallback simple per-line formatter (keeps old behavior)
    let make_lrc_from_array = |arr: &Vec<Value>, time_key: &str| -> (Vec<LyricLine>, String) {
        let mut out = String::new();
        let mut parsed = Vec::new();
        for line in arr {
            let t = line
                .pointer(time_key)
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let text = line
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("\u{266a}");
            let ms = (t * 1000.0).round() as u64;
            let minutes = ms / 60000;
            let seconds = (ms % 60000) / 1000;
            let centi = ms % 1000 / 10;
            out.push_str(&format!("[{:02}:{:02}.{:02}]{}\n", minutes, seconds, centi, text));
            parsed.push(LyricLine { time: t, text: text.to_string(), words: None });
        }
        (parsed, out)
    };

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
                            // Parse per-word timings when available. Expected format is an array of lines,
                            // where each line can contain `text` and `words` array with start/finish times.
                            let mut parsed = Vec::new();
                            let mut out = String::new();
                            for line in arr {
                                let t = line.pointer("/ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                let text = line.get("text").and_then(|v| v.as_str()).unwrap_or("\u{266a}");
                                // Build raw LRC line
                                let ms = (t * 1000.0).round() as u64;
                                let minutes = ms / 60000;
                                let seconds = (ms % 60000) / 1000;
                                let centi = ms % 1000 / 10;
                                out.push_str(&format!("[{:02}:{:02}.{:02}]{}\n", minutes, seconds, centi, text));

                                // Parse words array if present
                                let words = if let Some(words_arr) = line.get("words").and_then(|v| v.as_array()) {
                                    let mut wts = Vec::new();
                                    for w in words_arr {
                                        let start = w.get("start").and_then(|v| v.as_f64()).unwrap_or(t);
                                        let end = w.get("end").and_then(|v| v.as_f64()).unwrap_or(start);
                                        let wtext = w.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        wts.push(crate::lyrics::types::WordTiming { start, end, text: wtext });
                                    }
                                    if wts.is_empty() { None } else { Some(wts) }
                                } else {
                                    None
                                };

                                parsed.push(LyricLine { time: t, text: text.to_string(), words });
                            }
                            return Ok((parsed, Some(out)));
                        }
                    }
                }
            }
        }
    }

    if let Some(subs) = macro_calls.get("track.subtitles.get") {
        let status = subs
            .pointer("/message/header/status_code")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if status == 200
            && let Some(list) = subs
                .pointer("/message/body/subtitle_list")
                .and_then(|v| v.as_array())
            && let Some(first) = list.first()
            && let Some(sub_body) = first
                .pointer("/subtitle/subtitle_body")
                .and_then(|v| v.as_str())
            && let Ok(lines_val) = serde_json::from_str::<Value>(sub_body)
            && let Some(arr) = lines_val.as_array()
        {
            let (parsed, raw) = make_lrc_from_array(&arr.to_vec(), "/time/total");
            return Ok((parsed, Some(raw)));
        }
    }

    Ok((Vec::new(), None))
}
