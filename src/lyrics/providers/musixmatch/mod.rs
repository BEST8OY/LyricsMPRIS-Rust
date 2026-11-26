//! Musixmatch lyrics provider implementation.
//!
//! Features:
//! - Multi-tier usertoken engine (environment variables + dynamic token.get fallback + transparent error retry)
//! - Disk-backed persistent dynamic token caching with automatic UpgradeOnly token rejection
//! - Request pacing matching Morphe (`500ms` per API call, no 10-minute ban)
//! - Header hint inspection differentiating between token expiration (`hint: renew`) and throttling
//! - Direct ID lookups (Spotify ID, ISRC) and single-macro fuzzy track search
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
async fn fetch_musixmatch_with_meta(
    track: &TrackInfo,
    ids: &TrackMatchInfo,
    custom_regex: Option<&regex::Regex>,
) -> Result<Option<MusixmatchSuccess>, LyricsError> {
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
            match fetch_lyrics_with_token(client, &token, track, ids, custom_regex).await {
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
                    if is_renewable && let Ok(mut manager) = TOKEN_MANAGER.lock() {
                        manager.invalidate_env_token(&token);
                    }
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

    match fetch_lyrics_with_token(client, &dynamic_token, track, ids, custom_regex).await {
        Ok(FetchOutcome::Success(lines, raw, _ids, meta)) => {
            Ok(Some(MusixmatchSuccess { lines, raw, meta }))
        }
        Ok(FetchOutcome::TrackNotFound) => Ok(None),
        Ok(FetchOutcome::TokenError {
            reason,
            is_renewable,
        }) => {
            token::clear_cached_dynamic_token();
            if !is_renewable {
                tracing::warn!(
                    reason = %reason,
                    "Musixmatch dynamic token rejected without renew hint; skipping request"
                );
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
                    match fetch_lyrics_with_token(client, &fresh, track, ids, custom_regex).await {
                        Ok(FetchOutcome::Success(lines, raw, _ids, meta)) => {
                            Ok(Some(MusixmatchSuccess { lines, raw, meta }))
                        }
                        Ok(FetchOutcome::TokenError { reason, .. }) => {
                            tracing::warn!(
                                reason = %reason,
                                "Freshly issued Musixmatch token was rejected"
                            );
                            token::clear_cached_dynamic_token();
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

pub(crate) fn metadata_match_score(
    meta: Option<&MatchedTrackMeta>,
    track: &TrackInfo,
    custom_regex: Option<&regex::Regex>,
) -> Option<i32> {
    let Some(m) = meta else {
        return Some(NEUTRAL);
    };
    let base_score = score_track_candidate(
        &m.title,
        &m.artist,
        m.duration_secs,
        (!m.album.is_empty()).then_some(m.album.as_str()),
        track,
        custom_regex,
    );
    (base_score >= 0).then_some(base_score)
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
    let base_score = metadata_match_score(meta, track, custom_regex)?;
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
            if let Some(raw_tokens) = ctx.settings.tokens.musixmatch_usertoken.as_deref()
                && !raw_tokens.trim().is_empty()
                && let Ok(mut mgr) = TOKEN_MANAGER.lock()
            {
                mgr.seed_configured_tokens(raw_tokens);
            }

            let Some(res) =
                fetch_musixmatch_with_meta(track, ids, ctx.custom_regex.as_ref()).await?
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
