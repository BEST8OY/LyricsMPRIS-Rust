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
    event::{Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use std::io::{self};
use std::time::Instant;
use std::pin::Pin;
use tokio::time::Sleep;
use tokio::sync::mpsc;
use std::thread;
use tui::{Terminal, backend::CrosstermBackend};

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
        }
    }
}

// Compute a line index from an Arc<Vec<LyricLine>> for a given position.
// Mirrors the binary-search logic used in `LyricState::get_index` but kept
// small here; VisibleLines and gather_visible_lines live in `modern_helpers`.

/// Display lyrics in modern TUI mode (centered, highlighted, real-time)
pub async fn display_lyrics_modern(
    _meta: crate::mpris::TrackMetadata,
    _pos: f64,
    mpris_config: crate::Config,
    karaoke_enabled: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::channel(32);
    let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
    tokio::spawn(pool::listen(tx, shutdown_rx, mpris_config.clone()));
    enable_raw_mode().map_err(to_boxed_err)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(to_boxed_err)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(to_boxed_err)?;
    let styles = LyricStyles::default();
    let mut state = ModernUIState::new();
    state.karaoke_enabled = karaoke_enabled;
    // per-word sleep used to schedule redraws only at interesting times (word boundaries)
    let mut next_word_sleep: Option<Pin<Box<Sleep>>> = None;
    // Single background thread to poll for crossterm events and forward them
    // to the async runtime via `event_rx`. This avoids repeatedly calling
    // `tokio::task::spawn_blocking` which grows the blocking threadpool when
    // the UI wakes frequently (e.g. karaoke mode).
    let (event_tx, mut event_rx) = mpsc::channel(32);
    // Spawn a real OS thread that polls and reads events synchronously.
    // Use try_send so the thread can exit when the receiver is closed.
    thread::spawn(move || {
        loop {
            // Poll with a short timeout to remain responsive.
            match crossterm::event::poll(std::time::Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(ev) => {
                        // If the async receiver is closed, stop the thread.
                        if event_tx.try_send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        // ignore and continue polling
                    }
                },
                Ok(false) => {
                    // timeout, continue
                }
                Err(_) => {
                    // on error, sleep a bit to avoid busy loop
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    });
    // Main event loop: handle updates, user input, and timer-driven redraws
    while !state.should_exit {
        tokio::select! {
            biased;

            // MPRIS lyrics/position updates
            update = rx.recv() => {
                process_update(update, &mut state)?;
                redraw_and_reschedule(&mut terminal, &mut state, &styles, &mut next_word_sleep)?;
            }

            // User keyboard input
            maybe_event = event_rx.recv() => {
                if let Some(event) = maybe_event {
                    process_event(event, &mut state)?;
                    redraw_and_reschedule(&mut terminal, &mut state, &styles, &mut next_word_sleep)?;
                } else {
                    // Event channel closed -> exit gracefully
                    state.should_exit = true;
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
                redraw_and_reschedule(&mut terminal, &mut state, &styles, &mut next_word_sleep)?;
            }
        }
    }
    disable_raw_mode().map_err(to_boxed_err)?;
    execute!(io::stdout(), LeaveAlternateScreen).map_err(to_boxed_err)?;
    Ok(())
}

/// Redraw the UI and reschedule the next timer wakeup.
/// 
/// Consolidates the repeated pattern of:
/// 1. Estimate current position based on elapsed time
/// 2. Draw UI with estimated/actual update
/// 3. Compute next word boundary for karaoke timer
fn redraw_and_reschedule<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &mut ModernUIState,
    styles: &LyricStyles,
    next_word_sleep: &mut Option<Pin<Box<Sleep>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (estimated_update, next_sleep) = crate::ui::estimate_update_and_next_sleep(
        &state.last_update,
        state.last_update_instant,
        state.karaoke_enabled,
    );

    // Use estimated update if available, otherwise fall back to stored update
    let draw_update = estimated_update.or_else(|| state.last_update.clone());

    crate::ui::modern_helpers::draw_ui_with_cache(
        terminal,
        &draw_update,
        &mut state.wrapped_cache,
        styles,
        state.karaoke_enabled,
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

    // Update with error message
    if update.lines.is_empty() && update.err.is_some() {
        if is_new_track {
            state.last_update = None;
        }
        state.last_track_id = Some(track_id);
        return;
    }

    // Empty update (no lyrics available)
    if update.lines.is_empty() {
        state.last_update = None;
        state.last_track_id = Some(track_id);
        return;
    }

    // Full update with lyrics
    if !update.lines.is_empty() {
        update_cache_and_state(state, &update);
        state.last_track_id = Some(track_id);
        return;
    }

    // Position-only update (shouldn't reach here based on above conditions)
    if let Some(ref mut last_upd) = state.last_update {
        last_upd.index = update.index;
        state.last_update_instant = Some(Instant::now());
    }
    state.last_track_id = Some(track_id);
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
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                state.should_exit = true;
            }
            KeyCode::Char('k') => {
                // Toggle karaoke at runtime
                state.karaoke_enabled = !state.karaoke_enabled;
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