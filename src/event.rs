use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update, Provider};
use tokio::sync::mpsc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub enum MprisEvent {
    PlayerUpdate(TrackMetadata, f64, String),
    Seeked(TrackMetadata, f64, String),
}

#[derive(Debug)]
pub enum Event {
    Mpris(MprisEvent),
    Shutdown,
}
// Track last sent (version + playing bit) to avoid redundant UI updates.
static LAST_SENT_VERSION: AtomicU64 = AtomicU64::new(0);

pub async fn send_update(state: &StateBundle, update_tx: &mpsc::Sender<Update>, force: bool) {
    let version = state.version;
    let playing_bit: u64 = if state.player_state.playing { 1 } else { 0 };
    let key = (version << 1) | playing_bit;

    // Cheap early-out if nothing changed and caller didn't force an update.
    let last_key = LAST_SENT_VERSION.load(Ordering::Relaxed);
    if !force && key == last_key {
        return;
    }

    // Avoid allocating/cloning all fields when there's nothing worth sending.
    let should_send = force
        || !state.lyric_state.lines.is_empty()
        || state.player_state.err.is_some();

    if !should_send {
        return;
    }

    // Use the backend's estimate for position when playing so the UI sees an
    // up-to-date snapshot anchored to the time the Update was produced.
    let position = if state.player_state.playing {
        state.player_state.estimate_position()
    } else {
        state.player_state.position
    };

    let update = Update {
        lines: state.lyric_state.lines.clone(),
        index: state.lyric_state.index,
        position,
        err: state.player_state.err.as_ref().map(|e| e.to_string()),
        version,
        playing: state.player_state.playing,
        artist: state.player_state.artist.clone(),
        title: state.player_state.title.clone(),
        album: state.player_state.album.clone(),
        provider: state.provider.clone(),
    };

    if update_tx.send(update).await.is_ok() {
        LAST_SENT_VERSION.store(key, Ordering::Relaxed);
    }
}

async fn fetch_api_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    debug_log: bool,
    providers: &[String],
) {
    // Helper to handle non-network errors: set empty lyrics and stop trying.
    let mut handle_non_transient_error = |err: crate::lyrics::LyricsError| {
        if debug_log {
            eprintln!("[LyricsMPRIS] provider error: {}", err);
        }
        state.update_lyrics(Vec::new(), meta, Some(err.to_string()), None);
    };

    for prov in providers.iter() {
        match prov.as_str() {
            "lrclib" => {
                match crate::lyrics::fetch_lyrics_from_lrclib(
                    &meta.artist,
                    &meta.title,
                    &meta.album,
                    meta.length,
                )
                .await
                {
                    Ok((lines, _raw)) if !lines.is_empty() => {
                        state.update_lyrics(lines, meta, None, Some(Provider::Lrclib));
                        return;
                    }
                    Ok(_) => { /* no lyrics from this provider, try next */ }
                    Err(e) => match e {
                        crate::lyrics::LyricsError::Network(_) => { /* transient: try next */ }
                        _ => {
                            handle_non_transient_error(e);
                            return;
                        }
                    },
                }
            }
            "musixmatch" => {
                match crate::lyrics::fetch_lyrics_from_musixmatch_usertoken(
                    &meta.artist,
                    &meta.title,
                    &meta.album,
                    meta.length,
                    meta.spotify_id.as_deref(),
                )
                .await
                {
                    Ok((lines, raw)) if !lines.is_empty() => {
                        let provider_tag = if lines.iter().any(|l| l.words.is_some())
                            || raw.as_ref().map(|r| r.starts_with(";;richsync=1")).unwrap_or(false)
                        {
                            Some(Provider::MusixmatchRichsync)
                        } else {
                            Some(Provider::MusixmatchSubtitles)
                        };
                        state.update_lyrics(lines, meta, None, provider_tag);
                        return;
                    }
                    Ok(_) => { /* no lyrics from this provider, try next */ }
                    Err(e) => match e {
                        crate::lyrics::LyricsError::Network(_) => { /* transient: try next */ }
                        _ => {
                            handle_non_transient_error(e);
                            return;
                        }
                    },
                }
            }
            other => {
                if debug_log {
                    eprintln!("[LyricsMPRIS] unknown provider: {}", other);
                }
            }
        }
    }

    // No provider gave lyrics; record an empty lyrics set (no error).
    state.update_lyrics(Vec::new(), meta, None, None);
}

pub async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    state: &mut StateBundle,
    debug_log: bool,
    providers: &[String],
    service: Option<&str>,
) -> f64 {
    // Fetch lyrics from configured providers first (may take network time).
    fetch_api_lyrics(meta, state, debug_log, providers).await;

    // Try to get an up-to-date position from the MPRIS playback API when we
    // have a service string. This ensures the internal timer anchor is set to
    // the real playback position after lyrics are loaded into the UI. If the
    // D-Bus call fails or no service is available, fall back to the state's
    // estimated position.
    let position = if let Some(svc) = service {
        match crate::mpris::playback::get_position(svc).await {
            Ok(p) => p,
            Err(e) => {
                if debug_log {
                    eprintln!("[LyricsMPRIS] D-Bus error getting position after lyrics fetch: {}", e);
                }
                state.player_state.estimate_position()
            }
        }
    } else {
        state.player_state.estimate_position()
    };

    state.update_index(position);
    state.player_state.set_position(position);
    position
}

pub async fn process_event(
    event: Event,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    match event {
        Event::Mpris(ev) => handle_mpris_event(ev, state, update_tx, latest_meta).await,
        Event::Shutdown => send_update(state, update_tx, true).await,
    }
}

async fn handle_mpris_event(
    event: MprisEvent,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
) {
    let (meta, position, service, is_player_update) = match event {
        MprisEvent::PlayerUpdate(m, p, s) => (m, p, s, true),
        MprisEvent::Seeked(m, p, s) => (m, p, s, false),
    };
    if service.is_empty() {
        state.clear_lyrics();
        state.player_state = Default::default();
        send_update(state, update_tx, true).await;
        return;
    }

    let playback_status = crate::mpris::get_playback_status(&service).await.unwrap_or_default();

    if playback_status == "Stopped" {
        state.clear_lyrics();
        state.player_state = Default::default();
        send_update(state, update_tx, true).await;
        return;
    }

    let is_new_track = state.player_state.has_changed(&meta);

    if is_player_update && is_new_track {
        // Clear immediately and enqueue the full metadata for fetching lyrics
        state.clear_lyrics();
        *latest_meta = Some((meta, position, service));

        // Some players don't report PlaybackStatus on metadata change. If we
        // couldn't obtain a playback status (empty string) avoid forcing a
        // pause by leaving the previous playing state intact and only update
        // the reported position. When a real playback status update arrives
        // we'll update the playing flag.
        if playback_status.is_empty() {
            state.player_state.set_position(position);
        } else {
            let playing = playback_status == "Playing";
            state.player_state.update_playback_dbus(playing, position);
        }

        // immediate UI clear
        send_update(state, update_tx, true).await;
        return;
    }

    let prev_playing = state.player_state.playing;

    // If we couldn't fetch a playback status, don't modify the playing
    // flag (avoid forcing a pause). Only update the position so the UI
    // remains anchored correctly.
    let playing_opt: Option<bool> = if playback_status.is_empty() {
        None
    } else {
        Some(playback_status == "Playing")
    };

    if let Some(playing) = playing_opt {
        state.player_state.update_playback_dbus(playing, position);
    } else {
        state.player_state.set_position(position);
    }

    let changed_index = state.update_index(state.player_state.estimate_position());

    if prev_playing != state.player_state.playing || (changed_index && !is_new_track) {
        send_update(state, update_tx, false).await;
    }
}

async fn handle_latest_meta_update(
    state: &mut StateBundle,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
    debug_log: bool,
    providers: &[String],
    update_tx: &mpsc::Sender<Update>,
) -> bool {
    if let Some((meta, _pos, service)) = latest_meta.take() {
        // We stored (meta, pos, service) earlier; use the captured `service`
        // string to query a fresh position from MPRIS after lyrics are
        // fetched so the internal timer is anchored correctly.
        let _position = fetch_and_update_lyrics(&meta, state, debug_log, providers, Some(&service)).await;
        send_update(state, update_tx, true).await;
        return true;
    }
    false
}

async fn handle_position_sync(state: &mut StateBundle) -> bool {
    let position = state.player_state.estimate_position();
    state.player_state.set_position(position);
    state.update_index(position)
}

pub async fn handle_poll(
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    debug_log: bool,
    latest_meta: &mut Option<(TrackMetadata, f64, String)>,
    providers: &[String],
) {
    let mut should_send_update = false;

    if handle_latest_meta_update(state, latest_meta, debug_log, providers, update_tx).await {
        should_send_update = true;
    }

    if state.player_state.playing && handle_position_sync(state).await {
        should_send_update = true;
    }

    if state.player_state.err.is_some() {
        should_send_update = true;
    }

    if should_send_update {
        send_update(state, update_tx, false).await;
    }
}
