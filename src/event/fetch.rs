//! Multi-provider lyrics fetching and caching pipeline.

use crate::mpris::TrackMetadata;
use crate::state::{Provider, StateBundle};

/// Result of a lyrics fetch attempt from a single provider.
///
/// This enum classifies failures as transient (retry with next provider)
/// or non-transient (stop trying and report error).
pub(crate) enum FetchResult {
    /// Lyrics fetched successfully
    Success,
    /// Transient error (no lyrics found, network issue) - try next provider
    Transient,
    /// Non-transient error (API error, parse error) - stop trying
    NonTransient(crate::lyrics::LyricsError),
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
        _ => FetchResult::Transient,
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
        Ok(_) => FetchResult::Transient,
        Err(crate::lyrics::LyricsError::Network(_)) => FetchResult::Transient,
        Err(e) => FetchResult::NonTransient(e),
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
            let provider = determine_musixmatch_provider(&lines, &raw);
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
        Ok(_) => FetchResult::Transient,
        Err(crate::lyrics::LyricsError::Network(_)) => FetchResult::Transient,
        Err(e) => FetchResult::NonTransient(e),
    }
}

/// Determines which Musixmatch format was returned.
pub(crate) fn determine_musixmatch_provider(
    lines: &[crate::lyrics::LyricLine],
    raw: &Option<String>,
) -> Provider {
    let has_words = lines.iter().any(|l| l.words.is_some());
    let is_richsync = raw
        .as_deref()
        .is_some_and(|r| r.starts_with(";;richsync=1"));

    if has_words || is_richsync {
        Provider::MusixmatchRichsync
    } else {
        Provider::MusixmatchSubtitles
    }
}

/// Determines provider type from raw lyrics format.
pub(crate) fn detect_provider_from_raw(raw: &Option<String>) -> Option<Provider> {
    raw.as_deref().map(|text| {
        let trimmed = text.trim_start();
        if trimmed.starts_with("[{") {
            if trimmed.contains("\"ts\":")
                || trimmed.contains("\"l\":[")
                || trimmed.contains("\"words\":[")
            {
                Provider::MusixmatchRichsync
            } else {
                Provider::MusixmatchSubtitles
            }
        } else {
            Provider::Lrclib
        }
    })
}

/// Attempts to fetch lyrics from the database cache.
pub(crate) async fn try_database(meta: &TrackMetadata, state: &mut StateBundle) -> bool {
    let Some(db_result) = crate::lyrics::database::fetch_from_database(
        &meta.artist,
        &meta.title,
        &meta.album,
        meta.length,
        meta.isrc.as_deref(),
        meta.spotify_id.as_deref(),
        meta.itunes_id.as_deref(),
    )
    .await
    else {
        return false;
    };

    match db_result {
        Ok((lines, raw, _ids)) if !lines.is_empty() => {
            let provider = detect_provider_from_raw(&raw);
            let line_count = lines.len();
            state.update_lyrics(lines, meta, None, provider);

            tracing::debug!(
                title = %meta.title,
                artist = %meta.artist,
                lines = line_count,
                "Database cache hit"
            );

            true
        }
        Ok(_) => {
            tracing::debug!(
                title = %meta.title,
                artist = %meta.artist,
                "Empty lyrics in database cache"
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                title = %meta.title,
                artist = %meta.artist,
                error = %e,
                "Failed to parse cached lyrics"
            );
            false
        }
    }
}

/// Fetches lyrics from all configured providers in order.
pub(crate) async fn fetch_api_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    providers: &[String],
) {
    if try_database(meta, state).await {
        return;
    }

    for provider in providers {
        match try_provider(provider, meta, state).await {
            FetchResult::Success => return,
            FetchResult::Transient => continue,
            FetchResult::NonTransient(err) => {
                tracing::warn!(
                    provider = %provider,
                    error = %err,
                    track = %meta.title,
                    artist = %meta.artist,
                    "Provider failed to fetch lyrics"
                );
                state.update_lyrics(Vec::new(), meta, Some(err.to_string()), None);
                return;
            }
        }
    }

    state.update_lyrics(Vec::new(), meta, None, None);
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
pub async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    providers: &[String],
    service: Option<&str>,
) -> f64 {
    let position_before = state.player_state.estimate_position();
    let start_time = std::time::Instant::now();

    fetch_api_lyrics(meta, state, providers).await;

    let fetch_duration = start_time.elapsed();
    let position = fetch_fresh_position(service, state).await;
    let position_change = position - position_before;

    tracing::debug!(
        position_before,
        position_after = position,
        position_change,
        fetch_duration = ?fetch_duration,
        "Position updated after lyrics fetch"
    );

    state.update_index(position);
    state.player_state.set_position(position);

    position
}
