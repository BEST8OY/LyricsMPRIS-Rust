//! Pipe mode for streaming lyrics to stdout.
//!
//! This module implements a simple, scripting-friendly output mode that:
//! - Prints each lyric line as it becomes active
//! - Uses progressive timing to print lines even between MPRIS updates
//! - Handles track transitions cleanly
//! - Outputs plain text suitable for pipes and redirects

use crate::pool;
use crate::ui::estimate_update_and_next_sleep;
use std::pin::Pin;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::Sleep;

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

            if has_lyrics && upd.index.is_some() {
                self.print_current_line(&upd);
            }
        } else if has_lyrics && upd.index != self.last_line_idx {
            if upd.index.is_none() {
                // Seeked back before first lyric line: clear pipe consumer (e.g. Polybar, Waybar)
                println!();
                self.last_line_idx = None;
            } else {
                // Seeked to a valid lyric line (forward or backward): print current line immediately
                self.print_current_line(&upd);
            }
        }

        // Store update for local position estimation
        self.last_update = Some(upd);
        self.last_update_instant = Some(Instant::now());

        // Schedule next timer wakeup
        let (_, next) =
            estimate_update_and_next_sleep(&self.last_update, self.last_update_instant, true);
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
        let (maybe_estimated, next) =
            estimate_update_and_next_sleep(&self.last_update, self.last_update_instant, true);

        if let Some(estimated) = maybe_estimated {
            // Print if line index has advanced
            if estimated.index != self.last_line_idx {
                if let Some(idx) = estimated.index
                    && let Some(line) = estimated.lines.get(idx)
                {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_pipe_state_seek_before_first_line_and_middle_of_line() {
        let mut pipe_state = PipeState::new();

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

        let update_60s = crate::state::Update {
            lines: Arc::new(lines.clone()),
            index: Some(1),
            position: 60.0,
            playing: true,
            version: 1,
            err: None,
            provider: None,
            title: "Title".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
        };

        pipe_state.update_from_mpris(update_60s);
        assert_eq!(pipe_state.last_line_idx, Some(1));

        // Seek to 2s (before first line, index = None)
        let update_2s = crate::state::Update {
            lines: Arc::new(lines.clone()),
            index: None,
            position: 2.0,
            playing: true,
            version: 2,
            err: None,
            provider: None,
            title: "Title".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
        };

        pipe_state.update_from_mpris(update_2s);
        assert_eq!(pipe_state.last_line_idx, None);

        // Seek to 25s (middle of Line 1, index = Some(0))
        let update_25s = crate::state::Update {
            lines: Arc::new(lines),
            index: Some(0),
            position: 25.0,
            playing: true,
            version: 3,
            err: None,
            provider: None,
            title: "Title".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
        };

        pipe_state.update_from_mpris(update_25s);
        assert_eq!(pipe_state.last_line_idx, Some(0));
    }
}
