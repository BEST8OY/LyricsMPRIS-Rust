//! HTTP request execution and response handling for Musixmatch API.

use super::extract::{
    extract_track_ids_from_json, extract_track_ids_from_macro, get_root_status_code,
};
use crate::lyrics::types::{LyricLine, LyricsError, TrackMatchInfo};
use reqwest::Client;
use serde_json::Value;

pub(crate) const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Electron/1.7.6 Safari/537.36";

/// Outcome of a single token's fetch attempt.
#[derive(Debug)]
pub(crate) enum FetchOutcome {
    /// Successfully found lyrics: (parsed lines, raw response content, track identifiers)
    Success(Vec<LyricLine>, Option<String>, TrackMatchInfo),
    /// Track was not found on Musixmatch (200 status but empty list/no lyrics).
    TrackNotFound,
    /// A token-specific issue (401 unauthorized, 402 payment, 403 forbidden, 429 rate limit).
    TokenError(String),
}

/// Returns true if the status code indicates a token/auth/quota/rate-limit error.
pub(crate) fn is_token_error_code(code: i64) -> bool {
    matches!(code, 401 | 402 | 403 | 429)
}

/// Returns true if the HTTP response status indicates a token/auth/quota/rate-limit error.
pub(crate) fn is_token_error_status(status: reqwest::StatusCode) -> bool {
    let code = status.as_u16();
    matches!(code, 401 | 402 | 403 | 429)
}

/// Check if a macro response has a successful status code (200).
pub(crate) fn is_success(macro_calls: &Value, endpoint: &str) -> bool {
    macro_calls
        .get(endpoint)
        .and_then(|v| v.pointer("/message/header/status_code"))
        .and_then(|v| v.as_i64())
        .map(|code| code == 200)
        .unwrap_or(false)
}

/// Try to call macro.subtitles.get and extract richsync or subtitle_body.
pub(crate) async fn try_macro_for_lyrics(
    client: &Client,
    params: &[(String, String)],
) -> Result<FetchOutcome, LyricsError> {
    let macro_base = "https://apic-desktop.musixmatch.com/ws/1.1/macro.subtitles.get?format=json&namespace=lyrics_richsynched&subtitle_format=mxm&optional_calls=track.richsync&app_id=web-desktop-app-v1.0&";
    let macro_url = macro_base.to_string()
        + &params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

    let macro_resp = client
        .get(&macro_url)
        .header("User-Agent", DEFAULT_USER_AGENT)
        .header("Cookie", "x-mxm-token-guid=")
        .send()
        .await?;

    if is_token_error_status(macro_resp.status()) {
        return Ok(FetchOutcome::TokenError(format!(
            "HTTP {}",
            macro_resp.status()
        )));
    }

    if !macro_resp.status().is_success() {
        return Ok(FetchOutcome::TrackNotFound);
    }

    let macro_json: Value = macro_resp.json().await?;
    if let Some(code) = get_root_status_code(&macro_json)
        && is_token_error_code(code)
    {
        return Ok(FetchOutcome::TokenError(format!("API Status {}", code)));
    }

    let macro_calls = macro_json.pointer("/message/body/macro_calls");

    if let Some(calls) = macro_calls {
        let track_ids = extract_track_ids_from_macro(calls);

        // Prefer richsync (word-level timing) if available
        if is_success(calls, "track.richsync.get")
            && let Some(richsync_body) = calls
                .pointer("/track.richsync.get/message/body/richsync/richsync_body")
                .and_then(|v| v.as_str())
            && let Some(parsed) = crate::lyrics::parse::parse_richsync_body(richsync_body)
        {
            return Ok(FetchOutcome::Success(
                parsed,
                Some(richsync_body.to_string()),
                track_ids,
            ));
        }

        // Fall back to subtitles (line-level timing)
        if is_success(calls, "track.subtitles.get")
            && let Some(subtitle_body) = calls
                .pointer("/track.subtitles.get/message/body/subtitle_list/0/subtitle/subtitle_body")
                .and_then(|v| v.as_str())
            && let Some(parsed) = crate::lyrics::parse::parse_subtitle_body(subtitle_body)
        {
            return Ok(FetchOutcome::Success(
                parsed,
                Some(subtitle_body.to_string()),
                track_ids,
            ));
        }
    }

    Ok(FetchOutcome::TrackNotFound)
}

/// Fetch lyrics with a single token.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fetch_lyrics_with_token(
    client: &Client,
    token: &str,
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    track_spotify_id: Option<&str>,
    track_isrc: Option<&str>,
    track_itunes_id: Option<&str>,
) -> Result<FetchOutcome, LyricsError> {
    // Strategy 1: If we have track identifiers, try direct lookup first
    // Try in order: Spotify ID, ISRC, iTunes ID
    if let Some(sid) = track_spotify_id {
        let mut params = vec![
            ("track_spotify_id".to_string(), sid.to_string()),
            ("usertoken".to_string(), token.to_string()),
        ];
        if let Some(len) = duration.map(|d| d.round() as i64) {
            params.push(("q_duration".to_string(), len.to_string()));
        }

        match try_macro_for_lyrics(client, &params).await? {
            FetchOutcome::Success(parsed, raw, ids) => {
                return Ok(FetchOutcome::Success(parsed, raw, ids));
            }
            FetchOutcome::TokenError(reason) => return Ok(FetchOutcome::TokenError(reason)),
            FetchOutcome::TrackNotFound => {} // fall through to next lookup
        }
    }

    if let Some(isrc) = track_isrc {
        let mut params = vec![
            ("track_isrc".to_string(), isrc.to_string()),
            ("usertoken".to_string(), token.to_string()),
        ];
        if let Some(len) = duration.map(|d| d.round() as i64) {
            params.push(("q_duration".to_string(), len.to_string()));
        }

        match try_macro_for_lyrics(client, &params).await? {
            FetchOutcome::Success(parsed, raw, ids) => {
                return Ok(FetchOutcome::Success(parsed, raw, ids));
            }
            FetchOutcome::TokenError(reason) => return Ok(FetchOutcome::TokenError(reason)),
            FetchOutcome::TrackNotFound => {} // fall through to next lookup
        }
    }

    if let Some(itunes) = track_itunes_id {
        let mut params = vec![
            ("track_itunes_id".to_string(), itunes.to_string()),
            ("usertoken".to_string(), token.to_string()),
        ];
        if let Some(len) = duration.map(|d| d.round() as i64) {
            params.push(("q_duration".to_string(), len.to_string()));
        }

        match try_macro_for_lyrics(client, &params).await? {
            FetchOutcome::Success(parsed, raw, ids) => {
                return Ok(FetchOutcome::Success(parsed, raw, ids));
            }
            FetchOutcome::TokenError(reason) => return Ok(FetchOutcome::TokenError(reason)),
            FetchOutcome::TrackNotFound => {} // fall through to search
        }
    }

    // Strategy 2: Use matcher.track.get for matching (musixmatch's own fuzzy matcher),
    // then fetch lyrics via macro.subtitles.get with the matched commontrack_id.
    let matcher_base = "https://apic-desktop.musixmatch.com/ws/1.1/matcher.track.get?format=json&app_id=web-desktop-app-v1.0&";
    let mut matcher_params = vec![
        format!("q_artist={}", urlencoding::encode(artist)),
        format!("q_track={}", urlencoding::encode(title)),
        format!("usertoken={}", urlencoding::encode(token)),
    ];

    if !album.is_empty() {
        matcher_params.push(format!("q_album={}", urlencoding::encode(album)));
    }
    if let Some(d) = duration {
        matcher_params.push(format!("q_duration={}", d.round() as i64));
    }

    let matcher_url = matcher_base.to_string() + &matcher_params.join("&");
    let matcher_resp = client
        .get(&matcher_url)
        .header("User-Agent", DEFAULT_USER_AGENT)
        .header("Cookie", "x-mxm-token-guid=")
        .send()
        .await?;

    if is_token_error_status(matcher_resp.status()) {
        return Ok(FetchOutcome::TokenError(format!(
            "Matcher HTTP {}",
            matcher_resp.status()
        )));
    }

    if !matcher_resp.status().is_success() {
        return Ok(FetchOutcome::TrackNotFound);
    }

    let matcher_json: Value = matcher_resp.json().await?;
    if let Some(code) = get_root_status_code(&matcher_json)
        && is_token_error_code(code)
    {
        return Ok(FetchOutcome::TokenError(format!(
            "Matcher API Status {}",
            code
        )));
    }

    let track = matcher_json.pointer("/message/body/track");
    let Some(track) = track else {
        return Ok(FetchOutcome::TrackNotFound);
    };

    // Extract track identifiers from the matcher response
    let ids = extract_track_ids_from_json(track);

    // Check if track is instrumental
    if track
        .get("instrumental")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let line = LyricLine {
            time: 0.0,
            text: "♪ Instrumental ♪".to_string(),
            words: None,
        };
        return Ok(FetchOutcome::Success(vec![line], None, ids));
    }

    // Check if track has lyrics before attempting to fetch them
    if !track
        .get("has_lyrics")
        .and_then(|v| v.as_i64())
        .map(|v| v == 1)
        .unwrap_or(false)
    {
        return Ok(FetchOutcome::TrackNotFound);
    }

    // Fetch lyrics using commontrack_id from the matcher
    if let Some(commontrack_id) = track.get("commontrack_id").and_then(|v| v.as_i64()) {
        let mut params = vec![
            ("commontrack_id".to_string(), commontrack_id.to_string()),
            ("usertoken".to_string(), token.to_string()),
        ];

        if let Some(d) = duration {
            params.push(("q_duration".to_string(), (d.round() as i64).to_string()));
        }

        match try_macro_for_lyrics(client, &params).await? {
            FetchOutcome::Success(parsed, raw, _macro_ids) => {
                return Ok(FetchOutcome::Success(parsed, raw, ids));
            }
            FetchOutcome::TokenError(reason) => return Ok(FetchOutcome::TokenError(reason)),
            FetchOutcome::TrackNotFound => {}
        }
    }

    Ok(FetchOutcome::TrackNotFound)
}
