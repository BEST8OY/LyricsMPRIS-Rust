//! MPRIS event handlers for state transitions, track changes, and seeks.

use super::fetch::fetch_and_update_lyrics;
use super::{MprisEvent, send_update};
use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update};
use tokio::sync::mpsc;

/// Context for handling new track events.
pub(crate) struct NewTrackContext<'a> {
    pub(crate) meta: TrackMetadata,
    pub(crate) position: f64,
    pub(crate) service: String,
    pub(crate) playback_status: Option<String>,
    pub(crate) state: &'a mut StateBundle,
    pub(crate) update_tx: &'a mpsc::Sender<Update>,
    pub(crate) providers: &'a [String],
}

/// Handles MPRIS events (player updates and seeks).
pub(crate) async fn handle_mpris_event(
    event: MprisEvent,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    providers: &[String],
) {
    let (meta, position, service, is_full_update) = match event {
        MprisEvent::PlayerUpdate(m, p, s) => (m, p, s, true),
        MprisEvent::Seeked(m, p, s) => (m, p, s, false),
    };

    // No active player: clear state and notify UI
    if service.is_empty() {
        handle_no_player(state, update_tx).await;
        return;
    }

    // Only fetch playback status for full updates (optimization)
    let playback_status = if is_full_update {
        get_playback_status(&service).await
    } else {
        None
    };

    // If the player reported 'Stopped' on a full update, treat as no player
    if is_full_update && playback_status.as_deref() == Some("Stopped") {
        handle_no_player(state, update_tx).await;
        return;
    }

    // New track detection on full updates
    if is_full_update && state.player_state.has_changed(&meta) {
        handle_new_track(NewTrackContext {
            meta,
            position,
            service,
            playback_status,
            state,
            update_tx,
            providers,
        })
        .await;
        return;
    }

    // For seek events, filter initialization Seeked signals.
    if !is_full_update {
        if state.player_state.title == meta.title
            && state.player_state.artist == meta.artist
            && state.has_lyrics()
            && let Some(loaded_at) = state.lyrics_loaded_at
        {
            let elapsed = loaded_at.elapsed().as_secs_f64();
            let current_position = state.player_state.estimate_position();
            let position_delta = (position - current_position).abs();

            if elapsed < 0.5 && position_delta < 2.0 {
                tracing::debug!(
                    seek_position = position,
                    current_position,
                    position_delta,
                    time_since_load = elapsed,
                    "Ignoring initialization Seeked echo"
                );
                return;
            }
        }

        // Legitimate seek event - update position immediately
        let playing = state.player_state.playing;
        state.update_playback_and_position(playing, position);
        send_update(state, update_tx, true).await;
        return;
    }

    // Position/playback state update (for full updates)
    handle_state_update(position, playback_status, state, update_tx).await;
}

/// Clears state when no player is active.
pub(crate) async fn handle_no_player(state: &mut StateBundle, update_tx: &mpsc::Sender<Update>) {
    state.clear_lyrics();
    state.player_state = Default::default();
    send_update(state, update_tx, true).await;
}

/// Handles detection of a new track.
pub(crate) async fn handle_new_track(ctx: NewTrackContext<'_>) {
    let NewTrackContext {
        meta,
        position: _event_position, // Ignored - often stale from previous track
        service,
        playback_status,
        state,
        update_tx,
        providers,
    } = ctx;

    state.clear_lyrics();
    state.player_state.update_from_metadata(&meta);
    state.player_state.set_position(0.0);

    if let Some(status) = playback_status {
        let playing = status == "Playing";
        if playing {
            state.player_state.start_playing();
        } else {
            state.player_state.pause();
        }
    }

    send_update(state, update_tx, true).await;

    let _ = fetch_and_update_lyrics(&meta, state, providers, Some(&service)).await;

    send_update(state, update_tx, true).await;
}

/// Handles position and playback state updates.
pub(crate) async fn handle_state_update(
    position: f64,
    playback_status: Option<String>,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
) {
    let playing = playback_status
        .map(|s| s == "Playing")
        .unwrap_or(state.player_state.playing);

    let (playing_changed, changed_index, position_jumped) =
        state.update_playback_and_position(playing, position);

    if playing_changed || changed_index || position_jumped {
        send_update(state, update_tx, position_jumped).await;
    }
}

/// Fetches playback status from the player via D-Bus.
pub(crate) async fn get_playback_status(service: &str) -> Option<String> {
    crate::mpris::get_playback_status(service)
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_state_update_seek_jump() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let lines = vec![
            crate::lyrics::LyricLine {
                time: 10.0,
                text: "Line 1".to_string(),
                words: None,
            },
            crate::lyrics::LyricLine {
                time: 50.0,
                text: "Line 2".to_string(),
                words: None,
            },
        ];
        state.lyric_state.update_lines(lines);
        state.player_state.update_playback_dbus(true, 45.0);
        state.update_index(45.0);
        state.mark_state_sent();

        // Perform backward seek into middle of Line 1 (time 20.0s)
        handle_state_update(20.0, Some("Playing".to_string()), &mut state, &tx).await;

        let update = rx.recv().await.expect("Seek update should be received");
        assert_eq!(update.index, Some(0));
        assert!((update.position - 20.0).abs() < 0.5);
    }
}
