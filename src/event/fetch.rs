//! Multi-provider lyrics fetching and caching pipeline.

use crate::lyrics::types::{LyricsError, is_retryable_status};
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

/// Classifies a provider error into retryable vs fatal.
fn classify_error(err: LyricsError) -> FetchResult {
    match err {
        LyricsError::Network(_) => FetchResult::Transient,
        LyricsError::Http(code) if is_retryable_status(code) => FetchResult::Transient,
        e => FetchResult::NonTransient(e),
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
        // Note: meta.isrc from local file tags is intentionally NOT merged here
        // to prevent saving incorrect or corrupted user file tags into the database.
        // Only authoritative ISRCs returned directly by providers are stored.
        let mut spotify_ids = provider_spotify_ids.to_vec();
        if let Some(meta_sid) = &meta.spotify_id
            && !spotify_ids.iter().any(|s| s.eq_ignore_ascii_case(meta_sid))
        {
            spotify_ids.push(meta_sid.clone());
        }

        let mut itunes_ids = provider_itunes_ids.to_vec();
        if let Some(meta_itunes) = &meta.itunes_id
            && !itunes_ids
                .iter()
                .any(|s| s.eq_ignore_ascii_case(meta_itunes))
        {
            itunes_ids.push(meta_itunes.clone());
        }

        crate::lyrics::database::store_in_database(
            &meta.artist,
            &meta.title,
            &meta.album,
            meta.length,
            format,
            raw_text,
            provider_isrcs,
            &spotify_ids,
            &itunes_ids,
        )
        .await;
    }
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
            let provider = determine_musixmatch_provider(&lines);
            state.update_lyrics(lines, meta, None, Some(provider));

            let format = provider_to_db_format(provider);
            store_lyrics_in_cache(
                meta,
                raw,
                format,
                &ids.track_isrcs,
                &ids.track_spotify_ids,
                &ids.track_itunes_ids,
            )
            .await;

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
/// On cache miss, if both Musixmatch and LRCLIB are configured, concurrent hedged fetching is used
/// to race the providers, displaying whichever lyrics arrive first and seamlessly hot-swapping to
/// Richsync karaoke if Musixmatch finishes afterwards.
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
    if providers.iter().any(|p| p == "musixmatch") && providers.iter().any(|p| p == "lrclib") {
        return fetch_concurrent_hedged(meta, state, event_tx).await;
    }

    let mut saw_transient = false;

    for provider in providers {
        match try_provider(provider, meta, state).await {
            FetchResult::Success => return FetchSummary::Loaded,
            FetchResult::NotFound => continue,
            FetchResult::Transient => {
                saw_transient = true;
            }
            FetchResult::NonTransient(err) => {
                tracing::warn!(
                    provider = %provider,
                    error = %err,
                    track = %meta.title,
                    artist = %meta.artist,
                    "Provider failed to fetch lyrics"
                );
                state.update_lyrics(Vec::new(), meta, Some(err.to_string()), None);
                return FetchSummary::Exhausted;
            }
        }
    }

    if saw_transient {
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

    let mut mxm_fut = std::pin::pin!(crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
        meta.spotify_id.as_deref(),
        meta.isrc.as_deref(),
        meta.itunes_id.as_deref(),
    ));

    tokio::select! {
        mxm_res = &mut mxm_fut => {
            match mxm_res {
                Ok((lines, raw, ids)) if !lines.is_empty() => {
                    let format = if lines.iter().any(|l| l.words.is_some()) {
                        crate::lyrics::database::LyricsFormat::Richsync
                    } else {
                        crate::lyrics::database::LyricsFormat::Subtitles
                    };
                    let provider = db_format_to_provider(format);
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
                    FetchSummary::Loaded
                }
                _ => {
                    // Musixmatch missed or errored; wait for in-flight LRCLIB
                    match lrclib_fut.await {
                        Ok((lines, raw, ids)) if !lines.is_empty() => {
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
                            FetchSummary::Loaded
                        }
                        Ok(_) => {
                            state.update_lyrics(Vec::new(), meta, None, None);
                            FetchSummary::Exhausted
                        }
                        Err(e) => {
                            if matches!(classify_error(e), FetchResult::Transient) {
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
                    }
                }
            }
        }
        lrc_res = &mut lrclib_fut => {
            match lrc_res {
                Ok((lines, raw, ids)) if !lines.is_empty() => {
                    // Display line lyrics instantly (~100-150ms)!
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

                    // Asynchronously check Musixmatch to upgrade to Richsync karaoke
                    spawn_background_richsync_upgrade(meta.clone(), event_tx.clone());

                    FetchSummary::Loaded
                }
                _ => {
                    // LRCLIB missed or errored; wait for in-flight Musixmatch
                    match mxm_fut.await {
                        Ok((lines, raw, ids)) if !lines.is_empty() => {
                            let format = if lines.iter().any(|l| l.words.is_some()) {
                                crate::lyrics::database::LyricsFormat::Richsync
                            } else {
                                crate::lyrics::database::LyricsFormat::Subtitles
                            };
                            let provider = db_format_to_provider(format);
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
                            FetchSummary::Loaded
                        }
                        Ok(_) => {
                            state.update_lyrics(Vec::new(), meta, None, None);
                            FetchSummary::Exhausted
                        }
                        Err(e) => {
                            if matches!(classify_error(e), FetchResult::Transient) {
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
                    .send(crate::event::Event::Mpris(Box::new(
                        crate::event::MprisEvent::UpgradeLyrics {
                            meta,
                            lines,
                            format: crate::lyrics::database::LyricsFormat::Richsync,
                        },
                    )))
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
}
