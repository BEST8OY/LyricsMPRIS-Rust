//! Event processing module for MPRIS player state changes.
//!
//! Handles all player events (track changes, seeks, playback state updates)
//! and coordinates lyrics fetching with state updates.

pub mod fetch;
pub mod handlers;

pub use fetch::fetch_and_update_lyrics;
pub use handlers::{RetryDirective, handle_no_player};

use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update};
use tokio::sync::mpsc;

/// Top-level events processed by the main event loop.
#[derive(Debug, Clone)]
pub enum Event {
    /// Full player state update with metadata, position, service name, optional playback status, and sample timestamp.
    PlayerUpdate(
        TrackMetadata,
        f64,
        String,
        Option<String>,
        std::time::Instant,
    ),

    /// Seek event when user scrubs through track with sample timestamp.
    Seeked(TrackMetadata, f64, String, std::time::Instant),

    /// Position calibration event after transient buffering.
    Calibrated {
        meta: TrackMetadata,
        position: f64,
        sampled_at: std::time::Instant,
    },

    /// Background upgrade of lyrics format (e.g. line-level to Richsync karaoke).
    UpgradeLyrics {
        meta: TrackMetadata,
        lines: Vec<crate::lyrics::LyricLine>,
        format: crate::lyrics::database::LyricsFormat,
    },
}

/// Alias for backwards compatibility
pub type MprisEvent = Event;

/// Sends an update to the UI channel when appropriate.
pub async fn send_update(state: &mut StateBundle, update_tx: &mpsc::Sender<Update>, force: bool) {
    if !state.should_send_update(force) {
        return;
    }

    let update = state.create_update();

    if update_tx.send(update).await.is_ok() {
        state.mark_state_sent();
    }
}

/// Processes a single event from the event loop.
///
/// Returns the [`RetryDirective`] computed by the handler so callers can
/// maintain the network-retry schedule.
pub async fn process_event(
    event: Event,
    state: &mut StateBundle,
    event_tx: &mpsc::Sender<Event>,
    update_tx: &mpsc::Sender<Update>,
    providers: &[String],
) -> RetryDirective {
    handlers::handle_mpris_event(event, state, event_tx, update_tx, providers).await
}
