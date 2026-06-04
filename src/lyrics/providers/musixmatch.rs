use serde_json::Value;
use std::env;
use std::sync::Mutex;
use reqwest::Client;
use once_cell::sync::Lazy;

use crate::lyrics::types::{http_client, LyricLine, LyricsError, ProviderResult};

/// Round-robin token manager for multiple Musixmatch usertokens
struct TokenManager {
    tokens: Vec<String>,
    current_index: usize,
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
        }
    }

    /// Get the next token in round-robin fashion.
    /// Returns None if no tokens are configured.
    fn next_token(&mut self) -> Option<String> {
        if self.tokens.is_empty() {
            return None;
        }
        
        let token = self.tokens[self.current_index].clone();
        self.current_index = (self.current_index + 1) % self.tokens.len();
        Some(token)
    }
}

static TOKEN_MANAGER: Lazy<Mutex<TokenManager>> = Lazy::new(|| {
    Mutex::new(TokenManager::new())
});

/// Get the next token from the round-robin manager
fn get_next_token() -> Option<String> {
    TOKEN_MANAGER.lock().ok()?.next_token()
}

/// Outcome of a single token's fetch attempt.
#[derive(Debug)]
enum FetchOutcome {
    /// Successfully found lyrics: (parsed lines, raw response content)
    Success(Vec<LyricLine>, Option<String>),
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

/// Try to call macro.subtitles.get and extract richsync or subtitle_body.
async fn try_macro_for_lyrics(
    client: &Client,
    params: &[(String, String)],
) -> Result<FetchOutcome, reqwest::Error> {
    let macro_base = "https://apic-desktop.musixmatch.com/ws/1.1/macro.subtitles.get?format=json&namespace=lyrics_richsynched&subtitle_format=mxm&optional_calls=track.richsync&app_id=web-desktop-app-v1.0&";
    let macro_url = macro_base.to_string()
        + &params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

    let macro_resp = client
        .get(&macro_url)
        .header("Cookie", "x-mxm-token-guid=")
        .send()
        .await?;

    if is_token_error_status(macro_resp.status()) {
        return Ok(FetchOutcome::TokenError(format!("HTTP {}", macro_resp.status())));
    }

    if !macro_resp.status().is_success() {
        return Ok(FetchOutcome::TrackNotFound);
    }

    let macro_json: Value = macro_resp.json().await?;
    if let Some(code) = get_root_status_code(&macro_json) {
        if is_token_error_code(code) {
            return Ok(FetchOutcome::TokenError(format!("API Status {}", code)));
        }
    }

    let macro_calls = macro_json.pointer("/message/body/macro_calls");
    
    if let Some(calls) = macro_calls {
        // Prefer richsync (word-level timing) if available
        if is_success(calls, "track.richsync.get") {
            if let Some(richsync_body) = calls
                .pointer("/track.richsync.get/message/body/richsync/richsync_body")
                .and_then(|v| v.as_str())
            {
                if let Some(parsed) = crate::lyrics::parse::parse_richsync_body(richsync_body) {
                    return Ok(FetchOutcome::Success(parsed, Some(richsync_body.to_string())));
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
                    return Ok(FetchOutcome::Success(parsed, Some(subtitle_body.to_string())));
                }
            }
        }
    }

    Ok(FetchOutcome::TrackNotFound)
}

/// Fetch lyrics with a single token.
async fn fetch_lyrics_with_token(
    client: &Client,
    token: &str,
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    track_spotify_id: Option<&str>,
) -> Result<FetchOutcome, LyricsError> {
    // Strategy 1: If we have a Spotify track ID, try direct lookup first
    if let Some(sid) = track_spotify_id {
        let mut params = vec![
            ("track_spotify_id".to_string(), sid.to_string()),
            ("usertoken".to_string(), token.to_string()),
        ];
        if let Some(len) = duration.map(|d| d.round() as i64) {
            params.push(("q_duration".to_string(), len.to_string()));
        }
        
        match try_macro_for_lyrics(client, &params).await? {
            FetchOutcome::Success(parsed, raw) => return Ok(FetchOutcome::Success(parsed, raw)),
            FetchOutcome::TokenError(reason) => return Ok(FetchOutcome::TokenError(reason)),
            FetchOutcome::TrackNotFound => {} // fall through to search
        }
    }

    // Strategy 2: Search by track metadata and use similarity matching
    let search_base = "https://apic-desktop.musixmatch.com/ws/1.1/track.search?format=json&app_id=web-desktop-app-v1.0&";
    let mut search_params = vec![
        format!("q_artist={}", urlencoding::encode(artist)),
        format!("q_track={}", urlencoding::encode(title)),
        format!("usertoken={}", urlencoding::encode(token)),
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
        .header("Cookie", "x-mxm-token-guid=")
        .send()
        .await?;

    if is_token_error_status(search_resp.status()) {
        return Ok(FetchOutcome::TokenError(format!("Search HTTP {}", search_resp.status())));
    }

    if !search_resp.status().is_success() {
        return Ok(FetchOutcome::TrackNotFound);
    }

    let search_json: Value = search_resp.json().await?;
    if let Some(code) = get_root_status_code(&search_json) {
        if is_token_error_code(code) {
            return Ok(FetchOutcome::TokenError(format!("Search API Status {}", code)));
        }
    }

    let track_list = search_json
        .pointer("/message/body/track_list")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if track_list.is_empty() {
        return Ok(FetchOutcome::TrackNotFound);
    }

    // Extract track objects from the track_list wrapper
    let candidates: Vec<Value> = track_list
        .iter()
        .filter_map(|item| item.get("track").cloned())
        .collect();

    if candidates.is_empty() {
        return Ok(FetchOutcome::TrackNotFound);
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
                return Ok(FetchOutcome::Success(vec![line], None));
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
                    ("usertoken".to_string(), token.to_string()),
                ];
                
                if let Some(len) = track_length {
                    params.push(("q_duration".to_string(), len.to_string()));
                }

                match try_macro_for_lyrics(client, &params).await? {
                    FetchOutcome::Success(parsed, raw) => return Ok(FetchOutcome::Success(parsed, raw)),
                    FetchOutcome::TokenError(reason) => return Ok(FetchOutcome::TokenError(reason)),
                    FetchOutcome::TrackNotFound => {}
                }
            }
        }
    }

    Ok(FetchOutcome::TrackNotFound)
}

/// Fetch lyrics using Musixmatch desktop "usertoken" (apic-desktop.musixmatch.com).
/// 
/// Supports multiple usertokens (comma-separated in MUSIXMATCH_USERTOKEN env var)
/// and uses them in a round-robin fashion.
pub async fn fetch_lyrics_from_musixmatch_usertoken(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    track_spotify_id: Option<&str>,
) -> ProviderResult {
    let total_tokens = {
        let manager = TOKEN_MANAGER.lock().ok();
        manager.map(|m| m.tokens.len()).unwrap_or(0)
    };

    if total_tokens == 0 {
        return Ok((Vec::new(), None));
    }

    let client = http_client();
    let mut attempts = 0;

    while attempts < total_tokens {
        let token = match get_next_token() {
            Some(t) => t,
            None => break,
        };

        attempts += 1;

        match fetch_lyrics_with_token(
            client,
            &token,
            artist,
            title,
            album,
            duration,
            track_spotify_id,
        )
        .await
        {
            Ok(FetchOutcome::Success(parsed, raw)) => {
                return Ok((parsed, raw));
            }
            Ok(FetchOutcome::TrackNotFound) => {
                // The track genuinely doesn't exist or doesn't have lyrics, no need to retry on other tokens
                return Ok((Vec::new(), None));
            }
            Ok(FetchOutcome::TokenError(reason)) => {
                tracing::warn!(
                    attempt = attempts,
                    total = total_tokens,
                    reason = %reason,
                    "Musixmatch token error, attempting fallback to next token in round-robin"
                );
                continue;
            }
            Err(e) => {
                // If it's a network error, treat it as a transient error and fallback
                tracing::warn!(
                    attempt = attempts,
                    total = total_tokens,
                    error = %e,
                    "Musixmatch request failed, attempting fallback to next token in round-robin"
                );
                continue;
            }
        }
    }

    Ok((Vec::new(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_manager_round_robin() {
        let mut tm = TokenManager {
            tokens: vec!["token1".to_string(), "token2".to_string(), "token3".to_string()],
            current_index: 0,
        };

        assert_eq!(tm.next_token(), Some("token1".to_string()));
        assert_eq!(tm.next_token(), Some("token2".to_string()));
        assert_eq!(tm.next_token(), Some("token3".to_string()));
        assert_eq!(tm.next_token(), Some("token1".to_string())); // Wraps around
    }

    #[test]
    fn test_token_manager_empty() {
        let mut tm = TokenManager {
            tokens: Vec::new(),
            current_index: 0,
        };

        assert_eq!(tm.next_token(), None);
    }
}
