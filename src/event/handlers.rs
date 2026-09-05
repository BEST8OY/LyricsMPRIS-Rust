//! MPRIS event handlers for state transitions, track changes, and seeks.

use super::fetch::{FetchSummary, fetch_and_update_lyrics};
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
    pub(crate) event_tx: &'a mpsc::Sender<super::Event>,
    pub(crate) update_tx: &'a mpsc::Sender<Update>,
    pub(crate) providers: &'a [String],
}

/// Instructs the event loop on how to manage the network-retry timer
/// after processing an MPRIS event.
#[derive(Debug)]
pub enum RetryDirective {
    /// Leave any scheduled retry untouched (position updates, seeks)
    Keep,
    /// Cancel any scheduled retry (no player, stopped, shutdown)
    Clear,
    /// Schedule a retry for this track after providers were unreachable
    Schedule {
        meta: TrackMetadata,
        service: String,
    },
}

/// Handles MPRIS events (player updates and seeks).
///
/// Returns a [`RetryDirective`] so the caller can maintain the retry schedule.
pub(crate) async fn handle_mpris_event(
    event: MprisEvent,
    state: &mut StateBundle,
    event_tx: &mpsc::Sender<super::Event>,
    update_tx: &mpsc::Sender<Update>,
    providers: &[String],
) -> RetryDirective {
    let (meta, position, service, event_status, sampled_at, is_full_update) = match event {
        MprisEvent::PlayerUpdate(m, p, s, status, sampled_at) => {
            (m, p, s, status, sampled_at, true)
        }
        MprisEvent::Seeked(m, p, s, sampled_at) => (m, p, s, None, sampled_at, false),
        MprisEvent::UpgradeLyrics {
            meta,
            lines,
            format,
        } => {
            if state.player_state.title == meta.title && state.player_state.artist == meta.artist {
                let provider = super::fetch::db_format_to_provider(format);
                state.update_lyrics(lines, &meta, None, Some(provider));
                let current_pos = state.player_state.estimate_position();
                state.update_index(current_pos);
                send_update(state, update_tx, false).await;
                tracing::info!(
                    title = %meta.title,
                    artist = %meta.artist,
                    "Hot-swapped lyrics to Richsync karaoke"
                );
            }
            return RetryDirective::Keep;
        }
        MprisEvent::Calibrated {
            position,
            sampled_at,
        } => {
            if !state.player_state.playing {
                return RetryDirective::Keep;
            }

            // Discard calibration events that were captured before the current track's lyrics were loaded
            if let Some(loaded_at) = state.lyrics_loaded_at
                && sampled_at < loaded_at
            {
                tracing::debug!(
                    position,
                    "Discarding calibration event sampled before lyrics were loaded"
                );
                return RetryDirective::Keep;
            }

            // Compensate for any time the event spent waiting in the channel queue (capped at 2.0s)
            let queue_latency = sampled_at.elapsed().as_secs_f64().min(2.0);
            let effective_pos = position + queue_latency;

            handle_state_update(effective_pos, None, state, update_tx).await;
            return RetryDirective::Keep;
        }
    };

    // No active player: clear state and notify UI
    if service.is_empty() {
        handle_no_player(state, update_tx).await;
        return RetryDirective::Clear;
    }

    // Prioritize playback status provided in event payload; fallback to live D-Bus query only if absent
    let playback_status = if let Some(status) = event_status {
        Some(status)
    } else if is_full_update {
        Some(crate::mpris::get_playback_status(&service).await)
    } else {
        None
    };

    // If the player reported 'Stopped' on a full update, treat as no player
    if is_full_update && playback_status.as_deref() == Some("Stopped") {
        handle_no_player(state, update_tx).await;
        return RetryDirective::Clear;
    }

    // New track detection on full updates
    if is_full_update && state.player_state.has_changed(&meta) {
        return handle_new_track(NewTrackContext {
            meta,
            position,
            service,
            playback_status,
            state,
            event_tx,
            update_tx,
            providers,
        })
        .await;
    }

    // Discard stale position updates for the current track sampled before its lyrics were loaded,
    // provided playback status has not changed.
    if is_full_update
        && let Some(loaded_at) = state.lyrics_loaded_at
        && sampled_at < loaded_at
    {
        let status_changed = playback_status
            .as_deref()
            .map(|s| (s == "Playing") != state.player_state.playing)
            .unwrap_or(false);

        if !status_changed {
            tracing::debug!(
                position,
                "Discarding stale MPRIS event sampled before lyrics were loaded"
            );
            return RetryDirective::Keep;
        }
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
                return RetryDirective::Keep;
            }
        }

        // Legitimate seek event - update position immediately with queue latency compensation (if playing)
        let playing = state.player_state.playing;
        let queue_latency = if playing {
            sampled_at.elapsed().as_secs_f64().min(2.0)
        } else {
            0.0
        };
        let effective_position = position + queue_latency;
        state.update_playback_and_position(playing, effective_position);
        send_update(state, update_tx, true).await;
        return RetryDirective::Keep;
    }

    // Position/playback state update (for full updates) with queue latency compensation (if playing)
    let playing = playback_status
        .as_deref()
        .map(|s| s == "Playing")
        .unwrap_or(state.player_state.playing);
    let queue_latency = if playing {
        sampled_at.elapsed().as_secs_f64().min(2.0)
    } else {
        0.0
    };
    let effective_position = position + queue_latency;
    handle_state_update(effective_position, playback_status, state, update_tx).await;
    RetryDirective::Keep
}

/// Clears state when no player is active.
pub async fn handle_no_player(state: &mut StateBundle, update_tx: &mpsc::Sender<Update>) {
    state.clear_lyrics();
    state.player_state = Default::default();
    send_update(state, update_tx, true).await;
}

/// Handles detection of a new track.
///
/// Returns a [`RetryDirective`]: [`RetryDirective::Schedule`] when providers
/// were unreachable so the event loop can retry in the background.
pub(crate) async fn handle_new_track(ctx: NewTrackContext<'_>) -> RetryDirective {
    let NewTrackContext {
        meta,
        position: event_position,
        service,
        playback_status,
        state,
        event_tx,
        update_tx,
        providers,
    } = ctx;

    state.clear_lyrics();
    state.player_state.update_from_metadata(&meta);

    // If starting mid-track (> 0.5s), lock to actual playback position immediately;
    // otherwise initialize to 0.0 for start-of-track buffering alignment.
    let initial_pos = if event_position > 0.5 {
        event_position
    } else {
        0.0
    };
    state.player_state.set_position(initial_pos);

    if let Some(status) = playback_status {
        let playing = status == "Playing";
        if playing {
            state.player_state.start_playing();
        } else {
            state.player_state.pause();
        }
    }

    send_update(state, update_tx, true).await;

    let (_, summary) =
        fetch_and_update_lyrics(&meta, state, providers, Some(&service), event_tx).await;

    send_update(state, update_tx, true).await;

    match summary {
        FetchSummary::Unavailable => RetryDirective::Schedule { meta, service },
        FetchSummary::Loaded | FetchSummary::Exhausted => RetryDirective::Clear,
    }
}

/// Handles position and playback state updates.
pub(crate) async fn handle_state_update(
    position: f64,
    playback_status: Option<String>,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
) {
    let playing = playback_status
        .as_deref()
        .map(|s| s == "Playing")
        .unwrap_or(state.player_state.playing);

    // Guard: ignore 0.0s (or near-zero) position reports while playback is already active past 1.0s,
    // whether playing or paused, avoiding retrograde desyncs from stale buffering events,
    // UNLESS the track has reached the end and looped back to the beginning.
    let current_pos = state.player_state.estimate_position();
    let is_track_loop = state
        .player_state
        .length
        .is_some_and(|len| current_pos >= (len - 3.0).max(1.0))
        || state
            .lyric_state
            .lines
            .last()
            .is_some_and(|last| current_pos >= last.time && current_pos > 3.0);

    let effective_position = if position <= 0.05 && !is_track_loop {
        if current_pos > 1.0 {
            tracing::debug!(
                estimated = current_pos,
                incoming = position,
                "Ignoring 0.0s position report while track is already active at {current_pos}s"
            );
            current_pos
        } else {
            position
        }
    } else {
        position
    };

    let (playing_changed, changed_index, position_jumped) =
        state.update_playback_and_position(playing, effective_position);

    if playing_changed || changed_index || position_jumped {
        send_update(state, update_tx, playing_changed || position_jumped).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_state_update_seek_jump() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let lines = vec![
            crate::lyrics::LyricLine::new(10.0, "Line 1"),
            crate::lyrics::LyricLine::new(50.0, "Line 2"),
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

    #[tokio::test]
    async fn test_handle_calibrated_event() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let lines = vec![
            crate::lyrics::LyricLine::new(5.0, "Line 1"),
            crate::lyrics::LyricLine::new(15.0, "Line 2"),
        ];
        state.lyric_state.update_lines(lines);
        state.player_state.update_playback_dbus(true, 0.0);
        state.update_index(0.0);
        state.mark_state_sent();

        let (event_tx, _event_rx) = mpsc::channel(32);
        // Calibration moves position to 6.0s (Line 1 active)
        let directive = handle_mpris_event(
            MprisEvent::Calibrated {
                position: 6.0,
                sampled_at: std::time::Instant::now(),
            },
            &mut state,
            &event_tx,
            &tx,
            &[],
        )
        .await;

        assert!(matches!(directive, RetryDirective::Keep));
        let update = rx.recv().await.expect("Calibration update should be sent");
        assert_eq!(update.index, Some(0));
        assert!((update.position - 6.0).abs() < 0.1);
    }

    #[tokio::test]
    async fn test_handle_calibrated_event_guards_spurious_zero_reset() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let lines = vec![
            crate::lyrics::LyricLine::new(10.0, "Line 1"),
            crate::lyrics::LyricLine::new(50.0, "Line 2"),
        ];
        state.lyric_state.update_lines(lines);
        state.player_state.length = Some(180.0);
        state.player_state.update_playback_dbus(true, 45.0);
        state.update_index(45.0);
        state.mark_state_sent();

        let (event_tx, _event_rx) = mpsc::channel(32);
        // Spurious calibration tick reporting 0.0s during mid-track playback
        let directive = handle_mpris_event(
            MprisEvent::Calibrated {
                position: 0.0,
                sampled_at: std::time::Instant::now(),
            },
            &mut state,
            &event_tx,
            &tx,
            &[],
        )
        .await;

        assert!(matches!(directive, RetryDirective::Keep));
        // Position must NOT reset to 0.0s
        assert!(state.player_state.estimate_position() >= 45.0);
        // No retrograde position update should be emitted
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_pause_retains_position_when_incoming_is_zero() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let lines = vec![
            crate::lyrics::LyricLine::new(10.0, "Line 1"),
            crate::lyrics::LyricLine::new(50.0, "Line 2"),
        ];
        state.lyric_state.update_lines(lines);
        state.player_state.update_playback_dbus(true, 45.0);
        state.update_index(45.0);
        state.mark_state_sent();

        // Pause occurs, but player erroneously reports position 0.0
        handle_state_update(0.0, Some("Paused".to_string()), &mut state, &tx).await;

        let update = rx.recv().await.expect("Pause update should be sent");
        assert!(!update.playing);
        // Should NOT reset to 0.0; should retain ~45.0s and active line 0
        assert_eq!(update.index, Some(0));
        assert!((update.position - 45.0).abs() < 0.5);
    }

    #[tokio::test]
    async fn test_upgrade_lyrics_hotswap_current_track() {
        let (tx, mut rx) = mpsc::channel(32);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let mut meta = TrackMetadata::default();
        meta.title = "Current Song".into();
        meta.artist = "Current Artist".into();
        state.player_state.update_from_metadata(&meta);
        state.player_state.update_playback_dbus(true, 12.0);

        let initial_lines = vec![
            crate::lyrics::LyricLine::new(5.0, "Old line 1"),
            crate::lyrics::LyricLine::new(15.0, "Old line 2"),
        ];
        state.lyric_state.update_lines(initial_lines);
        state.update_index(12.0);
        state.mark_state_sent();

        // An UpgradeLyrics event arrives for the currently playing track
        let rich_word = crate::lyrics::types::WordTiming {
            start: 10.0,
            end: 14.0,
            text: "Karaoke".into(),
            grapheme_boundaries: vec![0, 7],
            grapheme_times: vec![],
        };
        let upgraded_lines = vec![
            crate::lyrics::LyricLine::new(5.0, "Upgraded line 1"),
            crate::lyrics::LyricLine::new(10.0, "Karaoke line 2").with_words(Some(vec![rich_word])),
        ];

        let directive = handle_mpris_event(
            MprisEvent::UpgradeLyrics {
                meta: meta.clone(),
                lines: upgraded_lines,
                format: crate::lyrics::database::LyricsFormat::Richsync,
            },
            &mut state,
            &event_tx,
            &tx,
            &[],
        )
        .await;

        assert!(matches!(directive, RetryDirective::Keep));
        let update = rx.recv().await.expect("Hot-swap update should be sent");
        assert_eq!(
            update.provider,
            Some(crate::state::Provider::MusixmatchRichsync)
        );
        assert_eq!(state.lyric_state.lines.len(), 2);
        assert_eq!(state.lyric_state.lines[1].text, "Karaoke line 2");
        assert!(state.lyric_state.lines[1].words.is_some());
    }

    #[tokio::test]
    async fn test_upgrade_lyrics_ignores_stale_track() {
        let (tx, mut rx) = mpsc::channel(32);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        // Player is now on Track B
        let mut current_meta = TrackMetadata::default();
        current_meta.title = "New Song".into();
        current_meta.artist = "New Artist".into();
        state.player_state.update_from_metadata(&current_meta);
        let initial_lines = vec![crate::lyrics::LyricLine::new(0.0, "New track line")];
        state.lyric_state.update_lines(initial_lines);
        state.mark_state_sent();

        // Stale upgrade event arrives for Track A
        let mut old_meta = TrackMetadata::default();
        old_meta.title = "Old Song".into();
        old_meta.artist = "Old Artist".into();
        let old_lines = vec![crate::lyrics::LyricLine::new(0.0, "Stale line")];

        let directive = handle_mpris_event(
            MprisEvent::UpgradeLyrics {
                meta: old_meta,
                lines: old_lines,
                format: crate::lyrics::database::LyricsFormat::Richsync,
            },
            &mut state,
            &event_tx,
            &tx,
            &[],
        )
        .await;

        assert!(matches!(directive, RetryDirective::Keep));
        // No update sent because track didn't match
        assert!(rx.try_recv().is_err());
        assert_eq!(state.lyric_state.lines[0].text, "New track line");
    }

    #[tokio::test]
    async fn test_discard_stale_player_update_sampled_before_lyrics_loaded() {
        let (tx, mut rx) = mpsc::channel(32);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let mut meta = TrackMetadata::default();
        meta.title = "Song".into();
        meta.artist = "Artist".into();
        state.player_state.update_from_metadata(&meta);
        state.player_state.update_playback_dbus(true, 18.0);

        let lines = vec![
            crate::lyrics::LyricLine::new(5.0, "Line 1"),
            crate::lyrics::LyricLine::new(15.0, "Line 2"),
        ];
        state.update_lyrics(lines, &meta, None, None);
        // Lyrics loaded at current instant
        let load_time = std::time::Instant::now();
        state.lyrics_loaded_at = Some(load_time);
        state.mark_state_sent();

        // Stale PlayerUpdate sampled 200ms BEFORE lyrics were loaded
        let stale_sample = load_time - std::time::Duration::from_millis(200);
        let directive = handle_mpris_event(
            MprisEvent::PlayerUpdate(
                meta.clone(),
                0.0, // Stale 0.0s position from track start
                "org.mpris.MediaPlayer2.spotify".into(),
                Some("Playing".into()),
                stale_sample,
            ),
            &mut state,
            &event_tx,
            &tx,
            &[],
        )
        .await;

        assert!(matches!(directive, RetryDirective::Keep));
        // Stale update must be discarded; no update sent to UI, position remains active
        assert!(rx.try_recv().is_err());
        assert!(state.player_state.estimate_position() >= 17.5);
    }

    #[tokio::test]
    async fn test_playing_ignores_zero_position_reset() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let lines = vec![
            crate::lyrics::LyricLine::new(10.0, "Line 1"),
            crate::lyrics::LyricLine::new(20.0, "Line 2"),
        ];
        state.lyric_state.update_lines(lines);
        state.player_state.update_playback_dbus(true, 15.0);
        state.update_index(15.0);
        state.mark_state_sent();

        // While playing at 15.0s, an erroneous 0.0s position update arrives
        handle_state_update(0.0, Some("Playing".to_string()), &mut state, &tx).await;

        // Should ignore 0.0s and retain ~15.0s
        assert!(state.player_state.estimate_position() >= 14.5);
        if let Ok(update) = rx.try_recv() {
            assert!(update.position >= 14.5);
            assert_eq!(update.index, Some(0));
        }
    }

    #[tokio::test]
    async fn test_track_change_not_discarded_even_if_sampled_before_lyrics_loaded() {
        let (tx, _rx) = mpsc::channel(32);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let mut old_meta = TrackMetadata::default();
        old_meta.title = "Old Song".into();
        old_meta.artist = "Artist".into();
        state.player_state.update_from_metadata(&old_meta);

        let lines = vec![crate::lyrics::LyricLine::new(5.0, "Old Line")];
        state.update_lyrics(lines, &old_meta, None, None);
        let load_time = std::time::Instant::now();
        state.lyrics_loaded_at = Some(load_time);

        // Track B arrives, but was sampled 200ms BEFORE Old Song's lyrics were loaded
        let mut new_meta = TrackMetadata::default();
        new_meta.title = "New Song".into();
        new_meta.artist = "Artist".into();
        let stale_sample = load_time - std::time::Duration::from_millis(200);

        let directive = handle_mpris_event(
            MprisEvent::PlayerUpdate(
                new_meta,
                0.0,
                "org.mpris.MediaPlayer2.spotify".into(),
                Some("Playing".into()),
                stale_sample,
            ),
            &mut state,
            &event_tx,
            &tx,
            &[],
        )
        .await;

        // New track must NOT be discarded; title must change
        assert_eq!(state.player_state.title, "New Song");
        assert!(matches!(directive, RetryDirective::Clear));
    }

    #[tokio::test]
    async fn test_pause_status_not_discarded_even_if_sampled_before_lyrics_loaded() {
        let (tx, mut rx) = mpsc::channel(32);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let mut meta = TrackMetadata::default();
        meta.title = "Song".into();
        meta.artist = "Artist".into();
        state.player_state.update_from_metadata(&meta);
        state.player_state.update_playback_dbus(true, 10.0);

        let lines = vec![crate::lyrics::LyricLine::new(5.0, "Line 1")];
        state.update_lyrics(lines, &meta, None, None);
        let load_time = std::time::Instant::now();
        state.lyrics_loaded_at = Some(load_time);
        state.mark_state_sent();

        // User paused while lyrics were loading
        let stale_sample = load_time - std::time::Duration::from_millis(150);
        let directive = handle_mpris_event(
            MprisEvent::PlayerUpdate(
                meta.clone(),
                10.0,
                "org.mpris.MediaPlayer2.spotify".into(),
                Some("Paused".into()),
                stale_sample,
            ),
            &mut state,
            &event_tx,
            &tx,
            &[],
        )
        .await;

        assert!(matches!(directive, RetryDirective::Keep));
        assert!(!state.player_state.playing);
        let update = rx.recv().await.expect("Pause update must be emitted");
        assert!(!update.playing);
    }

    #[tokio::test]
    async fn test_paused_state_does_not_add_queue_latency() {
        let (tx, mut rx) = mpsc::channel(32);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let mut meta = TrackMetadata::default();
        meta.title = "Song".into();
        meta.artist = "Artist".into();
        state.player_state.update_from_metadata(&meta);
        state.player_state.update_playback_dbus(false, 30.0);
        state.mark_state_sent();

        // Seek while paused, but event sat in queue for 500ms
        let sampled_at = std::time::Instant::now() - std::time::Duration::from_millis(500);
        handle_mpris_event(
            MprisEvent::Seeked(
                meta,
                30.0,
                "org.mpris.MediaPlayer2.spotify".into(),
                sampled_at,
            ),
            &mut state,
            &event_tx,
            &tx,
            &[],
        )
        .await;

        let update = rx.recv().await.expect("Seek update emitted");
        // Effective position must NOT have added the 500ms latency because player was paused
        assert_eq!(update.position, 30.0);
    }

    #[tokio::test]
    async fn test_track_loop_resets_to_beginning() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = StateBundle::new();

        let lines = vec![
            crate::lyrics::LyricLine::new(10.0, "Line 1"),
            crate::lyrics::LyricLine::new(50.0, "Line 2"),
        ];
        state.lyric_state.update_lines(lines);
        state.player_state.length = Some(60.0);
        state.player_state.update_playback_dbus(true, 58.0);
        state.update_index(58.0);
        state.mark_state_sent();

        // Track reached 58s of 60s, then loops back to 0.0s
        handle_state_update(0.0, Some("Playing".to_string()), &mut state, &tx).await;

        let update = rx.recv().await.expect("Loop update emitted");
        assert!(update.position < 0.5);
        assert_eq!(update.index, None);
    }
}
