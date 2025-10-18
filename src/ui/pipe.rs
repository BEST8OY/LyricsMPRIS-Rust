//! Pipe mode for streaming lyrics to stdout.
//!
//! This module implements a simple, scripting-friendly output mode that:
//! - Prints each lyric line as it becomes active
//! - Uses progressive timing to print lines even between MPRIS updates
//! - Handles track transitions cleanly
//! - Outputs plain text suitable for pipes and redirects

use crate::pool;
use tokio::sync::mpsc;
use std::pin::Pin;
use tokio::time::Sleep;
use std::time::Instant;
use crate::ui::estimate_update_and_next_sleep;

/// State tracker for pipe mode output.
struct PipeState {
    /// Current track identifier (artist, title, album)
    last_track_id: Option<(String, String, String)>,
    /// Whether the last track had lyrics (for spacing)
    last_track_had_lyric: bool,
    /// Last printed line index
    last_line_idx: Option<usize>,
    /// Last received update for position estimation
    last_update: Option<crate::state::Update>,
    /// Time when last update was received
    last_update_instant: Option<Instant>,
    /// Scheduled timer for next line/word boundary
    next_sleep: Option<Pin<Box<Sleep>>>,
}

impl PipeState {
    fn new() -> Self {
        Self {
            last_track_id: None,
            last_track_had_lyric: false,
            last_line_idx: None,
            last_update: None,
            last_update_instant: None,
            next_sleep: None,
        }
    }

    /// Update state with a new update from MPRIS.
    fn update_from_mpris(&mut self, upd: crate::state::Update) {
        let track_id = crate::ui::track_id(&upd);
        let has_lyrics = !upd.lines.is_empty();
        let track_changed = self.last_track_id.as_ref() != Some(&track_id);

        if track_changed {
            self.handle_track_change();
            self.last_track_id = Some(track_id);

            // Don't print first line immediately - wait for it to become active
        } else if has_lyrics && upd.index != self.last_line_idx {
            self.print_current_line(&upd);
        }

        // Store update for local position estimation
        self.last_update = Some(upd);
        self.last_update_instant = Some(Instant::now());

        // Schedule next timer wakeup
        let (_, next) = estimate_update_and_next_sleep(
            &self.last_update,
            self.last_update_instant,
            true,
        );
        self.next_sleep = next;
    }

    /// Handle track change transition.
    fn handle_track_change(&mut self) {
        // Always print empty line for visual separation between tracks
        if self.last_track_id.is_some() {
            println!();
        }
        
        // Explicitly clear old update to free memory
        self.last_update = None;
        self.last_line_idx = None;
        self.last_track_had_lyric = false;
    }

    /// Print the current line from an update.
    fn print_current_line(&mut self, upd: &crate::state::Update) {
        if let Some(idx) = upd.index {
            if let Some(line) = upd.lines.get(idx) {
                println!("{}", line.text);
                self.last_track_had_lyric = true;
            }
            self.last_line_idx = Some(idx);
        }
    }

    /// Handle timer wakeup - estimate position and print new lines if changed.
    fn handle_timer_wakeup(&mut self) {
        let (maybe_estimated, next) = estimate_update_and_next_sleep(
            &self.last_update,
            self.last_update_instant,
            true,
        );

        if let Some(estimated) = maybe_estimated {
            // Print if line index has advanced
            if estimated.index != self.last_line_idx {
                if let Some(idx) = estimated.index
                    && let Some(line) = estimated.lines.get(idx) {
                        println!("{}", line.text);
                        self.last_track_had_lyric = true;
                    }
                self.last_line_idx = estimated.index;

                // Update stored update to the estimated one
                self.last_update = Some(estimated);
                self.last_update_instant = Some(Instant::now());
            }
        }

        self.next_sleep = next;
    }
}

/// Display lyrics in pipe mode (stdout only, for scripting).
pub async fn display_lyrics_pipe(
    _meta: crate::mpris::TrackMetadata,
    _pos: f64,
    mpris_config: crate::Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::channel(32);
    let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
    tokio::spawn(pool::listen(tx, shutdown_rx, mpris_config.clone()));

    let mut state = PipeState::new();

    loop {
        tokio::select! {
            // MPRIS lyrics/position updates
            maybe_upd = rx.recv() => {
                match maybe_upd {
                    Some(upd) => state.update_from_mpris(upd),
                    None => break, // Channel closed
                }
            }

            // Timer wakeup for progressive line printing
            _ = async {
                if let Some(s) = &mut state.next_sleep {
                    s.as_mut().await;
                } else {
                    futures_util::future::pending::<()>().await;
                }
            } => {
                state.handle_timer_wakeup();
            }
        }
    }

    Ok(())
}
