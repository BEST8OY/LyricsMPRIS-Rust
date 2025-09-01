use serde_json::Value;
use std::env;

use crate::lyrics::types::{http_client, LyricLine, LyricsError, ProviderResult};

/// Fetch lyrics using Musixmatch desktop "usertoken" (apic-desktop.musixmatch.com).
#[allow(dead_code)]
pub async fn fetch_lyrics_from_musixmatch_usertoken(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
) -> ProviderResult {
    // Requirements: a usertoken must be present.
    let token = match env::var("MUSIXMATCH_USERTOKEN").ok() {
        Some(t) if !t.is_empty() => t,
        _ => return Ok((Vec::new(), None)),
    };

    let client = http_client();

    // Helper closures/functions to reduce repeated code and improve readability.
    fn status_code_at(v: &Value, ptr: &str) -> i64 {
        v.pointer(ptr).and_then(|x| x.as_i64()).unwrap_or(0)
    }

    fn bool_or_num_at(v: &Value, ptr: &str) -> Option<bool> {
    v.pointer(ptr).and_then(|x| x.as_i64().map(|i| i == 1).or_else(|| x.as_bool()))
    }

    fn lrc_from_array(arr: &[Value], time_key: &str) -> (Vec<LyricLine>, String) {
        let mut out = String::new();
        let mut parsed = Vec::with_capacity(arr.len());
        for line in arr {
            let t = line.pointer(time_key).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let text = line.get("text").and_then(|v| v.as_str()).unwrap_or("\u{266a}");
            let ms = (t * 1000.0).round() as u64;
            let minutes = ms / 60000;
            let seconds = (ms % 60000) / 1000;
            let centi = (ms % 1000) / 10;
            out.push_str(&format!("[{:02}:{:02}.{:02}]{}\n", minutes, seconds, centi, text));
            parsed.push(LyricLine { time: t, text: text.to_string(), words: None });
        }
        (parsed, out)
    }

    async fn try_richsync_call(
        client: &reqwest::Client,
        token: &str,
        commontrack_id: i64,
        track_len: Option<i64>,
    ) -> Result<Option<(Vec<LyricLine>, Option<String>)>, LyricsError> {
        let rich_base = "https://apic-desktop.musixmatch.com/ws/1.1/track.richsync.get?format=json&subtitle_format=mxm&app_id=web-desktop-app-v1.0&";
        let mut parts: Vec<String> = Vec::new();
        if let Some(len) = track_len {
            parts.push(format!("f_subtitle_length={}", len));
            parts.push(format!("q_duration={}", len));
        }
        parts.push(format!("commontrack_id={}", commontrack_id));
        parts.push(format!("usertoken={}", urlencoding::encode(token)));
        let rich_url = rich_base.to_string() + &parts.join("&");

        let resp = client
            .get(&rich_url)
            .header("Cookie", format!("x-mxm-token-guid={}", token))
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Ok(None);
        }

        let rich_json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status_code_at(&rich_json, "/message/header/status_code") != 200 {
            return Ok(None);
        }

        if let Some(body) = rich_json.pointer("/message/body")
            && let Some(richsync_body) = body
                .get("richsync")
                .and_then(|r| r.get("richsync_body"))
                .and_then(|v| v.as_str())
            && let Some((parsed, raw)) = crate::lyrics::parse::parse_richsync_body(richsync_body) {
            return Ok(Some((parsed, Some(raw))));
        }

        Ok(None)
    }

    // Build the initial macro.subtitles.get URL
    let base_url = "https://apic-desktop.musixmatch.com/ws/1.1/macro.subtitles.get?format=json&namespace=lyrics_richsynched&subtitle_format=mxm&app_id=web-desktop-app-v1.0&";
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("q_artist={}", urlencoding::encode(artist)));
    parts.push(format!("q_track={}", urlencoding::encode(title)));
    if !album.is_empty() {
        parts.push(format!("q_album={}", urlencoding::encode(album)));
    }
    parts.push(format!("usertoken={}", urlencoding::encode(&token)));
    if let Some(d) = duration {
        let secs = d.round() as i64;
        parts.push(format!("q_duration={}", secs));
    }

    let final_url = base_url.to_string() + &parts.join("&");
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
    let macro_calls = json.pointer("/message/body/macro_calls").cloned().unwrap_or(Value::Null);
    if macro_calls.is_null() {
        return Ok((Vec::new(), None));
    }

    if status_code_at(&macro_calls, "/matcher.track.get/message/header/status_code") != 200 {
        let mode = macro_calls
            .pointer("/matcher.track.get/message/header/mode")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(LyricsError::Api(format!("Requested error: {}", mode)));
    }

    if macro_calls
        .pointer("/track.lyrics.get/message/body/lyrics/restricted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(LyricsError::Api(
            "Unfortunately we're not authorized to show these lyrics.".to_string(),
        ));
    }

    if macro_calls
        .pointer("/matcher.track.get/message/body/track/instrumental")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let line = LyricLine { time: 0.0, text: "♪ Instrumental ♪".to_string(), words: None };
        return Ok((vec![line], None));
    }

    // If matcher reports richsync, try the dedicated richsync call for better results.
    let has_richsync = bool_or_num_at(&macro_calls, "/matcher.track.get/message/body/track/has_richsync").unwrap_or(false);
    if has_richsync
        && let Some(track_body) = macro_calls.pointer("/matcher.track.get/message/body") {
        let track_len = track_body
            .pointer("/track/track_length")
            .and_then(|v| v.as_i64())
            .or_else(|| track_body.pointer("/track_length").and_then(|v| v.as_i64()));
        let commontrack_id = track_body
            .pointer("/track/commontrack_id")
            .and_then(|v| v.as_i64())
            .or_else(|| track_body.pointer("/commontrack_id").and_then(|v| v.as_i64()));

        if let Some(ctid) = commontrack_id
            && let Some(result) = try_richsync_call(client, &token, ctid, track_len).await? {
            return Ok(result);
        }
    }

    // If macro_calls contains a richsync payload inline, prefer that.
    if let Some(rich) = macro_calls.get("track.richsync.get")
        && status_code_at(rich, "/message/header/status_code") == 200
        && let Some(body) = rich.pointer("/message/body")
        && let Some(richsync_body) = body
            .get("richsync")
            .and_then(|r| r.get("richsync_body"))
            .and_then(|v| v.as_str())
        && let Some((parsed, raw)) = crate::lyrics::parse::parse_richsync_body(richsync_body) {
        return Ok((parsed, Some(raw)));
    }

    // Fallback: check for subtitle_list and convert to LRC-like lines
    if let Some(subs) = macro_calls.get("track.subtitles.get")
        && status_code_at(subs, "/message/header/status_code") == 200
        && let Some(list) = subs.pointer("/message/body/subtitle_list").and_then(|v| v.as_array())
        && let Some(first) = list.first()
        && let Some(sub_body) = first.pointer("/subtitle/subtitle_body").and_then(|v| v.as_str())
        && let Ok(lines_val) = serde_json::from_str::<Value>(sub_body)
        && let Some(arr) = lines_val.as_array() {
        let (parsed, raw) = lrc_from_array(&arr.to_vec(), "/time/total");
        return Ok((parsed, Some(raw)));
    }

    Ok((Vec::new(), None))
}
