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
    pub wrapped_cache: Option<(usize, Vec<crate::ui::modern_helpers::WrappedBlock>)>,
    pub last_track_id: Option<(String, String, String)>,
    pub should_exit: bool,
    /// Instant when the last Update was received; used to estimate current position
    pub last_update_instant: Option<Instant>,
    /// Runtime karaoke toggle (can be toggled with 'k')
    pub karaoke_enabled: bool,
    /// Whether parenthesized background vocals split onto separate sub-lines (`--no-split-bg`)
    pub split_background: bool,
    /// Manual scroll offset when paused (in lyric blocks, not wrapped lines)
    pub scroll_offset: isize,
    /// Optional command channel to trigger candidate cycling ('r')
    pub cmd_tx: Option<mpsc::Sender<crate::pool::UiCommand>>,
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
            split_background: true,
            scroll_offset: 0,
            cmd_tx: None,
        }
    }
}

impl Default for ModernUIState {
    fn default() -> Self {
        Self::new()
    }
}

// Compute a line index from an Arc<Vec<LyricLine>> for a given position.
// Mirrors the binary-search logic used in `LyricState::get_index` but kept
// small here; VisibleLines and gather_visible_lines live in `modern_helpers`.

/// Restores the terminal to cooked mode, leaves alternate screen, and ensures cursor is visible.
/// Safe to call multiple times, including from panic hooks and signal handlers.
pub fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Installs a panic hook that ensures the terminal is restored to cooked mode
/// before printing the panic message and backtrace.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore_terminal();
        default_hook(panic_info);
    }));
}

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
        restore_terminal();
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
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    tokio::spawn(pool::listen_with_commands(
        tx,
        shutdown_rx,
        Some(cmd_rx),
        mpris_config.clone(),
    ));
    // Restores the terminal on scope exit no matter how we leave the loop
    let _terminal_guard = TerminalGuard::new()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(to_boxed_err)?;
    let styles = LyricStyles::default();
    let mut state = ModernUIState::new();
    state.karaoke_enabled = karaoke_enabled;
    state.split_background = mpris_config.settings.filters.split_background;
    state.cmd_tx = Some(cmd_tx);
    // per-word sleep used to schedule redraws only at interesting times (word boundaries)
    let mut next_word_sleep: Option<Pin<Box<Sleep>>> = None;
    // Asynchronous crossterm event stream directly integrated into Tokio select loop.
    let mut event_stream = EventStream::new();

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    // Main event loop: handle updates, user input, and timer-driven redraws
    while !state.should_exit {
        tokio::select! {
            biased;

            // OS SIGTERM signal
            _ = async {
                #[cfg(unix)]
                {
                    sigterm.recv().await
                }
                #[cfg(not(unix))]
                {
                    futures_util::future::pending::<Option<()>>().await
                }
            } => {
                tracing::debug!("Termination signal received, exiting modern TUI");
                state.should_exit = true;
            }

            // OS SIGINT (Ctrl+C) signal
            _ = tokio::signal::ctrl_c() => {
                tracing::debug!("Ctrl+C signal received, exiting modern TUI");
                state.should_exit = true;
            }

            // MPRIS lyrics/position updates
            update = rx.recv() => {
                process_update(update, &mut state)?;
                if !state.should_exit {
                    redraw_and_reschedule(&mut terminal, &mut state, &styles, &mut next_word_sleep, max_visible_lines)?;
                }
            }

            // User keyboard input (async crossterm event stream)
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        let needs_redraw = process_event(event, &mut state)?;
                        if !state.should_exit && needs_redraw {
                            redraw_and_reschedule(&mut terminal, &mut state, &styles, &mut next_word_sleep, max_visible_lines)?;
                        }
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
        state.split_background,
        max_visible_lines,
        state.scroll_offset,
    )?;

    *next_word_sleep = next_sleep;
    Ok(())
}

/// Helper: Update cached lines and last update
fn update_cache_and_state(state: &mut ModernUIState, update: &Update) {
    // Invalidate wrapped cache only if lyrics lines actually changed
    let lyrics_changed = match &state.last_update {
        Some(prev) => !std::sync::Arc::ptr_eq(&prev.lines, &update.lines),
        None => true,
    };
    if lyrics_changed {
        state.wrapped_cache = None;
    }

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
    // error banner. If a new track has started and has an error, display the
    // error rather than stale lyrics from the previous track.
    if update.lines.is_empty() && update.err.is_some() {
        if state.last_update.is_none() || is_new_track {
            update_cache_and_state(state, &update);
        }
        return;
    }

    // Empty update (no lyrics available)
    if update.lines.is_empty() {
        state.last_update = None;
        state.wrapped_cache = None;
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

/// Handle user input events (keyboard, resize, etc.)
/// Returns `Ok(true)` if the event changed state and requires a UI redraw.
fn process_event(
    event: Event,
    state: &mut ModernUIState,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    match event {
        Event::Key(key) => {
            // Ignore release/repeat events reported by terminals with enhanced
            // keyboard protocols (e.g. kitty); only act on genuine presses.
            if key.kind != KeyEventKind::Press {
                return Ok(false);
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    state.should_exit = true;
                    Ok(false)
                }
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    state.should_exit = true;
                    Ok(false)
                }
                KeyCode::Char('k') => {
                    // Toggle karaoke at runtime
                    state.karaoke_enabled = !state.karaoke_enabled;
                    Ok(true)
                }
                KeyCode::Char('r') => {
                    // Cycle to the next candidate lyrics variant from the candidate queue
                    if let Some(ref tx) = state.cmd_tx {
                        let _ = tx.try_send(crate::pool::UiCommand::NextCandidate);
                    }
                    Ok(false)
                }
                KeyCode::Up => Ok(apply_paused_scroll(state, PausedScroll::Step(-1))),
                KeyCode::Down => Ok(apply_paused_scroll(state, PausedScroll::Step(1))),
                KeyCode::PageUp => Ok(apply_paused_scroll(state, PausedScroll::Step(-5))),
                KeyCode::PageDown => Ok(apply_paused_scroll(state, PausedScroll::Step(5))),
                KeyCode::Home => Ok(apply_paused_scroll(state, PausedScroll::Edge(false))),
                KeyCode::End => Ok(apply_paused_scroll(state, PausedScroll::Edge(true))),
                _ => Ok(false),
            }
        }
        Event::Resize(..) => {
            // Terminal was resized; redraw is needed
            Ok(true)
        }
        _ => Ok(false),
    }
}

enum PausedScroll {
    Step(isize),
    Edge(bool),
}

fn apply_paused_scroll(state: &mut ModernUIState, action: PausedScroll) -> bool {
    let Some(ref update) = state.last_update else {
        return false;
    };
    if update.playing || update.lines.is_empty() {
        return false;
    }
    let base_index = update.index.unwrap_or(0);
    let old_offset = state.scroll_offset;
    state.scroll_offset = match action {
        PausedScroll::Step(delta) => crate::ui::modern_helpers::step_scroll_offset(
            &update.lines,
            base_index,
            state.scroll_offset,
            delta,
        ),
        PausedScroll::Edge(to_end) => {
            crate::ui::modern_helpers::edge_scroll_offset(&update.lines, base_index, to_end)
        }
    };
    state.scroll_offset != old_offset
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
                    .map(|i| crate::lyrics::LyricLine::new(i as f64, format!("line {i}")))
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
            ..Default::default()
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

    #[test]
    fn test_wrapped_cache_preserved_when_lines_unchanged() {
        let mut state = ModernUIState::new();
        let update1 = sample_update("Song", 3, None);
        update_state(&mut state, Some(update1.clone()));

        // Simulate population of wrapped_cache
        state.wrapped_cache = Some((80, Vec::new()));

        // Position update arrives with identical lines Arc
        let mut update2 = update1.clone();
        update2.position = 5.0;
        update2.index = Some(1);
        update_state(&mut state, Some(update2));

        // Cache must NOT be cleared!
        assert!(state.wrapped_cache.is_some());

        // Now a new track arrives with different lines Arc
        let update3 = sample_update("Different Song", 3, None);
        update_state(&mut state, Some(update3));

        // Cache must be cleared for new lines
        assert!(state.wrapped_cache.is_none());
    }

    #[test]
    fn test_error_update_on_new_track_replaces_old_track_lyrics() {
        let mut state = ModernUIState::new();
        // Track A has lyrics displayed
        update_state(&mut state, Some(sample_update("Track A", 5, None)));
        assert_eq!(
            state.last_track_id,
            Some(("Artist".into(), "Track A".into(), "Album".into()))
        );
        assert!(
            state
                .last_update
                .as_ref()
                .is_some_and(|u| !u.lines.is_empty())
        );

        // Track B arrives with an error and 0 lines
        let err = "Lyrics not found for Track B".to_string();
        update_state(
            &mut state,
            Some(sample_update("Track B", 0, Some(err.clone()))),
        );

        // Track B's error MUST replace Track A's lyrics!
        assert_eq!(
            state.last_track_id,
            Some(("Artist".into(), "Track B".into(), "Album".into()))
        );
        assert_eq!(
            state.last_update.as_ref().and_then(|u| u.err.clone()),
            Some(err)
        );
    }

    #[test]
    fn test_empty_update_clears_wrapped_cache() {
        let mut state = ModernUIState::new();
        update_state(&mut state, Some(sample_update("Song", 3, None)));
        state.wrapped_cache = Some((80, Vec::new()));

        // Empty update arrives
        update_state(&mut state, Some(sample_update("Song", 0, None)));
        assert!(state.last_update.is_none());
        assert!(state.wrapped_cache.is_none());
    }

    #[test]
    fn test_process_event_scroll_bounds() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let key_event = |code: KeyCode| {
            Event::Key(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })
        };

        let mut state = ModernUIState::new();
        let mut update = sample_update("Track", 10, None);
        update.playing = false;
        update.index = Some(2); // base_index = 2, total = 10 (indices 0..9)
        state.last_update = Some(update);

        // Scroll up by 1: from offset 0 to -1 (effective index 2 - 1 = 1)
        let redraw = process_event(key_event(KeyCode::Up), &mut state).unwrap();
        assert!(redraw);
        assert_eq!(state.scroll_offset, -1);

        // Scroll up by 1: from offset -1 to -2 (effective index 2 - 2 = 0, top of lyrics)
        let redraw = process_event(key_event(KeyCode::Up), &mut state).unwrap();
        assert!(redraw);
        assert_eq!(state.scroll_offset, -2);

        // Scroll up past beginning: must be clamped at -2 (cannot exceed top bound)
        let redraw = process_event(key_event(KeyCode::Up), &mut state).unwrap();
        assert!(!redraw); // No state change -> no redraw needed!
        assert_eq!(state.scroll_offset, -2);

        // Immediate first Down key moves downward without dead zone!
        let redraw = process_event(key_event(KeyCode::Down), &mut state).unwrap();
        assert!(redraw);
        assert_eq!(state.scroll_offset, -1);

        // PageDown: scrolls down by 5 lines (clamped to max_offset = 10 - 1 - 2 = 7)
        let redraw = process_event(key_event(KeyCode::PageDown), &mut state).unwrap();
        assert!(redraw);
        assert_eq!(state.scroll_offset, 4);

        // End: jumps directly to the end of lyrics (offset = 7)
        let redraw = process_event(key_event(KeyCode::End), &mut state).unwrap();
        assert!(redraw);
        assert_eq!(state.scroll_offset, 7);

        // Down at end: clamped, no redraw
        let redraw = process_event(key_event(KeyCode::Down), &mut state).unwrap();
        assert!(!redraw);
        assert_eq!(state.scroll_offset, 7);

        // Home: jumps to beginning (offset = -2)
        let redraw = process_event(key_event(KeyCode::Home), &mut state).unwrap();
        assert!(redraw);
        assert_eq!(state.scroll_offset, -2);
    }

    #[test]
    fn test_modern_ui_state_default() {
        let state = ModernUIState::default();
        assert_eq!(state.scroll_offset, 0);
        assert!(state.karaoke_enabled);
        assert!(state.split_background);
        assert!(!state.should_exit);
    }

    #[test]
    fn test_paused_scroll_skips_is_bg_lines() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let key_event = |code: KeyCode| {
            Event::Key(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })
        };

        let mut state = ModernUIState::new();
        let lines = vec![
            crate::lyrics::LyricLine::new(0.0, "Lead 0"),
            crate::lyrics::LyricLine::new(1.0, "Echo 0").with_bg(true),
            crate::lyrics::LyricLine::new(5.0, "Lead 1"),
            crate::lyrics::LyricLine::new(6.0, "Echo 1").with_bg(true),
            crate::lyrics::LyricLine::new(10.0, "Lead 2"),
        ];
        state.last_update = Some(Update {
            lines: Arc::new(lines),
            index: Some(0),
            playing: false,
            ..Default::default()
        });

        assert!(process_event(key_event(KeyCode::Down), &mut state).unwrap());
        assert_eq!(state.scroll_offset, 2); // Lead 1

        assert!(process_event(key_event(KeyCode::End), &mut state).unwrap());
        assert_eq!(state.scroll_offset, 4); // Lead 2

        assert!(process_event(key_event(KeyCode::Up), &mut state).unwrap());
        assert_eq!(state.scroll_offset, 2); // Lead 1

        assert!(process_event(key_event(KeyCode::Home), &mut state).unwrap());
        assert_eq!(state.scroll_offset, 0); // Lead 0
    }
}
