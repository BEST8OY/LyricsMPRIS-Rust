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
use tokio::sync::watch;
// ...existing code...
use tui::{Terminal, backend::CrosstermBackend};

use crate::ui::modern_helpers::{estimate_update_and_next_sleep, draw_ui_with_cache};

/// UI state for the modern TUI mode
pub struct ModernUIState {
    pub last_update: Option<Update>,
    pub cached_lines: Option<Vec<String>>,
    /// Cached wrapped blocks at a given width: (width, wrapped_blocks)
    pub cached_wrapped: Option<(usize, Vec<Vec<String>>)>,
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
            cached_wrapped: None,
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
    // New behavior: run a synchronous UI bridge on the current (blocking) thread.
    // Spawn a small tokio runtime to drive the async `pool::listen` and forward Updates
    // over a std channel to this sync loop. This keeps terminal rendering purely
    // synchronous and avoids using tokio runtime threads for blocking terminal I/O.
    display_lyrics_modern_sync(_meta, _pos, poll_interval, mpris_config, karaoke_enabled).await
}

/// Synchronous UI bridge: runs on the calling thread, hosts the terminal render loop,
/// and receives the latest Updates from async tasks via a tokio::watch -> std bridge.
async fn display_lyrics_modern_sync(
    _meta: crate::mpris::TrackMetadata,
    _pos: f64,
    poll_interval: Duration,
    mpris_config: crate::Config,
    karaoke_enabled: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // std channel for sync UI loop
    let (std_tx, std_rx) = std::sync::mpsc::channel::<crate::state::Update>();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // watch channel holds the latest Update (Option<Update>) and starts as None
    let (watch_tx, mut watch_rx) = watch::channel::<Option<crate::state::Update>>(None);

    // Start the async pool listener, which will publish into `watch_tx` directly.
    let pool_handle = tokio::spawn(pool::listen(watch_tx.clone(), poll_interval, shutdown_rx, mpris_config.clone()));

    // Forward watch -> std (async task). This keeps the UI loop synchronous while
    // getting notifications of latest updates via watch.
    let forward_watch_to_std = tokio::spawn(async move {
        loop {
            if watch_rx.changed().await.is_err() {
                break;
            }
            if let Some(upd) = watch_rx.borrow().clone() {
                if std_tx.send(upd).is_err() {
                    break;
                }
            }
        }
    });

    // Now run the sync UI loop on this thread
    enable_raw_mode().map_err(to_boxed_err)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(to_boxed_err)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(to_boxed_err)?;
    let styles = LyricStyles::default();
    let mut state = ModernUIState::new();
    state.karaoke_enabled = karaoke_enabled;

    // per-word scheduling using an Instant; avoid allocating tokio Sleep in hot path
    let mut next_word_instant: Option<std::time::Instant> = None;

    // spawn blocking std thread to read crossterm events and forward via std::sync channel
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel::<crossterm::event::Event>(64);
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag_thread = stop_flag.clone();
    let event_thread = std::thread::spawn(move || {
        while !stop_flag_thread.load(std::sync::atomic::Ordering::Relaxed) {
            if crossterm::event::poll(std::time::Duration::from_millis(100)).unwrap_or(false) {
                if let Ok(ev) = crossterm::event::read() {
                    if event_tx.send(ev).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // initial draw
    let _ = draw_ui_with_cache(&mut terminal, state.last_update.as_ref(), state.cached_lines.as_deref(), &mut state.cached_wrapped, &styles, state.karaoke_enabled);

    while !state.should_exit {
        // Compute timeout until next_word_instant, clamp to a small value
        let timeout = next_word_instant.map(|i| i.saturating_duration_since(std::time::Instant::now())).unwrap_or(std::time::Duration::from_millis(500));

        // Wait for either an update from async bridge, an input event, or timeout
        if let Ok(upd) = std_rx.recv_timeout(timeout) {
            // Received update from async pool
            if state.last_track_id.as_ref() != Some(&crate::ui::track_id(&upd)) {
                state.last_track_id = Some(crate::ui::track_id(&upd));
            }
            process_update(Some(upd), &mut state)?;

            // Estimate and schedule next boundary (use Instant)
            let (maybe_tmp, next_sleep) = estimate_update_and_next_sleep(&state.last_update, state.last_update_instant, state.karaoke_enabled);
            next_word_instant = next_sleep.map(|d| std::time::Instant::now() + d);
            let draw_arg = maybe_tmp.as_ref().or_else(|| state.last_update.as_ref());
                    let _ = draw_ui_with_cache(&mut terminal, draw_arg, state.cached_lines.as_deref(), &mut state.cached_wrapped, &styles, state.karaoke_enabled);
            continue;
        }

        // Check input events without blocking longer than timeout
        if let Ok(ev) = event_rx.recv_timeout(std::time::Duration::from_millis(1)) {
            process_event(ev, &mut state)?;
            let (maybe_tmp, next_sleep) = estimate_update_and_next_sleep(&state.last_update, state.last_update_instant, state.karaoke_enabled);
            next_word_instant = next_sleep.map(|d| std::time::Instant::now() + d);
            let draw_arg = maybe_tmp.as_ref().or_else(|| state.last_update.as_ref());
            let _ = draw_ui_with_cache(&mut terminal, draw_arg, state.cached_lines.as_deref(), &mut state.cached_wrapped, &styles, state.karaoke_enabled);
            continue;
        }

        // Timeout fired: redraw if needed and reschedule
    let (maybe_tmp, next_sleep) = estimate_update_and_next_sleep(&state.last_update, state.last_update_instant, state.karaoke_enabled);
    next_word_instant = next_sleep.map(|d| std::time::Instant::now() + d);
        let draw_arg = maybe_tmp.as_ref().or_else(|| state.last_update.as_ref());
    let _ = draw_ui_with_cache(&mut terminal, draw_arg, state.cached_lines.as_deref(), &mut state.cached_wrapped, &styles, state.karaoke_enabled);
    }

    // shutdown
    stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = event_thread.join();
    let _ = shutdown_tx.send(());
    // wait for tasks to finish
    let _ = forward_watch_to_std.await;
    let _ = pool_handle.await;

    disable_raw_mode().map_err(to_boxed_err)?;
    execute!(io::stdout(), LeaveAlternateScreen).map_err(to_boxed_err)?;
    Ok(())
}

/// Helper: Update cached lines and last update
fn update_cache_and_state(state: &mut ModernUIState, update: &Update) {
    state.cached_lines = Some(update.lines.iter().map(|l| l.text.clone()).collect());
    state.last_update = Some(update.clone());
    state.last_update_instant = Some(Instant::now());
    // Invalidate wrapped cache; it will be recomputed with the current terminal width on next draw
    state.cached_wrapped = None;
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
                state.cached_wrapped = None;
            }
            state.last_track_id = Some(track_id);
            return;
        }
        if update.lines.is_empty() && update.err.is_none() {
            state.cached_lines = None;
            state.cached_wrapped = None;
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