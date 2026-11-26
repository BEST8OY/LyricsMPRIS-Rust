//! Musixmatch lyrics provider implementation.
//!
//! Features:
//! - Multi-tier usertoken engine (environment variables + dynamic token.get fallback + transparent error retry)
//! - Disk-backed persistent dynamic token caching with automatic UpgradeOnly token rejection
//! - Request pacing (1.5s) and 10-minute throttling cooldown gate to prevent IP bans
//! - Header hint inspection differentiating between token expiration (`hint: renew`) and network throttling
//! - Direct ID lookups (Spotify ID, ISRC, iTunes ID) and single-macro fuzzy track search
//! - Richsync word-level timing and Subtitles line-level timing support

pub mod extract;
pub mod request;
pub mod token;

#[allow(unused_imports)]
pub use token::fetch_fresh_musixmatch_token;

use crate::lyrics::types::{LyricsError, ProviderResult, TrackMatchInfo, http_client};
use request::{FetchOutcome, fetch_lyrics_with_token};
use token::{TOKEN_MANAGER, fetch_fresh_musixmatch_token as get_fresh_token};

/// Fetch lyrics using Musixmatch mobile usertoken (apic-appmobile.musixmatch.com).
///
/// Multi-tier token strategy:
/// 1. Tries user-configured tokens from `MUSIXMATCH_USERTOKEN` env var in round-robin fashion.
/// 2. If no env tokens are configured or all return token errors, automatically fetches
///    an anonymous user token via `token.get` (or reads from disk cache).
/// 3. If a renewable token error (`401` with `hint: renew`) occurs on the dynamic token,
///    automatically requests a fresh token and retries once transparently.
/// 4. If throttled (`401` without renew hint, `402`, `403`, `429`), triggers a 10-minute
///    cooldown period to prevent extended IP bans.
pub async fn fetch_lyrics_from_musixmatch_usertoken(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    track_spotify_id: Option<&str>,
    track_isrc: Option<&str>,
    track_itunes_id: Option<&str>,
) -> ProviderResult {
    // Check rate-limiting gate (request pacing + 10-minute throttling cooldown)
    if !token::wait_for_gate().await {
        tracing::warn!("Musixmatch is currently under rate-limit cooldown; skipping request");
        return Ok((Vec::new(), None, TrackMatchInfo::default()));
    }

    let client = http_client();
    let env_token_count = {
        let manager = TOKEN_MANAGER.lock().ok();
        manager.map(|m| m.env_token_count()).unwrap_or(0)
    };

    // 1. Try env tokens in round-robin fashion if configured
    // Track network-level failures separately: if every attempt failed to reach
    // the server at all, there is no point trying token.get - propagate the error
    // so callers can classify it as transient and schedule a retry.
    let mut attempts = 0usize;
    let mut non_network_outcomes = 0usize;
    let mut last_network_err: Option<LyricsError> = None;

    if env_token_count > 0 {
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
                Ok(FetchOutcome::TokenError {
                    reason,
                    is_renewable,
                }) => {
                    non_network_outcomes += 1;
                    tracing::warn!(
                        attempt = attempts,
                        total = env_token_count,
                        reason = %reason,
                        is_renewable = is_renewable,
                        "Musixmatch env token error, attempting fallback to next token"
                    );
                }
                Err(e) => {
                    if matches!(e, LyricsError::Network(_)) {
                        tracing::warn!(
                            attempt = attempts,
                            total = env_token_count,
                            error = %e,
                            "Musixmatch request failed with env token"
                        );
                        last_network_err = Some(e);
                    } else {
                        non_network_outcomes += 1;
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
    }

    if attempts > 0
        && non_network_outcomes == 0
        && let Some(e) = last_network_err
    {
        tracing::warn!(error = %e, "Musixmatch unreachable with all env tokens");
        return Err(e);
    }

    // 2. Fallback to dynamic token (from memory/disk cache or via token.get API)
    let dynamic_token = {
        let cached = TOKEN_MANAGER
            .lock()
            .ok()
            .and_then(|mut m| m.get_dynamic_token());
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
                    return Err(e);
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
        Ok(FetchOutcome::TokenError {
            reason,
            is_renewable,
        }) => {
            if !is_renewable {
                tracing::warn!(
                    reason = %reason,
                    "Musixmatch network/token is throttled; activating 10-minute cooldown"
                );
                token::trigger_gate_cooldown();
                return Ok((Vec::new(), None, TrackMatchInfo::default()));
            }

            tracing::warn!(
                reason = %reason,
                "Dynamic Musixmatch token expired (hint: renew), requesting fresh token and retrying"
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
                        Ok(FetchOutcome::TokenError { reason, .. }) => {
                            tracing::warn!(
                                reason = %reason,
                                "Freshly issued Musixmatch token was rejected; activating cooldown"
                            );
                            token::trigger_gate_cooldown();
                            Ok((Vec::new(), None, TrackMatchInfo::default()))
                        }
                        Ok(FetchOutcome::TrackNotFound) => {
                            Ok((Vec::new(), None, TrackMatchInfo::default()))
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to renew dynamic Musixmatch token");
                    Err(e)
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Musixmatch request failed with dynamic token");
            Err(e)
        }
    }
}

pub struct MusixmatchProvider;

impl crate::lyrics::providers::LyricsProvider for MusixmatchProvider {
    fn id(&self) -> &'static str {
        "musixmatch"
    }

    fn display_name(&self) -> &'static str {
        "Musixmatch"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a crate::lyrics::types::TrackInfo,
        ids: &'a TrackMatchInfo,
        ctx: &'a crate::lyrics::providers::ProviderContext,
    ) -> crate::lyrics::providers::BoxFuture<
        'a,
        Result<Vec<crate::lyrics::providers::FetchedCandidate>, LyricsError>,
    > {
        Box::pin(async move {
            let duration = (track.duration_secs > 0).then_some(track.duration_secs as f64);

            if let Some(raw_tokens) = ctx.settings.tokens.musixmatch_usertoken.as_deref()
                && !raw_tokens.trim().is_empty()
                && let Ok(mut mgr) = TOKEN_MANAGER.lock()
            {
                mgr.seed_configured_tokens(raw_tokens);
            }

            let (lines, raw, _) = fetch_lyrics_from_musixmatch_usertoken(
                &track.artist,
                &track.title,
                &track.album,
                duration,
                ids.track_spotify_ids.first().map(String::as_str),
                ids.track_isrcs.first().map(String::as_str),
                ids.track_itunes_ids.first().map(String::as_str),
            )
            .await?;

            if lines.is_empty() {
                return Ok(Vec::new());
            }

            let synced = lines.iter().any(|l| l.time > 0.0 || l.has_word_timings());
            let has_words = lines.iter().any(|l| l.has_word_timings());
            let mut lyrics = crate::lyrics::types::Lyrics::new(lines, "Musixmatch", synced);
            lyrics.raw_format = raw;
            lyrics.format_type = Some(
                if has_words {
                    "richsync"
                } else if synced {
                    "subtitles"
                } else {
                    "txt"
                }
                .to_string(),
            );
            Ok(vec![crate::lyrics::providers::FetchedCandidate {
                lyrics,
                match_score: crate::lyrics::providers::NEUTRAL,
                is_blind: false,
                video_id_keyed: false,
            }])
        })
    }
}
