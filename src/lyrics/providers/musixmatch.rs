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

    /// Check if a macro response has a successful status code (200).
    fn is_success(macro_calls: &Value, endpoint: &str) -> bool {
        macro_calls
            .get(endpoint)
            .and_then(|v| v.pointer("/message/header/status_code"))
            .and_then(|v| v.as_i64())
            .map(|code| code == 200)
            .unwrap_or(false)
    }



    /// Try to call macro.subtitles.get and extract richsync or subtitle_body.
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

        let macro_json: Value = macro_resp.json().await?;
        let macro_calls = macro_json.pointer("/message/body/macro_calls");
        
        if let Some(calls) = macro_calls {
            // Prefer richsync (word-level timing) if available
            if is_success(calls, "track.richsync.get") {
                if let Some(richsync_body) = calls
                    .pointer("/track.richsync.get/message/body/richsync/richsync_body")
                    .and_then(|v| v.as_str())
                {
                    if let Some(parsed) = crate::lyrics::parse::parse_richsync_body(richsync_body) {
                        // Return parsed lines and the original JSON body
                        return Ok(Some((parsed, richsync_body.to_string())));
                    }
                }
            }

            // Fall back to subtitles (line-level timing)
            if is_success(calls, "track.subtitles.get") {
                if let Some(subtitle_body) = calls
                    .pointer("/track.subtitles.get/message/body/subtitle_list/0/subtitle/subtitle_body")
                    .and_then(|v| v.as_str())
                {
                    if let Some(parsed) = crate::lyrics::parse::parse_subtitle_body(subtitle_body) {
                        // Return parsed lines and the original JSON body
                        return Ok(Some((parsed, subtitle_body.to_string())));
                    }
                }
            }
        }

        Ok(None)
    }


    // Strategy 1: If we have a Spotify track ID, try direct lookup first
    if let Some(sid) = track_spotify_id {
        let mut params = vec![
            ("track_spotify_id".to_string(), sid.to_string()),
            ("usertoken".to_string(), token.clone()),
        ];
        if let Some(len) = duration.map(|d| d.round() as i64) {
            params.push(("q_duration".to_string(), len.to_string()));
        }
        
        if let Some((parsed, raw)) = try_macro_for_lyrics(&client, &token, &params).await? {
            return Ok((parsed, Some(raw)));
        }
    }

    // Strategy 2: Search by track metadata and use similarity matching
    let search_base = "https://apic-desktop.musixmatch.com/ws/1.1/track.search?format=json&app_id=web-desktop-app-v1.0&";
    let mut search_params = vec![
        format!("q_artist={}", urlencoding::encode(artist)),
        format!("q_track={}", urlencoding::encode(title)),
        format!("usertoken={}", urlencoding::encode(&token)),
        "page_size=10".to_string(),
        "f_has_lyrics=1".to_string(),
    ];
    
    if !album.is_empty() {
        search_params.push(format!("q_album={}", urlencoding::encode(album)));
    }
    if let Some(d) = duration {
        search_params.push(format!("q_duration={}", d.round() as i64));
    }

    let search_url = search_base.to_string() + &search_params.join("&");
    let search_resp = client
        .get(&search_url)
        .header("Cookie", format!("x-mxm-token-guid={}", token))
        .send()
        .await?;

    if !search_resp.status().is_success() {
        return Ok((Vec::new(), None));
    }

    let search_json: Value = search_resp.json().await?;
    let track_list = search_json
        .pointer("/message/body/track_list")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if track_list.is_empty() {
        return Ok((Vec::new(), None));
    }

    // Extract track objects from the track_list wrapper
    let candidates: Vec<Value> = track_list
        .iter()
        .filter_map(|item| item.get("track").cloned())
        .collect();

    if candidates.is_empty() {
        return Ok((Vec::new(), None));
    }

    // Find the best matching track using similarity scoring
    let best_match = crate::lyrics::similarity::find_best_song_match(
        &candidates,
        title,
        artist,
        if album.is_empty() { None } else { Some(album) },
        duration,
    );

    if let Some((idx, _score)) = best_match {
        if let Some(best) = candidates.get(idx) {
            // Check if track is instrumental
            if best.get("instrumental").and_then(|v| v.as_bool()).unwrap_or(false) {
                let line = LyricLine {
                    time: 0.0,
                    text: "♪ Instrumental ♪".to_string(),
                    words: None,
                };
                return Ok((vec![line], None));
            }

            // Try to fetch lyrics using commontrack_id
            if let Some(commontrack_id) = best
                .get("commontrack_id")
                .and_then(|v| v.as_i64())
                .or_else(|| best.get("track_id").and_then(|v| v.as_i64()))
            {
                let track_length = best
                    .get("track_length")
                    .and_then(|v| v.as_i64())
                    .or_else(|| best.get("length").and_then(|v| v.as_i64()));

                let mut params = vec![
                    ("commontrack_id".to_string(), commontrack_id.to_string()),
                    ("usertoken".to_string(), token.clone()),
                ];
                
                if let Some(len) = track_length {
                    params.push(("q_duration".to_string(), len.to_string()));
                }

                if let Some((parsed, raw)) = try_macro_for_lyrics(&client, &token, &params).await? {
                    return Ok((parsed, Some(raw)));
                }
            }
        }
    }

    Ok((Vec::new(), None))
}
