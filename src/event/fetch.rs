//! Multi-provider lyrics fetching and caching pipeline.

use crate::lyrics::types::{LyricsError, ProviderResult, is_retryable_status};
use crate::mpris::TrackMetadata;
use crate::state::{Provider, StateBundle};

/// Message surfaced to the UI while providers are unreachable.
pub(crate) const NETWORK_RETRY_MESSAGE: &str = "Network unavailable - retrying...";

/// Result of a lyrics fetch attempt from a single provider.
///
/// This enum classifies failures as transient (try next provider / schedule retry)
/// or non-transient (stop trying and report error).
pub(crate) enum FetchResult {
    /// Lyrics fetched successfully
    Success,
    /// Provider answered authoritatively: no lyrics exist for this track
    NotFound,
    /// Transient failure (network issue, rate limit, server outage) - try next
    /// provider and consider a delayed retry
    Transient,
    /// Non-transient error (API error, parse error) - stop trying and report
    NonTransient(LyricsError),
}

/// Outcome of a full multi-provider fetch pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchSummary {
    /// Lyrics are loaded (from cache or a provider)
    Loaded,
    /// Definitive miss or fatal error - retrying will not help
    Exhausted,
    /// At least one provider was unreachable - worth retrying after a delay
    Unavailable,
}

/// Returns true if the error is considered transient (temporary network issue, rate limit, server outage).
fn is_transient(err: &LyricsError) -> bool {
    match err {
        LyricsError::Network(_) => true,
        LyricsError::Http(code) if is_retryable_status(*code) => true,
        _ => false,
    }
}

/// Classifies a provider error into retryable vs fatal.
fn classify_error(err: LyricsError) -> FetchResult {
    if is_transient(&err) {
        FetchResult::Transient
    } else {
        FetchResult::NonTransient(err)
    }
}

/// Determines the final fetch summary when neither concurrent provider returned lyrics.
///
/// If at least one provider was reached cleanly and confirmed empty lyrics (i.e. `Ok`),
/// network connectivity is operational (the track simply doesn't exist on those providers).
/// Only if BOTH providers failed with transient network/retryable errors is the network
/// classified as `Unavailable` to trigger background retry.
fn resolve_failed_concurrent_fetch(
    res_a: &ProviderResult,
    res_b: &ProviderResult,
    meta: &TrackMetadata,
    state: &mut StateBundle,
) -> FetchSummary {
    if res_a.is_ok() || res_b.is_ok() {
        state.update_lyrics(Vec::new(), meta, None, None);
        return FetchSummary::Exhausted;
    }

    let a_transient = res_a.as_ref().err().is_some_and(is_transient);
    let b_transient = res_b.as_ref().err().is_some_and(is_transient);

    if a_transient && b_transient {
        state.update_lyrics(
            Vec::new(),
            meta,
            Some(NETWORK_RETRY_MESSAGE.to_string()),
            None,
        );
        FetchSummary::Unavailable
    } else {
        state.update_lyrics(Vec::new(), meta, None, None);
        FetchSummary::Exhausted
    }
}

/// Attempts to fetch lyrics from a single provider by name.
pub(crate) async fn try_provider(
    provider: &str,
    meta: &TrackMetadata,
    state: &mut StateBundle,
) -> FetchResult {
    match provider {
        "lrclib" => try_lrclib(meta, state).await,
        "musixmatch" => try_musixmatch(meta, state).await,
        _ => FetchResult::NotFound,
    }
}

/// Stores fetched lyrics in the database cache.
pub(crate) async fn store_lyrics_in_cache(
    meta: &TrackMetadata,
    raw: Option<String>,
    format: crate::lyrics::database::LyricsFormat,
    provider_isrcs: &[String],
    provider_spotify_ids: &[String],
    provider_itunes_ids: &[String],
) {
    if let Some(raw_text) = raw {
        // Note: Player/file reported identifiers (meta.isrc, meta.spotify_id, meta.itunes_id)
        // are intentionally NOT cached here. We only cache authoritative identifiers
        // reported directly by Musixmatch to prevent caching unverified or spoofed player metadata.
        crate::lyrics::database::store_in_database(
            &meta.artist,
            &meta.title,
            &meta.album,
            meta.length,
            format,
            raw_text,
            provider_isrcs,
            provider_spotify_ids,
            provider_itunes_ids,
        )
        .await;
    }
}

/// Applies and caches lyrics fetched from LRCLIB.
pub(crate) async fn apply_and_store_lrclib(
    lines: Vec<crate::lyrics::LyricLine>,
    raw: Option<String>,
    ids: &crate::lyrics::types::TrackMatchInfo,
    meta: &TrackMetadata,
    state: &mut StateBundle,
) {
    state.update_lyrics(lines, meta, None, Some(Provider::Lrclib));
    store_lyrics_in_cache(
        meta,
        raw,
        crate::lyrics::database::LyricsFormat::Lrclib,
        &ids.track_isrcs,
        &ids.track_spotify_ids,
        &ids.track_itunes_ids,
    )
    .await;
}

/// Applies and caches lyrics fetched from Musixmatch.
pub(crate) async fn apply_and_store_musixmatch(
    lines: Vec<crate::lyrics::LyricLine>,
    raw: Option<String>,
    ids: &crate::lyrics::types::TrackMatchInfo,
    meta: &TrackMetadata,
    state: &mut StateBundle,
) -> Provider {
    let provider = determine_musixmatch_provider(&lines);
    let format = provider_to_db_format(provider);
    state.update_lyrics(lines, meta, None, Some(provider));
    store_lyrics_in_cache(
        meta,
        raw,
        format,
        &ids.track_isrcs,
        &ids.track_spotify_ids,
        &ids.track_itunes_ids,
    )
    .await;
    provider
}

/// Fetches lyrics from LRCLIB.
pub(crate) async fn try_lrclib(meta: &TrackMetadata, state: &mut StateBundle) -> FetchResult {
    match crate::lyrics::fetch_lyrics_from_lrclib(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
    )
    .await
    {
        Ok((lines, raw, ids)) if !lines.is_empty() => {
            apply_and_store_lrclib(lines, raw, &ids, meta, state).await;
            FetchResult::Success
        }
        Ok(_) => FetchResult::NotFound,
        Err(e) => classify_error(e),
    }
}

/// Maps a Provider enum to the corresponding database LyricsFormat.
pub(crate) fn provider_to_db_format(provider: Provider) -> crate::lyrics::database::LyricsFormat {
    match provider {
        Provider::Lrclib => crate::lyrics::database::LyricsFormat::Lrclib,
        Provider::MusixmatchRichsync => crate::lyrics::database::LyricsFormat::Richsync,
        Provider::MusixmatchSubtitles => crate::lyrics::database::LyricsFormat::Subtitles,
    }
}

/// Maps a database LyricsFormat to the corresponding Provider enum.
pub(crate) fn db_format_to_provider(format: crate::lyrics::database::LyricsFormat) -> Provider {
    match format {
        crate::lyrics::database::LyricsFormat::Lrclib => Provider::Lrclib,
        crate::lyrics::database::LyricsFormat::Richsync => Provider::MusixmatchRichsync,
        crate::lyrics::database::LyricsFormat::Subtitles => Provider::MusixmatchSubtitles,
    }
}

/// Fetches lyrics from Musixmatch.
pub(crate) async fn try_musixmatch(meta: &TrackMetadata, state: &mut StateBundle) -> FetchResult {
    match crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
        meta.spotify_id.as_deref(),
        meta.isrc.as_deref(),
        meta.itunes_id.as_deref(),
    )
    .await
    {
        Ok((lines, raw, ids)) if !lines.is_empty() => {
            apply_and_store_musixmatch(lines, raw, &ids, meta, state).await;
            FetchResult::Success
        }
        Ok(_) => FetchResult::NotFound,
        Err(e) => classify_error(e),
    }
}

/// Determines which Musixmatch format was returned.
pub(crate) fn determine_musixmatch_provider(lines: &[crate::lyrics::LyricLine]) -> Provider {
    if lines.iter().any(|l| l.words.is_some()) {
        Provider::MusixmatchRichsync
    } else {
        Provider::MusixmatchSubtitles
    }
}

/// Attempts to fetch lyrics from the database cache.
/// Returns Some(LyricsFormat) if lyrics were successfully loaded.
pub(crate) async fn try_database(
    meta: &TrackMetadata,
    state: &mut StateBundle,
) -> Option<crate::lyrics::database::LyricsFormat> {
    let (db_result, format) = crate::lyrics::database::fetch_from_database(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
        meta.isrc.as_deref(),
        meta.spotify_id.as_deref(),
        meta.itunes_id.as_deref(),
    )
    .await?;

    match db_result {
        Ok((lines, _raw, _ids)) if !lines.is_empty() => {
            let provider = db_format_to_provider(format);
            let line_count = lines.len();
            state.update_lyrics(lines, meta, None, Some(provider));

            tracing::debug!(
                title = %meta.title,
                artist = %meta.artist,
                lines = line_count,
                format = ?format,
                "Database cache hit"
            );

            Some(format)
        }
        Ok(_) => {
            tracing::debug!(
                title = %meta.title,
                artist = %meta.artist,
                "Empty lyrics in database cache"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                title = %meta.title,
                artist = %meta.artist,
                error = %e,
                "Failed to parse cached lyrics"
            );
            None
        }
    }
}

/// Fetches lyrics from all configured providers.
///
/// Implements Instant Stale-While-Revalidate (SWR) cache emission: if lyrics are cached in SQLite,
/// they are loaded and returned immediately (<1ms) without blocking the UI. If line-synced lyrics
/// are cached and Musixmatch is configured, an asynchronous background task is spawned to upgrade
/// the cache and UI to Richsync karaoke when available.
///
/// On cache miss, if both Musixmatch and LRCLIB are configured and LRCLIB is not explicitly prioritized,
/// concurrent hedged fetching is used to race the providers, displaying whichever lyrics arrive first
/// and seamlessly hot-swapping to Richsync karaoke if Musixmatch finishes afterwards.
pub(crate) async fn fetch_api_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    providers: &[String],
    event_tx: &tokio::sync::mpsc::Sender<crate::event::Event>,
) -> FetchSummary {
    let db_format = try_database(meta, state).await;
    if db_format == Some(crate::lyrics::database::LyricsFormat::Richsync) {
        return FetchSummary::Loaded;
    }

    // Instant SWR Cache Emission:
    // If database already loaded line-level lyrics, return immediately so the UI displays them in <1ms!
    // If musixmatch is enabled, spawn a background task to upgrade to Richsync karaoke asynchronously.
    if db_format.is_some() {
        if providers.iter().any(|p| p == "musixmatch") {
            spawn_background_richsync_upgrade(meta.clone(), event_tx.clone());
        }
        return FetchSummary::Loaded;
    }

    // On cold cache miss: if both Musixmatch and LRCLIB are configured, use concurrent hedged fetching
    // UNLESS the user explicitly ordered LRCLIB first to respect custom provider priority.
    let user_prefers_lrclib = providers.first().map(|s| s.as_str()) == Some("lrclib");
    if !user_prefers_lrclib
        && providers.iter().any(|p| p == "musixmatch")
        && providers.iter().any(|p| p == "lrclib")
    {
        return fetch_concurrent_hedged(meta, state, event_tx).await;
    }

    let mut saw_transient = false;
    let mut saw_not_found = false;
    let mut last_non_transient_err: Option<LyricsError> = None;

    for provider in providers {
        match try_provider(provider, meta, state).await {
            FetchResult::Success => return FetchSummary::Loaded,
            FetchResult::NotFound => {
                saw_not_found = true;
            }
            FetchResult::Transient => {
                saw_transient = true;
            }
            FetchResult::NonTransient(err) => {
                tracing::warn!(
                    provider = %provider,
                    error = %err,
                    track = %meta.title,
                    artist = %meta.artist,
                    "Provider failed with non-transient error, trying next provider"
                );
                last_non_transient_err = Some(err);
            }
        }
    }

    if let Some(err) = last_non_transient_err
        && !saw_not_found
        && !saw_transient
    {
        state.update_lyrics(Vec::new(), meta, Some(err.to_string()), None);
        return FetchSummary::Exhausted;
    }

    if saw_transient && !saw_not_found {
        // Surface the outage so the UI can display it; the event loop schedules
        // a background retry with exponential backoff.
        state.update_lyrics(
            Vec::new(),
            meta,
            Some(NETWORK_RETRY_MESSAGE.to_string()),
            None,
        );
        FetchSummary::Unavailable
    } else {
        state.update_lyrics(Vec::new(), meta, None, None);
        FetchSummary::Exhausted
    }
}

/// Races Musixmatch and LRCLIB concurrently with streaming hot-swap priority.
///
/// Spawns Musixmatch as an independent task so if LRCLIB finishes first, Musixmatch's
/// in-flight request is preserved and handed off to background Richsync upgrade rather
/// than cancelled and restarted.
async fn fetch_concurrent_hedged(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    event_tx: &tokio::sync::mpsc::Sender<crate::event::Event>,
) -> FetchSummary {
    let mut lrclib_fut = std::pin::pin!(crate::lyrics::fetch_lyrics_from_lrclib(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
    ));

    let artist = meta.artist.clone();
    let title = meta.title.clone();
    let album = meta.album.clone();
    let length = meta.length;
    let spotify_id = meta.spotify_id.clone();
    let isrc = meta.isrc.clone();
    let itunes_id = meta.itunes_id.clone();

    let mut mxm_task = tokio::spawn(async move {
        crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
            &artist,
            &title,
            &album,
            length,
            spotify_id.as_deref(),
            isrc.as_deref(),
            itunes_id.as_deref(),
        )
        .await
    });

    tokio::select! {
        mxm_res = &mut mxm_task => {
            let mxm_res = mxm_res.unwrap_or_else(|e| Err(crate::lyrics::types::LyricsError::Api(e.to_string())));
            match mxm_res {
                Ok((lines, raw, ids)) if !lines.is_empty() => {
                    apply_and_store_musixmatch(lines, raw, &ids, meta, state).await;
                    FetchSummary::Loaded
                }
                mxm_other => {
                    // Musixmatch missed or errored; wait for in-flight LRCLIB
                    let lrc_res = lrclib_fut.await;
                    match lrc_res {
                        Ok((lines, raw, ids)) if !lines.is_empty() => {
                            apply_and_store_lrclib(lines, raw, &ids, meta, state).await;
                            FetchSummary::Loaded
                        }
                        lrc_other => {
                            resolve_failed_concurrent_fetch(&mxm_other, &lrc_other, meta, state)
                        }
                    }
                }
            }
        }
        lrc_res = &mut lrclib_fut => {
            match lrc_res {
                Ok((lines, raw, ids)) if !lines.is_empty() => {
                    // Display line lyrics instantly (~100-150ms)!
                    apply_and_store_lrclib(lines, raw, &ids, meta, state).await;

                    // Reuse the in-flight Musixmatch task rather than aborting and re-requesting from scratch
                    let meta_clone = meta.clone();
                    let event_tx_clone = event_tx.clone();
                    tokio::spawn(async move {
                        if let Ok(Ok((mxm_lines, raw, ids))) = mxm_task.await
                            && mxm_lines.iter().any(|l| l.words.is_some())
                        {
                            store_lyrics_in_cache(
                                    &meta_clone,
                                    raw,
                                    crate::lyrics::database::LyricsFormat::Richsync,
                                    &ids.track_isrcs,
                                    &ids.track_spotify_ids,
                                    &ids.track_itunes_ids,
                                )
                                .await;

                                let _ = event_tx_clone
                                    .send(crate::event::Event::UpgradeLyrics {
                                        meta: meta_clone,
                                        lines: mxm_lines,
                                        format: crate::lyrics::database::LyricsFormat::Richsync,
                                    })
                                    .await;
                            }
                    });

                    FetchSummary::Loaded
                }
                lrc_other => {
                    // LRCLIB missed or errored; wait for in-flight Musixmatch
                    let mxm_res = mxm_task.await.unwrap_or_else(|e| Err(crate::lyrics::types::LyricsError::Api(e.to_string())));
                    match mxm_res {
                        Ok((lines, raw, ids)) if !lines.is_empty() => {
                            apply_and_store_musixmatch(lines, raw, &ids, meta, state).await;
                            FetchSummary::Loaded
                        }
                        mxm_other => {
                            resolve_failed_concurrent_fetch(&lrc_other, &mxm_other, meta, state)
                        }
                    }
                }
            }
        }
    }
}

/// Spawns a background task to check if Musixmatch has Richsync word-level karaoke,
/// storing it in SQLite and notifying the event loop via `UpgradeLyrics` without blocking the UI.
pub(crate) fn spawn_background_richsync_upgrade(
    meta: TrackMetadata,
    event_tx: tokio::sync::mpsc::Sender<crate::event::Event>,
) {
    tokio::spawn(async move {
        if let Ok((lines, raw, ids)) = crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
            &meta.artist,
            &meta.title,
            &meta.album,
            meta.length,
            meta.spotify_id.as_deref(),
            meta.isrc.as_deref(),
            meta.itunes_id.as_deref(),
        )
        .await
        {
            let is_richsync = lines.iter().any(|l| l.words.is_some());
            if is_richsync {
                store_lyrics_in_cache(
                    &meta,
                    raw,
                    crate::lyrics::database::LyricsFormat::Richsync,
                    &ids.track_isrcs,
                    &ids.track_spotify_ids,
                    &ids.track_itunes_ids,
                )
                .await;

                let _ = event_tx
                    .send(crate::event::Event::UpgradeLyrics {
                        meta,
                        lines,
                        format: crate::lyrics::database::LyricsFormat::Richsync,
                    })
                    .await;
            }
        }
    });
}

/// Fetches a fresh position from the player or estimates it.
pub(crate) async fn fetch_fresh_position(service: Option<&str>, state: &StateBundle) -> f64 {
    let Some(svc) = service else {
        let estimated = state.player_state.estimate_position();
        tracing::debug!(
            position = estimated,
            "Using estimated position (no service)"
        );
        return estimated;
    };

    match crate::mpris::playback::get_position(svc).await {
        Ok(pos) => {
            tracing::debug!(
                service = %svc,
                position = pos,
                "Fetched fresh position from D-Bus"
            );
            pos
        }
        Err(e) => {
            let estimated = state.player_state.estimate_position();
            tracing::warn!(
                service = %svc,
                error = %e,
                position = estimated,
                "Failed to fetch position, using estimation"
            );
            estimated
        }
    }
}

/// Fetches lyrics and updates position atomically.
///
/// Returns the fresh playback position along with a [`FetchSummary`] so callers
/// can decide whether to schedule a retry.
pub async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    providers: &[String],
    service: Option<&str>,
    event_tx: &tokio::sync::mpsc::Sender<crate::event::Event>,
) -> (f64, FetchSummary) {
    let position_before = state.player_state.estimate_position();
    let start_time = std::time::Instant::now();

    let summary = fetch_api_lyrics(meta, state, providers, event_tx).await;

    let fetch_duration = start_time.elapsed();
    let position = fetch_fresh_position(service, state).await;
    let position_change = position - position_before;

    tracing::debug!(
        position_before,
        position_after = position,
        position_change,
        fetch_duration = ?fetch_duration,
        summary = ?summary,
        "Position updated after lyrics fetch"
    );

    state.update_index(position);
    state.player_state.set_position(position);

    (position, summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_error_retryability() {
        assert!(matches!(
            classify_error(LyricsError::Http(500)),
            FetchResult::Transient
        ));
        assert!(matches!(
            classify_error(LyricsError::Http(429)),
            FetchResult::Transient
        ));
        assert!(matches!(
            classify_error(LyricsError::Http(404)),
            FetchResult::NonTransient(_)
        ));
        assert!(matches!(
            classify_error(LyricsError::Api("bad request".into())),
            FetchResult::NonTransient(_)
        ));
    }

    #[test]
    fn test_retryable_status_boundaries() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(404));
    }

    #[test]
    fn test_default_config_provider_order() {
        let config = crate::Config::default();
        assert_eq!(
            config.providers.first().map(|s| s.as_str()),
            Some("musixmatch")
        );
        assert_eq!(config.providers.get(1).map(|s| s.as_str()), Some("lrclib"));
        assert!(!config.romanize);
    }

    #[test]
    fn test_determine_musixmatch_provider_format() {
        let plain_lines = vec![crate::lyrics::LyricLine::new(0.0, "hello")];
        assert_eq!(
            determine_musixmatch_provider(&plain_lines),
            Provider::MusixmatchSubtitles
        );

        let word = crate::lyrics::types::WordTiming {
            start: 0.0,
            end: 1.0,
            text: "hello".into(),
            grapheme_boundaries: vec![0, 5],
            grapheme_times: vec![],
        };
        let rich_lines =
            vec![crate::lyrics::LyricLine::new(0.0, "hello").with_words(Some(vec![word]))];
        assert_eq!(
            determine_musixmatch_provider(&rich_lines),
            Provider::MusixmatchRichsync
        );
    }

    #[test]
    fn test_resolve_failed_concurrent_fetch_one_ok_one_transient() {
        let mut state = StateBundle::new();
        let meta = TrackMetadata {
            title: "Test Song".into(),
            artist: "Test Artist".into(),
            ..Default::default()
        };
        let ok_empty: ProviderResult = Ok((
            Vec::new(),
            None,
            crate::lyrics::types::TrackMatchInfo::default(),
        ));
        let transient_err: ProviderResult = Err(LyricsError::Http(503));

        // When LRCLIB returns Ok (empty/404) and Musixmatch fails transiently
        let outcome1 =
            resolve_failed_concurrent_fetch(&ok_empty, &transient_err, &meta, &mut state);
        assert_eq!(outcome1, FetchSummary::Exhausted);
        assert_ne!(
            state.player_state.err.as_deref(),
            Some(NETWORK_RETRY_MESSAGE)
        );

        // When Musixmatch returns Ok (empty/not found) and LRCLIB fails transiently
        let outcome2 =
            resolve_failed_concurrent_fetch(&transient_err, &ok_empty, &meta, &mut state);
        assert_eq!(outcome2, FetchSummary::Exhausted);
        assert_ne!(
            state.player_state.err.as_deref(),
            Some(NETWORK_RETRY_MESSAGE)
        );
    }

    #[test]
    fn test_resolve_failed_concurrent_fetch_both_transient() {
        let mut state = StateBundle::new();
        let meta = TrackMetadata {
            title: "Test Song".into(),
            artist: "Test Artist".into(),
            ..Default::default()
        };
        let transient1: ProviderResult = Err(LyricsError::Http(500));
        let transient2: ProviderResult = Err(LyricsError::Http(503));

        let outcome = resolve_failed_concurrent_fetch(&transient1, &transient2, &meta, &mut state);
        assert_eq!(outcome, FetchSummary::Unavailable);
        assert_eq!(
            state.player_state.err.as_deref(),
            Some(NETWORK_RETRY_MESSAGE)
        );
    }

    #[test]
    fn test_resolve_failed_concurrent_fetch_one_nontransient() {
        let mut state = StateBundle::new();
        let meta = TrackMetadata {
            title: "Test Song".into(),
            artist: "Test Artist".into(),
            ..Default::default()
        };
        let non_transient: ProviderResult = Err(LyricsError::Http(400));
        let transient: ProviderResult = Err(LyricsError::Http(503));

        let outcome =
            resolve_failed_concurrent_fetch(&non_transient, &transient, &meta, &mut state);
        assert_eq!(outcome, FetchSummary::Exhausted);
        assert_ne!(
            state.player_state.err.as_deref(),
            Some(NETWORK_RETRY_MESSAGE)
        );
    }
}
