//! Modern TUI mode for real-time synchronized lyrics display.
//!
//! This module implements a full-screen terminal user interface with:
//! - Centered, vertically aligned lyrics display
//! - Real-time position estimation between MPRIS updates
//! - Per-word karaoke highlighting for richsync lyrics
//! - Dynamic event-driven rendering
//!
//! The event loop uses `tokio::select!` to handle:
//! - Lyrics updates from MPRIS
//! - User keyboard input (q/ESC to quit, k to toggle karaoke)
//! - Per-word timer wakeups for smooth karaoke rendering

use crate::pool;
use crate::state::Update;
use crate::ui::styles::LyricStyles;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::pin::Pin;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::Sleep;

/// UI state for the modern TUI mode
pub struct ModernUIState {
    pub last_update: Option<Update>,
    /// Cached wrapped blocks for the current terminal width: (width, wrapped_blocks)
    pub wrapped_cache: Option<(usize, Vec<Vec<String>>)>,
    pub last_track_id: Option<(String, String, String)>,
    pub should_exit: bool,
    /// Instant when the last Update was received; used to estimate current position
    pub last_update_instant: Option<Instant>,
    /// Runtime karaoke toggle (can be toggled with 'k')
    pub karaoke_enabled: bool,
    /// Manual scroll offset when paused (in lyric blocks, not wrapped lines)
    pub scroll_offset: isize,
}

impl ModernUIState {
    pub fn new() -> Self {
        Self {
            last_update: None,
            wrapped_cache: None,
            last_track_id: None,
            should_exit: false,
            last_update_instant: None,
            karaoke_enabled: true,
            scroll_offset: 0,
        }
    }
}

// Compute a line index from an Arc<Vec<LyricLine>> for a given position.
// Mirrors the binary-search logic used in `LyricState::get_index` but kept
// small here; VisibleLines and gather_visible_lines live in `modern_helpers`.

/// RAII guard that puts the terminal into raw mode + alternate screen on
/// creation and always restores it on drop - including on error propagation
/// (`?`) or panic unwinding from anywhere in the TUI event loop.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        enable_raw_mode().map_err(to_boxed_err)?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(to_boxed_err(e));
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Display lyrics in modern TUI mode (centered, highlighted, real-time)
pub async fn display_lyrics_modern(
    mpris_config: crate::Config,
    karaoke_enabled: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let max_visible_lines = mpris_config.visible_lines;
    let (tx, mut rx) = mpsc::channel(32);
    let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
    tokio::spawn(pool::listen(tx, shutdown_rx, mpris_config.clone()));
    // Restores the terminal on scope exit no matter how we leave the loop
    let _terminal_guard = TerminalGuard::new()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(to_boxed_err)?;
    let styles = LyricStyles::default();
    let mut state = ModernUIState::new();
    state.karaoke_enabled = karaoke_enabled;
    // per-word sleep used to schedule redraws only at interesting times (word boundaries)
    let mut next_word_sleep: Option<Pin<Box<Sleep>>> = None;
    // Asynchronous crossterm event stream directly integrated into Tokio select loop.
    let mut event_stream = EventStream::new();

    // Main event loop: handle updates, user input, and timer-driven redraws
    while !state.should_exit {
        tokio::select! {
            biased;

            // MPRIS lyrics/position updates
            update = rx.recv() => {
                process_update(update, &mut state)?;
                redraw_and_reschedule(&mut terminal, &mut state, &styles, &mut next_word_sleep, max_visible_lines)?;
            }

            // User keyboard input (async crossterm event stream)
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        process_event(event, &mut state)?;
                        redraw_and_reschedule(&mut terminal, &mut state, &styles, &mut next_word_sleep, max_visible_lines)?;
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "Terminal event stream error");
                    }
                    None => {
                        // Event stream closed -> exit gracefully
                        state.should_exit = true;
                    }
                }
            }

            // Per-word timer for smooth karaoke rendering
            _ = async {
                if let Some(s) = &mut next_word_sleep {
                    s.as_mut().await;
                } else {
                    futures_util::future::pending::<()>().await;
                }
            } => {
                redraw_and_reschedule(&mut terminal, &mut state, &styles, &mut next_word_sleep, max_visible_lines)?;
            }
        }
    }
    // TerminalGuard::drop restores raw mode and leaves the alternate screen,
    // both on the happy path and on early error returns.
    Ok(())
}

/// Redraw the UI and reschedule the next timer wakeup.
///
/// Consolidates the repeated pattern of:
/// 1. Estimate current position based on elapsed time
/// 2. Draw UI with estimated/actual update
/// 3. Compute next word boundary for karaoke timer
fn redraw_and_reschedule<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &mut ModernUIState,
    styles: &LyricStyles,
    next_word_sleep: &mut Option<Pin<Box<Sleep>>>,
    max_visible_lines: Option<usize>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let (estimated_update, next_sleep) = crate::ui::estimate_update_and_next_sleep(
        &state.last_update,
        state.last_update_instant,
        state.karaoke_enabled,
    );

    // Use estimated update if available, otherwise fall back to stored update
    let draw_update = estimated_update.or_else(|| state.last_update.clone());

    // Reset scroll offset when playback resumes
    if let Some(ref upd) = draw_update
        && upd.playing
    {
        state.scroll_offset = 0;
    }

    crate::ui::modern_helpers::draw_ui_with_cache(
        terminal,
        &draw_update,
        &mut state.wrapped_cache,
        styles,
        state.karaoke_enabled,
        max_visible_lines,
        state.scroll_offset,
    )?;

    *next_word_sleep = next_sleep;
    Ok(())
}

/// Helper: Update cached lines and last update
fn update_cache_and_state(state: &mut ModernUIState, update: &Update) {
    // Explicitly clear old cache before creating new one to free memory immediately
    state.wrapped_cache = None;

    state.last_update = Some(update.clone());
    state.last_update_instant = Some(Instant::now());
}

/// Encapsulates all logic for updating ModernUIState from an Update.
///
/// Handles track changes, errors, and position-only updates intelligently.
fn update_state(state: &mut ModernUIState, update: Option<Update>) {
    let Some(update) = update else {
        // Channel closed - signal exit
        state.should_exit = true;
        return;
    };

    let track_id = crate::ui::track_id(&update);
    let is_new_track = state.last_track_id.as_ref() != Some(&track_id);

    if is_new_track {
        state.scroll_offset = 0;
        state.last_track_id = Some(track_id);
    }

    // Update with error message and no lyrics.
    //
    // If nothing is currently displayed (fresh track, startup), surface the
    // error so the screen is not silently blank. If lyrics from this track are
    // already showing, keep them rather than clobbering good content with an
    // error banner.
    if update.lines.is_empty() && update.err.is_some() {
        if state.last_update.is_none() {
            update_cache_and_state(state, &update);
        }
        return;
    }

    // Empty update (no lyrics available)
    if update.lines.is_empty() {
        state.last_update = None;
        return;
    }

    // Full update with lyrics
    update_cache_and_state(state, &update);
}

// prepare_visible_spans moved to `ui_helpers::draw_ui_with_cache`.

/// Handle incoming update from the lyrics source (now simplified)
fn process_update(
    update: Option<Update>,
    state: &mut ModernUIState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    update_state(state, update);
    Ok(())
}

/// Handle user input events (keyboard)
fn process_event(
    event: Event,
    state: &mut ModernUIState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Event::Key(key) = event {
        // Ignore release/repeat events reported by terminals with enhanced
        // keyboard protocols (e.g. kitty); only act on genuine presses.
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                state.should_exit = true;
            }
            KeyCode::Char('k') => {
                // Toggle karaoke at runtime
                state.karaoke_enabled = !state.karaoke_enabled;
            }
            KeyCode::Up => {
                // Scroll up when paused
                if let Some(ref update) = state.last_update
                    && !update.playing
                {
                    state.scroll_offset = state.scroll_offset.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                // Scroll down when paused
                if let Some(ref update) = state.last_update
                    && !update.playing
                {
                    state.scroll_offset = state.scroll_offset.saturating_add(1);
                }
            }
            KeyCode::Char('c')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                state.should_exit = true;
            }
            _ => {}
        }
    }
    Ok(())
}

fn to_boxed_err<E: std::error::Error + Send + Sync + 'static>(
    e: E,
) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(e)
}

// Helpers for wrapping and visible-line selection live in `modern_helpers`.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sample_update(title: &str, lines: usize, err: Option<String>) -> Update {
        Update {
            lines: Arc::new(
                (0..lines)
                    .map(|i| crate::lyrics::LyricLine {
                        time: i as f64,
                        text: format!("line {i}"),
                        words: None,
                    })
                    .collect(),
            ),
            index: (lines > 0).then_some(0),
            position: 0.0,
            playing: false,
            version: 1,
            err,
            artist: "Artist".into(),
            title: title.into(),
            album: "Album".into(),
            provider: None,
        }
    }

    #[test]
    fn test_error_update_displayed_when_nothing_shown() {
        let mut state = ModernUIState::new();
        let err = "Network unavailable - retrying...".to_string();
        update_state(&mut state, Some(sample_update("T", 0, Some(err.clone()))));

        assert_eq!(
            state.last_track_id,
            Some(("Artist".into(), "T".into(), "Album".into()))
        );
        assert_eq!(
            state.last_update.as_ref().and_then(|u| u.err.clone()),
            Some(err)
        );
    }

    #[test]
    fn test_error_update_does_not_clobber_displayed_lyrics() {
        let mut state = ModernUIState::new();
        update_state(&mut state, Some(sample_update("T", 3, None)));
        assert!(
            state
                .last_update
                .as_ref()
                .is_some_and(|u| !u.lines.is_empty())
        );

        update_state(
            &mut state,
            Some(sample_update("T", 0, Some("boom".to_string()))),
        );
        assert!(
            state
                .last_update
                .as_ref()
                .is_some_and(|u| !u.lines.is_empty())
        );
    }

    #[test]
    fn test_scroll_offset_reset_on_track_change() {
        let mut state = ModernUIState::new();
        update_state(&mut state, Some(sample_update("A", 3, None)));
        state.scroll_offset = -5;
        assert_eq!(state.scroll_offset, -5);

        update_state(&mut state, Some(sample_update("B", 3, None)));
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_successful_update_replaces_stored_error() {
        let mut state = ModernUIState::new();
        update_state(
            &mut state,
            Some(sample_update(
                "T",
                0,
                Some("Network unavailable - retrying...".into()),
            )),
        );
        update_state(&mut state, Some(sample_update("T", 4, None)));

        let last = state.last_update.expect("lyrics should be stored");
        assert_eq!(last.lines.len(), 4);
        assert!(last.err.is_none());
    }

    #[test]
    fn test_channel_close_sets_exit() {
        let mut state = ModernUIState::new();
        update_state(&mut state, None);
        assert!(state.should_exit);
    }
}
