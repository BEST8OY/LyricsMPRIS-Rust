use once_cell::sync::Lazy;
use reqwest::Client;
use serde_json::Value;
use std::env;
use std::sync::Mutex;

use crate::lyrics::types::{LyricLine, LyricsError, ProviderResult, TrackMatchInfo, http_client};

const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Electron/1.7.6 Safari/537.36";

/// Requests a fresh anonymous Musixmatch user token directly from the API (`token.get`).
pub async fn fetch_fresh_musixmatch_token(client: &Client) -> Result<String, LyricsError> {
    let url = "https://apic-desktop.musixmatch.com/ws/1.1/token.get?app_id=web-desktop-app-v1.0";
    let resp = client
        .get(url)
        .header("User-Agent", DEFAULT_USER_AGENT)
        .header("Cookie", "x-mxm-token-guid=")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(LyricsError::Api(format!(
            "Failed to get Musixmatch token: HTTP {}",
            resp.status()
        )));
    }

    let json: Value = resp.json().await?;
    let code = get_root_status_code(&json).unwrap_or(0);
    if code != 200 {
        return Err(LyricsError::Api(format!(
            "Musixmatch token.get status {}",
            code
        )));
    }

    let token = json
        .pointer("/message/body/user_token")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    match token {
        Some(t) => {
            tracing::debug!(token_prefix = %&t[..t.len().min(8)], "Obtained fresh Musixmatch user token");
            Ok(t)
        }
        None => Err(LyricsError::Api(
            "Missing user_token in Musixmatch token.get response".to_string(),
        )),
    }
}

/// Round-robin token manager for multiple Musixmatch usertokens,
/// with automatic dynamic fallback via token.get API.
struct TokenManager {
    tokens: Vec<String>,
    current_index: usize,
    cached_dynamic_token: Option<String>,
}

impl TokenManager {
    /// Initialize the token manager from the MUSIXMATCH_USERTOKEN environment variable.
    /// Supports multiple tokens separated by commas.
    fn new() -> Self {
        let tokens = env::var("MUSIXMATCH_USERTOKEN")
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        TokenManager {
            tokens,
            current_index: 0,
            cached_dynamic_token: None,
        }
    }

    /// Returns available configured env tokens count.
    fn env_token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Get the next env token in round-robin fashion if available.
    fn next_env_token(&mut self) -> Option<String> {
        if self.tokens.is_empty() {
            return None;
        }

        let token = self.tokens[self.current_index].clone();
        self.current_index = (self.current_index + 1) % self.tokens.len();
        Some(token)
    }

    /// Get cached dynamic token if present.
    fn get_dynamic_token(&self) -> Option<String> {
        self.cached_dynamic_token.clone()
    }

    /// Update cached dynamic token.
    fn set_dynamic_token(&mut self, token: String) {
        self.cached_dynamic_token = Some(token);
    }
}

static TOKEN_MANAGER: Lazy<Mutex<TokenManager>> = Lazy::new(|| Mutex::new(TokenManager::new()));

/// Outcome of a single token's fetch attempt.
#[derive(Debug)]
enum FetchOutcome {
    /// Successfully found lyrics: (parsed lines, raw response content, track identifiers)
    Success(Vec<LyricLine>, Option<String>, TrackMatchInfo),
    /// Track was not found on Musixmatch (200 status but empty list/no lyrics).
    TrackNotFound,
    /// A token-specific issue (401 unauthorized, 402 payment, 403 forbidden, 429 rate limit).
    TokenError(String),
}

/// Extracts the root level status code from a Musixmatch response JSON.
fn get_root_status_code(json: &Value) -> Option<i64> {
    json.pointer("/message/header/status_code")
        .and_then(|v| v.as_i64())
}

fn extract_string_or_int(value: &Value) -> Option<String> {
    let s = value
        .as_str()
        .map(String::from)
        .or_else(|| value.as_i64().map(|i| i.to_string()))?;
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn extract_first_string_from_array(value: &Value) -> Option<String> {
    value
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(extract_string_or_int)
}

fn extract_first_string_from_nested_array(value: &Value) -> Option<String> {
    value
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_array())
        .and_then(|inner| inner.first())
        .and_then(extract_string_or_int)
}

/// Returns true if the status code indicates a token/auth/quota/rate-limit error.
fn is_token_error_code(code: i64) -> bool {
    matches!(code, 401 | 402 | 403 | 429)
}

/// Returns true if the HTTP response status indicates a token/auth/quota/rate-limit error.
fn is_token_error_status(status: reqwest::StatusCode) -> bool {
    let code = status.as_u16();
    matches!(code, 401 | 402 | 403 | 429)
}

/// Check if a macro response has a successful status code (200).
fn is_success(macro_calls: &Value, endpoint: &str) -> bool {
    macro_calls
        .get(endpoint)
        .and_then(|v| v.pointer("/message/header/status_code"))
        .and_then(|v| v.as_i64())
        .map(|code| code == 200)
        .unwrap_or(false)
}

/// Extract track identifiers (isrc, spotify_id, itunes_id) from a musixmatch track object.
/// Handles both flat string fields and array fields like `commontrack_itunes_ids`.
fn extract_track_ids_from_json(track: &Value) -> TrackMatchInfo {
    let itunes_id = track
        .get("track_itunes_id")
        .and_then(extract_string_or_int)
        .or_else(|| {
            track
                .get("commontrack_itunes_ids")
                .and_then(extract_first_string_from_array)
        });

    let isrc = track
        .get("track_isrc")
        .and_then(extract_string_or_int)
        .or_else(|| {
            track.get("commontrack_isrcs").and_then(|v| {
                extract_first_string_from_array(v)
                    .or_else(|| extract_first_string_from_nested_array(v))
            })
        });

    let spotify_id = track
        .get("track_spotify_id")
        .and_then(extract_string_or_int)
        .or_else(|| {
            track
                .get("commontrack_spotify_ids")
                .and_then(extract_first_string_from_array)
        });

    TrackMatchInfo {
        track_isrc: isrc,
        track_spotify_id: spotify_id,
        track_itunes_id: itunes_id,
    }
}

/// Try to extract track identifiers from the macro response JSON.
/// Musixmatch macro responses may include `matcher.track.get` with full track metadata.
fn extract_track_ids_from_macro(macro_calls: &Value) -> TrackMatchInfo {
    if let Some(track) = macro_calls.pointer("/matcher.track.get/message/body/track") {
        return extract_track_ids_from_json(track);
    }
    if let Some(track) = macro_calls.pointer("/track") {
        return extract_track_ids_from_json(track);
    }
    TrackMatchInfo::default()
}

/// Try to call macro.subtitles.get and extract richsync or subtitle_body.
async fn try_macro_for_lyrics(
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
async fn fetch_lyrics_with_token(
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

/// Fetch lyrics using Musixmatch desktop usertoken (apic-desktop.musixmatch.com).
///
/// Multi-tier token strategy:
/// 1. Tries user-configured tokens from `MUSIXMATCH_USERTOKEN` env var in round-robin fashion.
/// 2. If no env tokens are configured or all return token errors, automatically fetches
///    an anonymous user token via `token.get`.
/// 3. If a token error (401/402/403/429) occurs on the dynamic token, automatically
///    requests a fresh token and retries once transparently.
pub async fn fetch_lyrics_from_musixmatch_usertoken(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    track_spotify_id: Option<&str>,
    track_isrc: Option<&str>,
    track_itunes_id: Option<&str>,
) -> ProviderResult {
    let client = http_client();
    let env_token_count = {
        let manager = TOKEN_MANAGER.lock().ok();
        manager.map(|m| m.env_token_count()).unwrap_or(0)
    };

    // 1. Try env tokens in round-robin fashion if configured
    if env_token_count > 0 {
        let mut attempts = 0;
        while attempts < env_token_count {
            let token = {
                let mut manager = TOKEN_MANAGER.lock().ok();
                manager.as_mut().and_then(|m| m.next_env_token())
            };
            let Some(token) = token else { break };

            attempts += 1;
            match fetch_lyrics_with_token(
                client,
                &token,
                artist,
                title,
                album,
                duration,
                track_spotify_id,
                track_isrc,
                track_itunes_id,
            )
            .await
            {
                Ok(FetchOutcome::Success(parsed, raw, ids)) => {
                    return Ok((parsed, raw, ids));
                }
                Ok(FetchOutcome::TrackNotFound) => {
                    return Ok((Vec::new(), None, TrackMatchInfo::default()));
                }
                Ok(FetchOutcome::TokenError(reason)) => {
                    tracing::warn!(
                        attempt = attempts,
                        total = env_token_count,
                        reason = %reason,
                        "Musixmatch env token error, attempting fallback to next token"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        attempt = attempts,
                        total = env_token_count,
                        error = %e,
                        "Musixmatch request failed with env token, trying next token"
                    );
                }
            }
        }
    }

    // 2. Fallback to dynamic token (via token.get API)
    let dynamic_token = {
        let cached = TOKEN_MANAGER
            .lock()
            .ok()
            .and_then(|m| m.get_dynamic_token());
        match cached {
            Some(t) => t,
            None => match fetch_fresh_musixmatch_token(client).await {
                Ok(fresh) => {
                    if let Ok(mut m) = TOKEN_MANAGER.lock() {
                        m.set_dynamic_token(fresh.clone());
                    }
                    fresh
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to auto-fetch dynamic Musixmatch token");
                    return Ok((Vec::new(), None, TrackMatchInfo::default()));
                }
            },
        }
    };

    match fetch_lyrics_with_token(
        client,
        &dynamic_token,
        artist,
        title,
        album,
        duration,
        track_spotify_id,
        track_isrc,
        track_itunes_id,
    )
    .await
    {
        Ok(FetchOutcome::Success(parsed, raw, ids)) => Ok((parsed, raw, ids)),
        Ok(FetchOutcome::TrackNotFound) => Ok((Vec::new(), None, TrackMatchInfo::default())),
        Ok(FetchOutcome::TokenError(reason)) => {
            tracing::warn!(
                reason = %reason,
                "Dynamic Musixmatch token error, requesting fresh token and retrying"
            );
            // 3. Renew dynamic token and retry once
            match fetch_fresh_musixmatch_token(client).await {
                Ok(fresh) => {
                    if let Ok(mut m) = TOKEN_MANAGER.lock() {
                        m.set_dynamic_token(fresh.clone());
                    }
                    match fetch_lyrics_with_token(
                        client,
                        &fresh,
                        artist,
                        title,
                        album,
                        duration,
                        track_spotify_id,
                        track_isrc,
                        track_itunes_id,
                    )
                    .await
                    {
                        Ok(FetchOutcome::Success(parsed, raw, ids)) => Ok((parsed, raw, ids)),
                        _ => Ok((Vec::new(), None, TrackMatchInfo::default())),
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to renew dynamic Musixmatch token");
                    Ok((Vec::new(), None, TrackMatchInfo::default()))
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Musixmatch request failed with dynamic token");
            Ok((Vec::new(), None, TrackMatchInfo::default()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_manager_round_robin() {
        let mut tm = TokenManager {
            tokens: vec![
                "token1".to_string(),
                "token2".to_string(),
                "token3".to_string(),
            ],
            current_index: 0,
            cached_dynamic_token: None,
        };

        assert_eq!(tm.next_env_token(), Some("token1".to_string()));
        assert_eq!(tm.next_env_token(), Some("token2".to_string()));
        assert_eq!(tm.next_env_token(), Some("token3".to_string()));
        assert_eq!(tm.next_env_token(), Some("token1".to_string())); // Wraps around
    }

    #[test]
    fn test_token_manager_dynamic_caching() {
        let mut tm = TokenManager {
            tokens: Vec::new(),
            current_index: 0,
            cached_dynamic_token: None,
        };

        assert_eq!(tm.next_env_token(), None);
        assert_eq!(tm.get_dynamic_token(), None);

        tm.set_dynamic_token("fresh_dynamic_token_123".to_string());
        assert_eq!(
            tm.get_dynamic_token(),
            Some("fresh_dynamic_token_123".to_string())
        );
    }

    #[test]
    fn test_extract_track_ids_from_json() {
        let json: Value = serde_json::from_str(
            r#"{
            "track_isrc": "USUM72345678",
            "track_spotify_id": "6rqhFg4Kj6gIwR5v",
            "track_itunes_id": "123456789"
        }"#,
        )
        .unwrap();

        let ids = extract_track_ids_from_json(&json);
        assert_eq!(ids.track_isrc, Some("USUM72345678".to_string()));
        assert_eq!(ids.track_spotify_id, Some("6rqhFg4Kj6gIwR5v".to_string()));
        assert_eq!(ids.track_itunes_id, Some("123456789".to_string()));
    }

    #[test]
    fn test_extract_track_ids_from_json_array_fields() {
        let json: Value = serde_json::from_str(
            r#"{
            "commontrack_isrcs": [["GBCEL1300362"]],
            "commontrack_spotify_ids": ["5FVd6KXrgO9B3JPmC8OPst"],
            "commontrack_itunes_ids": [1442699400]
        }"#,
        )
        .unwrap();

        let ids = extract_track_ids_from_json(&json);
        assert_eq!(ids.track_isrc, Some("GBCEL1300362".to_string()));
        assert_eq!(
            ids.track_spotify_id,
            Some("5FVd6KXrgO9B3JPmC8OPst".to_string())
        );
        assert_eq!(ids.track_itunes_id, Some("1442699400".to_string()));
    }

    #[test]
    fn test_extract_track_ids_missing() {
        let json: Value = serde_json::from_str(
            r#"{
            "track_name": "Test Song"
        }"#,
        )
        .unwrap();

        let ids = extract_track_ids_from_json(&json);
        assert_eq!(ids.track_isrc, None);
        assert_eq!(ids.track_spotify_id, None);
        assert_eq!(ids.track_itunes_id, None);
    }
}
