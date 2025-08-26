use serde_json::Value;
use std::env;

use crate::lyrics::types::{LyricLine, LyricsError, ProviderResult, http_client};

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
        // Request richsync optional call so `track.richsync.get` (and `richsync_body`) may be included
        ("optional_calls", "track.richsync"),
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
                // Try to access richsync_body
                let richsync_body_opt = body
                    .get("richsync")
                    .and_then(|r| r.get("richsync_body"))
                    .and_then(|v| v.as_str());
                if let Some(richsync_body) = richsync_body_opt {
                    if let Some((parsed, raw)) = crate::lyrics::parse::parse_richsync_body(richsync_body) {
                        return Ok((parsed, Some(raw)));
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
