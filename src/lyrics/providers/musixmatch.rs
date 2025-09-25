use serde_json::Value;
use std::env;
use reqwest::Client;

use crate::lyrics::types::{http_client, LyricLine, ProviderResult};

/// Fetch lyrics using Musixmatch desktop "usertoken" (apic-desktop.musixmatch.com).
#[allow(dead_code)]
pub async fn fetch_lyrics_from_musixmatch_usertoken(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    track_spotify_id: Option<&str>,
) -> ProviderResult {
    // Requirements: a usertoken must be present.
    let token = match env::var("MUSIXMATCH_USERTOKEN").ok() {
        Some(t) if !t.is_empty() => t,
        _ => return Ok((Vec::new(), None)),
    };

    let client = http_client();

    // ...existing code...

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

    // Try to call macro.subtitles.get with the given params (key,value) pairs
    // and parse either an inline richsync payload or a subtitle_list into LRC.
    async fn try_macro_for_lyrics(
        client: &Client,
        token: &str,
        params: &[(String, String)],
    ) -> Result<Option<(Vec<LyricLine>, String)>, reqwest::Error> {
        let macro_base = "https://apic-desktop.musixmatch.com/ws/1.1/macro.subtitles.get?format=json&namespace=lyrics_richsynched&subtitle_format=mxm&optional_calls=track.richsync&app_id=web-desktop-app-v1.0&";
        let macro_url = macro_base.to_string()
            + &params
                .iter()
                .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
                .collect::<Vec<_>>()
                .join("&");

        let macro_resp = client
            .get(&macro_url)
            .header("Cookie", format!("x-mxm-token-guid={}", token))
            .send()
            .await?;

        if !macro_resp.status().is_success() {
            return Ok(None);
        }

        if let Ok(macro_json) = macro_resp.json::<Value>().await {
            let macro_calls = macro_json.pointer("/message/body/macro_calls").cloned().unwrap_or(Value::Null);
            if !macro_calls.is_null() {
                if let Some(rich) = macro_calls.get("track.richsync.get")
                    && status_code_at(rich, "/message/header/status_code") == 200
                    && let Some(body) = rich.pointer("/message/body")
                    && let Some(richsync_body) = body
                        .get("richsync")
                        .and_then(|r| r.get("richsync_body"))
                        .and_then(|v| v.as_str())
                    && let Some((parsed, raw)) = crate::lyrics::parse::parse_richsync_body(richsync_body) {
                    return Ok(Some((parsed, raw)));
                }

                if let Some(subs) = macro_calls.get("track.subtitles.get")
                    && status_code_at(subs, "/message/header/status_code") == 200
                    && let Some(list) = subs.pointer("/message/body/subtitle_list").and_then(|v| v.as_array())
                    && let Some(first) = list.first()
                    && let Some(sub_body) = first.pointer("/subtitle/subtitle_body").and_then(|v| v.as_str())
                    && let Ok(lines_val) = serde_json::from_str::<Value>(sub_body)
                    && let Some(arr) = lines_val.as_array() {
                    let (parsed, raw) = lrc_from_array(&arr.to_vec(), "/time/total");
                    return Ok(Some((parsed, raw)));
                }
            }
        }

        Ok(None)
    }


    // Primary flow: if we have a Spotify track id from MPRIS, prefer calling
    // macro.subtitles.get with track_spotify_id to avoid an extra track.search
    // and similarity checks. Otherwise fall back to the normal search+match
    // flow.
    {
        // If a spotify id is present, attempt a direct macro call first.
        if let Some(sid) = track_spotify_id {
            let mut params: Vec<(String, String)> = Vec::new();
            params.push(("track_spotify_id".to_string(), sid.to_string()));
            params.push(("usertoken".to_string(), token.clone()));
            if let Some(len) = duration.map(|d| d.round() as i64) {
                params.push(("q_duration".to_string(), len.to_string()));
            }
            if let Some((parsed, raw)) = try_macro_for_lyrics(client, &token, &params).await? {
                return Ok((parsed, Some(raw)));
            }
            // If the spotify-id macro lookup failed, fall through to search+match below.
        }

        // Primary flow: use track.search to obtain candidate tracks, pick the
        // most similar candidate with `find_best_song_match`, then prefer
        // richsync via a single macro.subtitles.get call for the selected commontrack_id.
        
        
        let matcher_base = "https://apic-desktop.musixmatch.com/ws/1.1/track.search?format=json&app_id=web-desktop-app-v1.0&";
        let mut mparts: Vec<String> = Vec::new();
        // prefer explicit artist/track fields when available
        mparts.push(format!("q_artist={}", urlencoding::encode(artist)));
        mparts.push(format!("q_track={}", urlencoding::encode(title)));
        if !album.is_empty() {
            mparts.push(format!("q_album={}", urlencoding::encode(album)));
        }
        if let Some(d) = duration {
            let secs = d.round() as i64;
            mparts.push(format!("q_duration={}", secs));
        }
        mparts.push(format!("usertoken={}", urlencoding::encode(&token)));
        // request more candidates for better similarity matching and prefer tracks with lyrics
        mparts.push("page_size=10".to_string());
        mparts.push("f_has_lyrics=1".to_string());

        let matcher_url = matcher_base.to_string() + &mparts.join("&");
        let mresp = client
            .get(&matcher_url)
            .header("Cookie", format!("x-mxm-token-guid={}", token))
            .send()
            .await?;

        if !mresp.status().is_success() {
            return Ok((Vec::new(), None));
        }

        let mjson: Value = mresp.json().await?;
        let list = mjson.pointer("/message/body/track_list").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if list.is_empty() {
            return Ok((Vec::new(), None));
        }

        // Build candidate array (each item is the inner `track` object)
        let mut candidates: Vec<Value> = Vec::with_capacity(list.len());
        for item in &list {
            if let Some(track) = item.get("track") {
                candidates.push(track.clone());
            }
        }

        if candidates.is_empty() {
            return Ok((Vec::new(), None));
        }

    // candidates collected

        // Use project's similarity helper to pick best match
        if let Some((idx, _score)) = crate::lyrics::similarity::find_best_song_match(
            &candidates,
            title,
            artist,
            if album.is_empty() { None } else { Some(album) },
            duration,
        ) {
            if let Some(best) = candidates.get(idx) {
                // best candidate obtained
                // If candidate is instrumental, return a single instrumental line
                if best.get("instrumental").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let line = LyricLine { time: 0.0, text: "♪ Instrumental ♪".to_string(), words: None };
                    return Ok((vec![line], None));
                }

                let commontrack_id = best
                    .get("commontrack_id")
                    .and_then(|v| v.as_i64())
                    .or_else(|| best.get("track_id").and_then(|v| v.as_i64()));
                let track_len = best
                    .get("track_length")
                    .and_then(|v| v.as_i64())
                    .or_else(|| best.get("length").and_then(|v| v.as_i64()));

                if let Some(ctid) = commontrack_id {
                    // Prefer a single macro.subtitles.get call requesting richsync
                    // and subtitles in one payload (lower latency, single roundtrip).
                    let mut params: Vec<(String, String)> = Vec::new();
                    params.push(("commontrack_id".to_string(), ctid.to_string()));
                    params.push(("usertoken".to_string(), token.clone()));
                    if let Some(len) = track_len {
                        params.push(("q_duration".to_string(), len.to_string()));
                    }
                    if let Some((parsed, raw)) = try_macro_for_lyrics(client, &token, &params).await? {
                        return Ok((parsed, Some(raw)));
                    }
                }
            }
        }
    }

    Ok((Vec::new(), None))
}
