//! Event processing module for MPRIS player state changes.
//!
//! Handles all player events (track changes, seeks, playback state updates)
//! and coordinates lyrics fetching with state updates.

pub mod fetch;
pub mod handlers;

pub use fetch::fetch_and_update_lyrics;

use crate::mpris::TrackMetadata;
use crate::state::{StateBundle, Update};
use tokio::sync::mpsc;

/// Events originating from MPRIS player interface.
#[derive(Debug, Clone)]
pub enum MprisEvent {
    /// Full player state update with metadata, position, and service name.
    PlayerUpdate(TrackMetadata, f64, String),

    /// Seek event when user scrubs through track.
    Seeked(TrackMetadata, f64, String),
}

/// Top-level events processed by the main event loop.
#[derive(Debug)]
pub enum Event {
    /// MPRIS player event
    Mpris(MprisEvent),
    /// Shutdown signal (graceful termination)
    Shutdown,
}

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
pub async fn process_event(
    event: Event,
    state: &mut StateBundle,
    update_tx: &mpsc::Sender<Update>,
    providers: &[String],
) {
    match event {
        Event::Mpris(ev) => handlers::handle_mpris_event(ev, state, update_tx, providers).await,
        Event::Shutdown => send_update(state, update_tx, true).await,
    }
}
