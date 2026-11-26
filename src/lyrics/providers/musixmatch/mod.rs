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

use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, NEUTRAL, score_track_candidate,
};
use crate::lyrics::types::{Lyrics, LyricsError, TrackInfo, TrackMatchInfo, http_client};
use extract::MatchedTrackMeta;
use request::{FetchOutcome, fetch_lyrics_with_token};
use token::{TOKEN_MANAGER, fetch_fresh_musixmatch_token as get_fresh_token};

pub(crate) struct MusixmatchSuccess {
    pub lines: Vec<crate::lyrics::types::LyricLine>,
    pub raw: Option<String>,
    pub meta: Option<MatchedTrackMeta>,
}

/// Multi-tier usertoken fetch (`MUSIXMATCH_USERTOKEN`/TOML round-robin + guest `token.get` + retry).
#[allow(clippy::too_many_arguments)]
async fn fetch_musixmatch_with_meta(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    track_spotify_id: Option<&str>,
    track_isrc: Option<&str>,
    track_itunes_id: Option<&str>,
) -> Result<Option<MusixmatchSuccess>, LyricsError> {
    // Check rate-limiting gate (request pacing + 10-minute throttling cooldown)
    if !token::wait_for_gate().await {
        tracing::warn!("Musixmatch is currently under rate-limit cooldown; skipping request");
        return Ok(None);
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
                Ok(FetchOutcome::Success(lines, raw, _ids, meta)) => {
                    return Ok(Some(MusixmatchSuccess { lines, raw, meta }));
                }
                Ok(FetchOutcome::TrackNotFound) => {
                    return Ok(None);
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
        Ok(FetchOutcome::Success(lines, raw, _ids, meta)) => {
            Ok(Some(MusixmatchSuccess { lines, raw, meta }))
        }
        Ok(FetchOutcome::TrackNotFound) => Ok(None),
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
                return Ok(None);
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
                        Ok(FetchOutcome::Success(lines, raw, _ids, meta)) => {
                            Ok(Some(MusixmatchSuccess { lines, raw, meta }))
                        }
                        Ok(FetchOutcome::TokenError { reason, .. }) => {
                            tracing::warn!(
                                reason = %reason,
                                "Freshly issued Musixmatch token was rejected; activating cooldown"
                            );
                            token::trigger_gate_cooldown();
                            Ok(None)
                        }
                        Ok(FetchOutcome::TrackNotFound) => Ok(None),
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

pub(crate) fn score_musixmatch_candidate(
    lyrics: Lyrics,
    meta: Option<&MatchedTrackMeta>,
    track: &TrackInfo,
    custom_regex: Option<&regex::Regex>,
) -> Option<FetchedCandidate> {
    if lyrics.is_empty() {
        return None;
    }
    let Some(m) = meta else {
        return Some(FetchedCandidate {
            lyrics,
            match_score: NEUTRAL,
            is_blind: false,
            video_id_keyed: false,
        });
    };

    let base_score = score_track_candidate(
        &m.title,
        &m.artist,
        m.duration_secs,
        (!m.album.is_empty()).then_some(m.album.as_str()),
        track,
        custom_regex,
    );
    if base_score < 0 {
        return None;
    }

    Some(FetchedCandidate {
        match_score: base_score + lyrics.sync_rank(),
        lyrics,
        is_blind: false,
        video_id_keyed: false,
    })
}

pub struct MusixmatchProvider;

impl LyricsProvider for MusixmatchProvider {
    fn id(&self) -> &'static str {
        "musixmatch"
    }

    fn display_name(&self) -> &'static str {
        "Musixmatch"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        ids: &'a TrackMatchInfo,
        ctx: &'a crate::lyrics::providers::ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let duration = (track.duration_secs > 0).then_some(track.duration_secs as f64);

            if let Some(raw_tokens) = ctx.settings.tokens.musixmatch_usertoken.as_deref()
                && !raw_tokens.trim().is_empty()
                && let Ok(mut mgr) = TOKEN_MANAGER.lock()
            {
                mgr.seed_configured_tokens(raw_tokens);
            }

            let Some(res) = fetch_musixmatch_with_meta(
                &track.artist,
                &track.title,
                &track.album,
                duration,
                ids.track_spotify_ids.first().map(String::as_str),
                ids.track_isrcs.first().map(String::as_str),
                ids.track_itunes_ids.first().map(String::as_str),
            )
            .await?
            else {
                return Ok(Vec::new());
            };

            if res.lines.is_empty() {
                return Ok(Vec::new());
            }

            let synced = res
                .lines
                .iter()
                .any(|l| l.time > 0.0 || l.has_word_timings());
            let has_words = res.lines.iter().any(|l| l.has_word_timings());
            let mut lyrics = Lyrics::new(res.lines, "Musixmatch", synced);
            lyrics.raw_format = res.raw;
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

            Ok(score_musixmatch_candidate(
                lyrics,
                res.meta.as_ref(),
                track,
                ctx.custom_regex.as_ref(),
            )
            .into_iter()
            .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::types::{LyricLine, create_word_timing};

    #[test]
    fn test_score_musixmatch_candidate_uses_metadata_and_rejects_mismatch() {
        let track = TrackInfo::new("Rolling in the Deep", "Adele", "21", 228);
        let words = vec![
            create_word_timing(1.0, 1.5, "There's").with_space(true),
            create_word_timing(1.5, 2.0, "fire"),
        ];
        let lyrics = Lyrics::new(
            vec![LyricLine::new(1.0, "There's fire").with_words(Some(words))],
            "Musixmatch",
            true,
        );

        let good_meta = MatchedTrackMeta {
            title: "Rolling in the Deep".into(),
            artist: "Adele".into(),
            album: "21".into(),
            duration_secs: 228,
        };
        let scored = score_musixmatch_candidate(lyrics.clone(), Some(&good_meta), &track, None)
            .expect("exact match should score positively");
        assert!(scored.match_score > NEUTRAL);
        assert_eq!(scored.match_score, 13); // 11 (exact metadata+album) + 2 (word-sync rank)

        let wrong_meta = MatchedTrackMeta {
            title: "Rolling in the Deep".into(),
            artist: "Random Cover Artist".into(),
            album: "Covers".into(),
            duration_secs: 228,
        };
        assert!(
            score_musixmatch_candidate(lyrics, Some(&wrong_meta), &track, None).is_none(),
            "mismatched artist must be rejected"
        );
    }
}
