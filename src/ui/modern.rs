use crate::pool;
use crate::state::Update;
use crate::ui::styles::LyricStyles;
use crossterm::{
    event::{Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use std::io::{self};
use std::time::{Duration, Instant};
use std::pin::Pin;
use tokio::time::Sleep;
use tokio::sync::mpsc;
use tui::{Terminal, backend::CrosstermBackend};

use crate::ui::modern_helpers::{estimate_update_and_next_sleep, draw_ui_with_cache};

/// UI state for the modern TUI mode
pub struct ModernUIState {
    pub last_update: Option<Update>,
    pub cached_lines: Option<Vec<String>>,
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
            cached_lines: None,
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
    poll_interval: Duration,
    mpris_config: crate::Config,
    karaoke_enabled: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::channel(32);
    let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
    tokio::spawn(pool::listen(tx, poll_interval, shutdown_rx, mpris_config.clone()));
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
    // use state.last_track_id for track-change detection; avoid redundant local copy
    while !state.should_exit {
        tokio::select! {
            biased;

            update = rx.recv() => {
                // Robust track change detection for TUI mode
                if let Some(ref upd) = update {
                    let track_id = crate::ui::track_id(upd);
                    if state.last_track_id.as_ref() != Some(&track_id) {
                        state.last_track_id = None;
                        state.last_track_id = Some(track_id);
                    }
                }
                process_update(update, &mut state)?;

                // After processing a new update, draw and (re)compute next per-word wakeup
                let (maybe_tmp, next) = estimate_update_and_next_sleep(&state.last_update, state.last_update_instant, state.karaoke_enabled);
                // Draw using the estimated update when available, otherwise fall back to the
                // stored `state.last_update`. This ensures UI is redrawn to clear content
                // (e.g. when cached_lines were cleared) even if no estimate exists.
                let draw_arg = if let Some(tmp) = maybe_tmp.clone() { Some(tmp) } else { state.last_update.clone() };
                let _ = draw_ui_with_cache(&mut terminal, &draw_arg, &state.cached_lines, &styles, state.karaoke_enabled);
                next_word_sleep = next;
            }

            maybe_event = tokio::task::spawn_blocking(|| crossterm::event::poll(std::time::Duration::from_millis(100))) => {
                if let Ok(Ok(true)) = maybe_event {
                    let event = crossterm::event::read().map_err(to_boxed_err)?;
                    process_event(event, &mut state)?;

                    // user-driven state changes (toggle karaoke, etc) may change scheduling
                    let (maybe_tmp, next) = estimate_update_and_next_sleep(&state.last_update, state.last_update_instant, state.karaoke_enabled);
                    let draw_arg = if let Some(tmp) = maybe_tmp.clone() { Some(tmp) } else { state.last_update.clone() };
                    let _ = draw_ui_with_cache(&mut terminal, &draw_arg, &state.cached_lines, &styles, state.karaoke_enabled);
                    next_word_sleep = next;
                }
            }

            // per-word timer branch: only present when scheduled for richsync karaoke
            _ = async {
                if let Some(s) = &mut next_word_sleep {
                    s.as_mut().await;
                } else {
                    futures_util::future::pending::<()>().await;
                }
            } => {
                // timer fired: redraw using estimated position and reschedule next boundary
                let (maybe_tmp, next) = estimate_update_and_next_sleep(&state.last_update, state.last_update_instant, state.karaoke_enabled);
                let draw_arg = if let Some(tmp) = maybe_tmp.clone() { Some(tmp) } else { state.last_update.clone() };
                let _ = draw_ui_with_cache(&mut terminal, &draw_arg, &state.cached_lines, &styles, state.karaoke_enabled);
                next_word_sleep = next;
            }
        }
    }
    disable_raw_mode().map_err(to_boxed_err)?;
    execute!(io::stdout(), LeaveAlternateScreen).map_err(to_boxed_err)?;
    Ok(())
}

/// Helper: Update cached lines and last update
fn update_cache_and_state(state: &mut ModernUIState, update: &Update) {
    state.cached_lines = Some(update.lines.iter().map(|l| l.text.clone()).collect());
    state.last_update = Some(update.clone());
    state.last_update_instant = Some(Instant::now());
}

// Scheduling helpers moved to `modern_helpers.rs` (estimate_update_and_next_sleep).

/// Encapsulates all logic for updating ModernUIState from an Update.
fn update_state(state: &mut ModernUIState, update: Option<Update>) {
    if let Some(update) = update {
        let track_id = crate::ui::track_id(&update);
        if update.lines.is_empty() && update.err.is_some() {
            if state.last_track_id.as_ref() != Some(&track_id) {
                state.cached_lines = None;
                state.last_update = None;
            }
            state.last_track_id = Some(track_id);
            return;
        }
        if update.lines.is_empty() && update.err.is_none() {
            state.cached_lines = None;
            state.last_update = None;
            state.last_track_id = Some(track_id);
            return;
        }
        if !update.lines.is_empty() {
            update_cache_and_state(state, &update);
        } else if let Some(ref mut last_upd) = state.last_update {
            last_upd.index = update.index;
            state.last_update_instant = Some(Instant::now());
        }
        state.last_track_id = Some(track_id);
    } else {
        state.should_exit = true;
    }
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