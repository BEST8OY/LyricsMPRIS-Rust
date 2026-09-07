//! Multi-provider lyrics fetching and caching pipeline.

use crate::lyrics::types::{LyricsError, is_retryable_status};
use crate::mpris::TrackMetadata;
use crate::state::Provider;

/// Message surfaced to the UI while providers are unreachable.
pub(crate) const NETWORK_RETRY_MESSAGE: &str = "Network unavailable - retrying...";

/// Returns true if the error is considered transient (temporary network issue, rate limit, server outage).
fn is_transient(err: &LyricsError) -> bool {
    match err {
        LyricsError::Network(_) => true,
        LyricsError::Http(code) if is_retryable_status(*code) => true,
        _ => false,
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

/// Determines which Musixmatch format was returned based on presence of word timings.
pub(crate) fn determine_musixmatch_provider(lines: &[crate::lyrics::LyricLine]) -> Provider {
    if lines.iter().any(|l| l.words.is_some()) {
        Provider::MusixmatchRichsync
    } else {
        Provider::MusixmatchSubtitles
    }
}

/// Asynchronous fetch outcome message sent to the event loop.
#[derive(Debug)]
pub enum FetchOutcome {
    /// Lyrics were retrieved (from SQLite cache or provider)
    Success {
        epoch: u64,
        meta: TrackMetadata,
        lines: Vec<crate::lyrics::LyricLine>,
        provider: Provider,
        position: f64,
    },
    /// Richsync karaoke upgrade discovered asynchronously for current track
    Upgraded {
        epoch: u64,
        meta: TrackMetadata,
        lines: Vec<crate::lyrics::LyricLine>,
        format: crate::lyrics::database::LyricsFormat,
    },
    /// Providers were queried authoritatively: no lyrics exist
    Exhausted {
        epoch: u64,
        meta: TrackMetadata,
        err: Option<String>,
    },
    /// All providers failed with transient network errors; schedule retry
    Unavailable {
        epoch: u64,
        meta: TrackMetadata,
        service: String,
        message: String,
    },
}

impl FetchOutcome {
    #[must_use]
    pub fn epoch(&self) -> u64 {
        match self {
            FetchOutcome::Success { epoch, .. } => *epoch,
            FetchOutcome::Upgraded { epoch, .. } => *epoch,
            FetchOutcome::Exhausted { epoch, .. } => *epoch,
            FetchOutcome::Unavailable { epoch, .. } => *epoch,
        }
    }
}

/// Helper to query live D-Bus position for a player service.
async fn fetch_position(service: Option<&str>) -> f64 {
    if let Some(svc) = service {
        crate::mpris::playback::get_position(svc)
            .await
            .unwrap_or(0.0)
    } else {
        0.0
    }
}

/// Spawns an isolated background fetch task carrying an epoch identifier.
///
/// Returns immediately without blocking the caller. When lyrics arrive or miss,
/// an authoritative [`FetchOutcome`] is dispatched to `outcome_tx`.
pub fn spawn_fetch(
    epoch: u64,
    meta: TrackMetadata,
    providers: Vec<String>,
    service: Option<String>,
    outcome_tx: tokio::sync::mpsc::Sender<FetchOutcome>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // 1. Instant SQLite cache lookup (<1ms)
        let db_match = crate::lyrics::database::fetch_from_database(
            &meta.artist,
            &meta.title,
            &meta.album,
            meta.length,
            meta.isrc.as_deref(),
            meta.spotify_id.as_deref(),
            meta.itunes_id.as_deref(),
        )
        .await;

        if let Some((Ok((lines, _raw, _ids)), format)) = db_match
            && !lines.is_empty()
        {
            let provider = db_format_to_provider(format);
            let position = fetch_position(service.as_deref()).await;

            let _ = outcome_tx
                .send(FetchOutcome::Success {
                    epoch,
                    meta: meta.clone(),
                    lines,
                    provider,
                    position,
                })
                .await;

            // Instant SWR: if cached lyrics are line-level and musixmatch is configured,
            // query Richsync karaoke upgrade asynchronously in the background.
            if format != crate::lyrics::database::LyricsFormat::Richsync
                && providers.iter().any(|p| p == "musixmatch")
                && let Ok((mxm_lines, mxm_raw, mxm_ids)) =
                    crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
                        &meta.artist,
                        &meta.title,
                        &meta.album,
                        meta.length,
                        meta.spotify_id.as_deref(),
                        meta.isrc.as_deref(),
                        meta.itunes_id.as_deref(),
                    )
                    .await
                && mxm_lines.iter().any(|l| l.words.is_some())
            {
                store_lyrics_in_cache(
                    &meta,
                    mxm_raw,
                    crate::lyrics::database::LyricsFormat::Richsync,
                    &mxm_ids.track_isrcs,
                    &mxm_ids.track_spotify_ids,
                    &mxm_ids.track_itunes_ids,
                )
                .await;

                let _ = outcome_tx
                    .send(FetchOutcome::Upgraded {
                        epoch,
                        meta,
                        lines: mxm_lines,
                        format: crate::lyrics::database::LyricsFormat::Richsync,
                    })
                    .await;
            }
            return;
        }

        // 2. Cold cache miss: API retrieval
        let user_prefers_lrclib = providers.first().map(|s| s.as_str()) == Some("lrclib");
        if !user_prefers_lrclib
            && providers.iter().any(|p| p == "musixmatch")
            && providers.iter().any(|p| p == "lrclib")
        {
            execute_hedged_fetch(epoch, meta, service, outcome_tx).await;
            return;
        }

        execute_sequential_fetch(epoch, meta, providers, service, outcome_tx).await;
    })
}

/// Races Musixmatch and LRCLIB in background without holding state locks.
async fn execute_hedged_fetch(
    epoch: u64,
    meta: TrackMetadata,
    service: Option<String>,
    outcome_tx: tokio::sync::mpsc::Sender<FetchOutcome>,
) {
    let lrc_artist = meta.artist.clone();
    let lrc_title = meta.title.clone();
    let lrc_album = meta.album.clone();
    let length = meta.length;
    let spotify_id = meta.spotify_id.clone();
    let isrc = meta.isrc.clone();
    let itunes_id = meta.itunes_id.clone();

    let mut lrclib_fut = std::pin::pin!(crate::lyrics::fetch_lyrics_from_lrclib(
        &lrc_artist,
        &lrc_title,
        &lrc_album,
        length,
    ));

    let mxm_artist = meta.artist.clone();
    let mxm_title = meta.title.clone();
    let mxm_album = meta.album.clone();

    let mut mxm_task = tokio::spawn(async move {
        crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
            &mxm_artist,
            &mxm_title,
            &mxm_album,
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
                    let provider = determine_musixmatch_provider(&lines);
                    let format = provider_to_db_format(provider);
                    store_lyrics_in_cache(&meta, raw, format, &ids.track_isrcs, &ids.track_spotify_ids, &ids.track_itunes_ids).await;
                    let position = fetch_position(service.as_deref()).await;
                    let _ = outcome_tx.send(FetchOutcome::Success {
                        epoch,
                        meta,
                        lines,
                        provider,
                        position,
                    }).await;
                }
                mxm_other => {
                    let lrc_res = lrclib_fut.await;
                    match lrc_res {
                        Ok((lines, raw, ids)) if !lines.is_empty() => {
                            store_lyrics_in_cache(&meta, raw, crate::lyrics::database::LyricsFormat::Lrclib, &ids.track_isrcs, &ids.track_spotify_ids, &ids.track_itunes_ids).await;
                            let position = fetch_position(service.as_deref()).await;
                            let _ = outcome_tx.send(FetchOutcome::Success {
                                epoch,
                                meta,
                                lines,
                                provider: Provider::Lrclib,
                                position,
                            }).await;
                        }
                        lrc_other => {
                            handle_failed_hedged_fetch(epoch, meta, service, mxm_other, lrc_other, outcome_tx).await;
                        }
                    }
                }
            }
        }
        lrc_res = &mut lrclib_fut => {
            match lrc_res {
                Ok((lines, raw, ids)) if !lines.is_empty() => {
                    store_lyrics_in_cache(&meta, raw, crate::lyrics::database::LyricsFormat::Lrclib, &ids.track_isrcs, &ids.track_spotify_ids, &ids.track_itunes_ids).await;
                    let position = fetch_position(service.as_deref()).await;
                    let _ = outcome_tx.send(FetchOutcome::Success {
                        epoch,
                        meta: meta.clone(),
                        lines,
                        provider: Provider::Lrclib,
                        position,
                    }).await;

                    // Reuse in-flight Musixmatch task for Richsync upgrade
                    if let Ok(Ok((mxm_lines, mxm_raw, mxm_ids))) = mxm_task.await
                        && mxm_lines.iter().any(|l| l.words.is_some())
                    {
                        store_lyrics_in_cache(
                            &meta,
                            mxm_raw,
                            crate::lyrics::database::LyricsFormat::Richsync,
                            &mxm_ids.track_isrcs,
                            &mxm_ids.track_spotify_ids,
                            &mxm_ids.track_itunes_ids,
                        ).await;

                        let _ = outcome_tx.send(FetchOutcome::Upgraded {
                            epoch,
                            meta,
                            lines: mxm_lines,
                            format: crate::lyrics::database::LyricsFormat::Richsync,
                        }).await;
                    }
                }
                lrc_other => {
                    let mxm_res = mxm_task.await.unwrap_or_else(|e| Err(crate::lyrics::types::LyricsError::Api(e.to_string())));
                    match mxm_res {
                        Ok((lines, raw, ids)) if !lines.is_empty() => {
                            let provider = determine_musixmatch_provider(&lines);
                            let format = provider_to_db_format(provider);
                            store_lyrics_in_cache(&meta, raw, format, &ids.track_isrcs, &ids.track_spotify_ids, &ids.track_itunes_ids).await;
                            let position = fetch_position(service.as_deref()).await;
                            let _ = outcome_tx.send(FetchOutcome::Success {
                                epoch,
                                meta,
                                lines,
                                provider,
                                position,
                            }).await;
                        }
                        mxm_other => {
                            handle_failed_hedged_fetch(epoch, meta, service, mxm_other, lrc_other, outcome_tx).await;
                        }
                    }
                }
            }
        }
    }
}

async fn handle_failed_hedged_fetch(
    epoch: u64,
    meta: TrackMetadata,
    service: Option<String>,
    mxm: crate::lyrics::types::ProviderResult,
    lrc: crate::lyrics::types::ProviderResult,
    outcome_tx: tokio::sync::mpsc::Sender<FetchOutcome>,
) {
    if mxm.is_ok() || lrc.is_ok() {
        let _ = outcome_tx
            .send(FetchOutcome::Exhausted {
                epoch,
                meta,
                err: None,
            })
            .await;
        return;
    }

    let a_transient = mxm.as_ref().err().is_some_and(is_transient);
    let b_transient = lrc.as_ref().err().is_some_and(is_transient);

    if a_transient && b_transient {
        let _ = outcome_tx
            .send(FetchOutcome::Unavailable {
                epoch,
                meta,
                service: service.unwrap_or_default(),
                message: NETWORK_RETRY_MESSAGE.to_string(),
            })
            .await;
    } else {
        let _ = outcome_tx
            .send(FetchOutcome::Exhausted {
                epoch,
                meta,
                err: None,
            })
            .await;
    }
}

async fn execute_sequential_fetch(
    epoch: u64,
    meta: TrackMetadata,
    providers: Vec<String>,
    service: Option<String>,
    outcome_tx: tokio::sync::mpsc::Sender<FetchOutcome>,
) {
    let mut saw_transient = false;
    let mut saw_not_found = false;
    let mut last_non_transient_err: Option<LyricsError> = None;

    for provider in &providers {
        let res = match provider.as_str() {
            "lrclib" => crate::lyrics::fetch_lyrics_from_lrclib(
                &meta.artist,
                &meta.title,
                &meta.album,
                meta.length,
            )
            .await
            .map(|(lines, raw, ids)| {
                (
                    lines,
                    raw,
                    ids,
                    crate::lyrics::database::LyricsFormat::Lrclib,
                    Provider::Lrclib,
                )
            }),
            "musixmatch" => crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
                &meta.artist,
                &meta.title,
                &meta.album,
                meta.length,
                meta.spotify_id.as_deref(),
                meta.isrc.as_deref(),
                meta.itunes_id.as_deref(),
            )
            .await
            .map(|(lines, raw, ids)| {
                let prov = determine_musixmatch_provider(&lines);
                let fmt = provider_to_db_format(prov);
                (lines, raw, ids, fmt, prov)
            }),
            _ => continue,
        };

        match res {
            Ok((lines, raw, ids, format, provider)) if !lines.is_empty() => {
                store_lyrics_in_cache(
                    &meta,
                    raw,
                    format,
                    &ids.track_isrcs,
                    &ids.track_spotify_ids,
                    &ids.track_itunes_ids,
                )
                .await;
                let position = fetch_position(service.as_deref()).await;
                let _ = outcome_tx
                    .send(FetchOutcome::Success {
                        epoch,
                        meta,
                        lines,
                        provider,
                        position,
                    })
                    .await;
                return;
            }
            Ok(_) => {
                saw_not_found = true;
            }
            Err(e) => {
                if is_transient(&e) {
                    saw_transient = true;
                } else {
                    last_non_transient_err = Some(e);
                }
            }
        }
    }

    if saw_transient && !saw_not_found {
        let _ = outcome_tx
            .send(FetchOutcome::Unavailable {
                epoch,
                meta,
                service: service.unwrap_or_default(),
                message: NETWORK_RETRY_MESSAGE.to_string(),
            })
            .await;
    } else {
        let err = last_non_transient_err
            .filter(|_| !saw_not_found)
            .map(|e| e.to_string());
        let _ = outcome_tx
            .send(FetchOutcome::Exhausted { epoch, meta, err })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_transient_error_classification() {
        assert!(is_transient(&LyricsError::Http(500)));
        assert!(is_transient(&LyricsError::Http(429)));
        assert!(is_transient(&LyricsError::Http(503)));
        assert!(!is_transient(&LyricsError::Http(404)));
        assert!(!is_transient(&LyricsError::Api("bad request".into())));
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

    #[tokio::test]
    async fn test_handle_failed_hedged_fetch_both_transient() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let meta = TrackMetadata {
            title: "Test Song".into(),
            artist: "Test Artist".into(),
            ..Default::default()
        };
        let transient1 = Err(LyricsError::Http(500));
        let transient2 = Err(LyricsError::Http(503));

        handle_failed_hedged_fetch(1, meta, Some("service".into()), transient1, transient2, tx)
            .await;
        let outcome = rx.recv().await.unwrap();
        assert!(matches!(outcome, FetchOutcome::Unavailable { .. }));
    }

    #[tokio::test]
    async fn test_handle_failed_hedged_fetch_one_ok_one_transient() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let meta = TrackMetadata {
            title: "Test Song".into(),
            artist: "Test Artist".into(),
            ..Default::default()
        };
        let ok_empty = Ok((Vec::new(), None, Default::default()));
        let transient = Err(LyricsError::Http(503));

        handle_failed_hedged_fetch(1, meta, Some("service".into()), ok_empty, transient, tx).await;
        let outcome = rx.recv().await.unwrap();
        assert!(matches!(outcome, FetchOutcome::Exhausted { .. }));
    }

    #[tokio::test]
    async fn test_handle_failed_hedged_fetch_one_nontransient() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let meta = TrackMetadata {
            title: "Test Song".into(),
            artist: "Test Artist".into(),
            ..Default::default()
        };
        let non_transient = Err(LyricsError::Http(400));
        let transient = Err(LyricsError::Http(503));

        handle_failed_hedged_fetch(
            1,
            meta,
            Some("service".into()),
            non_transient,
            transient,
            tx,
        )
        .await;
        let outcome = rx.recv().await.unwrap();
        assert!(matches!(outcome, FetchOutcome::Exhausted { .. }));
    }
}
