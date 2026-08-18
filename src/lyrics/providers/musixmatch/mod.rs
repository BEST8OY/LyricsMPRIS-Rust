//! Musixmatch lyrics provider implementation.
//!
//! Features:
//! - Multi-tier usertoken engine (environment variables + dynamic token.get fallback + transparent error retry)
//! - Richsync word-level timing and Subtitles line-level timing support
//! - Direct ID lookups (Spotify ID, ISRC, iTunes ID) with matcher.track.get fallback

pub mod extract;
pub mod request;
pub mod token;

#[allow(unused_imports)]
pub use token::fetch_fresh_musixmatch_token;

use crate::lyrics::types::{ProviderResult, TrackMatchInfo, http_client};
use request::{FetchOutcome, fetch_lyrics_with_token};
use token::{TOKEN_MANAGER, fetch_fresh_musixmatch_token as get_fresh_token};

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
            tracing::debug!(
                attempt = attempts,
                total = env_token_count,
                token_prefix = %&token[..token.len().min(8)],
                "Musixmatch: using env usertoken (Tier 1)"
            );
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
            Some(t) => {
                tracing::debug!(
                    token_prefix = %&t[..t.len().min(8)],
                    "Musixmatch: using cached guest token (Tier 2)"
                );
                t
            }
            None => match get_fresh_token(client).await {
                Ok(fresh) => {
                    tracing::debug!(
                        token_prefix = %&fresh[..fresh.len().min(8)],
                        "Musixmatch: fetched fresh guest token (Tier 2)"
                    );
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
            match get_fresh_token(client).await {
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
