//! HTTP request execution and response handling for Musixmatch API.

use super::extract::{extract_track_ids_from_macro, get_root_hint, get_root_status_code};
use super::token::DEFAULT_USER_AGENT;
use crate::lyrics::types::{LyricLine, LyricsError, TrackMatchInfo};
use reqwest::Client;
use serde_json::Value;

/// Outcome of a single token's fetch attempt.
#[derive(Debug, PartialEq)]
pub(crate) enum FetchOutcome {
    /// Successfully found lyrics: (parsed lines, raw response content, track identifiers)
    Success(Vec<LyricLine>, Option<String>, TrackMatchInfo),
    /// Track was not found on Musixmatch (200 status but empty list/no lyrics).
    TrackNotFound,
    /// A token-specific issue (401 unauthorized, 402 payment, 403 forbidden, 429 rate limit).
    TokenError { reason: String, is_renewable: bool },
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

/// Evaluates HTTP status code and optional body JSON to determine if a token/rate-limit error occurred.
/// Differentiates between renewable token expiration (`401` with `hint: "renew"`) and throttling.
pub(crate) fn evaluate_token_outcome(
    http_status: reqwest::StatusCode,
    json: Option<&Value>,
) -> Option<FetchOutcome> {
    if let Some(json) = json {
        let code = get_root_status_code(json);
        let hint = get_root_hint(json);
        if let Some(code) = code
            && is_token_error_code(code)
        {
            let is_renewable = code == 401 && hint.as_deref() == Some("renew");
            let reason = match hint {
                Some(h) => format!("API Status {code} (hint: {h})"),
                None => format!("API Status {code}"),
            };
            return Some(FetchOutcome::TokenError {
                reason,
                is_renewable,
            });
        }
    }

    if is_token_error_status(http_status) {
        return Some(FetchOutcome::TokenError {
            reason: format!("HTTP {http_status}"),
            is_renewable: false,
        });
    }

    None
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
    let macro_base = "https://apic-appmobile.musixmatch.com/ws/1.1/macro.subtitles.get?format=json&namespace=lyrics_richsynched&subtitle_format=mxm&optional_calls=track.richsync&app_id=mac-ios-v2.0&richsync_compact_type=words&";
    let macro_url = macro_base.to_string()
        + &params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

    let macro_resp = client
        .get(&macro_url)
        .header("User-Agent", DEFAULT_USER_AGENT)
        .header("X-User-Agent", DEFAULT_USER_AGENT)
        .header("Cookie", "x-mxm-token-guid=")
        .header("X-Cookie", "x-mxm-token-guid=")
        .header("x-mxm-app-version", "10.1.1")
        .header("Accept", "application/json")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await?;

    let macro_status = macro_resp.status();

    // Server-side failures are transient network-level issues, not "track not found"
    if macro_status.is_server_error() {
        return Err(LyricsError::Http(macro_status.as_u16()));
    }

    // Try reading JSON body even on HTTP error (e.g. 401/403/429) to inspect status and hint
    let macro_json: Option<Value> = macro_resp.json().await.ok();

    if let Some(outcome) = evaluate_token_outcome(macro_status, macro_json.as_ref()) {
        return Ok(outcome);
    }

    let Some(macro_json) = macro_json else {
        return Ok(FetchOutcome::TrackNotFound);
    };

    if !macro_status.is_success() {
        return Ok(FetchOutcome::TrackNotFound);
    }

    let macro_calls = macro_json.pointer("/message/body/macro_calls");

    if let Some(calls) = macro_calls {
        let track_ids = extract_track_ids_from_macro(calls);

        // Check instrumental status first
        let is_instrumental = calls
            .pointer("/matcher.track.get/message/body/track/instrumental")
            .map(|v| v.as_i64() == Some(1) || v.as_bool() == Some(true))
            .unwrap_or(false);

        if is_instrumental {
            let line = LyricLine::new(0.0, "♪ Instrumental ♪");
            return Ok(FetchOutcome::Success(vec![line], None, track_ids));
        }

        // Prefer richsync (word-level timing) if available
        if is_success(calls, "track.richsync.get")
            && let Some(richsync_body) = calls
                .pointer("/track.richsync.get/message/body/richsync/richsync_body")
                .and_then(|v| v.as_str())
            && let Some(mut parsed) = crate::lyrics::parse::parse_richsync_body(richsync_body)
        {
            crate::lyrics::romanize::apply_romanization(&mut parsed);
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
            && let Some(mut parsed) = crate::lyrics::parse::parse_subtitle_body(subtitle_body)
        {
            crate::lyrics::romanize::apply_romanization(&mut parsed);
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
    // Build unified single-shot query combining track identifiers and track metadata.
    // Musixmatch resolves matcher.track.get, track.richsync.get, and track.subtitles.get
    // in a single macro call, matching by ID if indexed and falling back seamlessly within
    // the same round-trip.
    let mut params = vec![
        ("q_artist".to_string(), artist.to_string()),
        ("q_artists".to_string(), artist.to_string()),
        ("q_track".to_string(), title.to_string()),
        ("usertoken".to_string(), token.to_string()),
    ];
    if !album.is_empty() {
        params.push(("q_album".to_string(), album.to_string()));
    }
    if let Some(d) = duration {
        let secs = (d.round() as i64).to_string();
        params.push(("q_duration".to_string(), secs.clone()));
        params.push(("f_subtitle_length".to_string(), secs));
        params.push((
            "f_subtitle_length_max_deviation".to_string(),
            "2".to_string(),
        ));
    }

    let has_ids = track_spotify_id.is_some() || track_isrc.is_some() || track_itunes_id.is_some();
    if let Some(sid) = track_spotify_id {
        params.push(("track_spotify_id".to_string(), sid.to_string()));
    }
    if let Some(isrc) = track_isrc {
        params.push(("track_isrc".to_string(), isrc.to_string()));
    }
    if let Some(itunes) = track_itunes_id {
        params.push(("track_itunes_id".to_string(), itunes.to_string()));
    }

    let outcome = try_macro_for_lyrics(client, &params).await?;

    // If an ID was supplied and the query resulted in TrackNotFound,
    // execute a resilient fallback with pure metadata search (omitting IDs)
    // in case the ID is unmapped or stale in Musixmatch's catalog.
    if has_ids && matches!(outcome, FetchOutcome::TrackNotFound) {
        let mut fallback_params = vec![
            ("q_artist".to_string(), artist.to_string()),
            ("q_artists".to_string(), artist.to_string()),
            ("q_track".to_string(), title.to_string()),
            ("usertoken".to_string(), token.to_string()),
        ];
        if !album.is_empty() {
            fallback_params.push(("q_album".to_string(), album.to_string()));
        }
        if let Some(d) = duration {
            let secs = (d.round() as i64).to_string();
            fallback_params.push(("q_duration".to_string(), secs.clone()));
            fallback_params.push(("f_subtitle_length".to_string(), secs));
            fallback_params.push((
                "f_subtitle_length_max_deviation".to_string(),
                "2".to_string(),
            ));
        }

        return try_macro_for_lyrics(client, &fallback_params).await;
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluate_token_outcome_renewable() {
        let json: Value = serde_json::from_str(
            r#"{
            "message": {
                "header": {
                    "status_code": 401,
                    "hint": "renew"
                }
            }
        }"#,
        )
        .unwrap();

        let outcome = evaluate_token_outcome(reqwest::StatusCode::OK, Some(&json));
        assert_eq!(
            outcome,
            Some(FetchOutcome::TokenError {
                reason: "API Status 401 (hint: renew)".to_string(),
                is_renewable: true,
            })
        );
    }

    #[test]
    fn test_evaluate_token_outcome_throttled() {
        let json: Value = serde_json::from_str(
            r#"{
            "message": {
                "header": {
                    "status_code": 429,
                    "hint": "too_many_requests"
                }
            }
        }"#,
        )
        .unwrap();

        let outcome = evaluate_token_outcome(reqwest::StatusCode::OK, Some(&json));
        assert_eq!(
            outcome,
            Some(FetchOutcome::TokenError {
                reason: "API Status 429 (hint: too_many_requests)".to_string(),
                is_renewable: false,
            })
        );
    }

    #[test]
    fn test_evaluate_token_outcome_http_fallback() {
        let outcome = evaluate_token_outcome(reqwest::StatusCode::FORBIDDEN, None);
        assert_eq!(
            outcome,
            Some(FetchOutcome::TokenError {
                reason: "HTTP 403 Forbidden".to_string(),
                is_renewable: false,
            })
        );
    }
}
