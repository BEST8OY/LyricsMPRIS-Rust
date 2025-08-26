use crate::lyricsdb::LyricsDB;
use crate::pool;
use crate::state::Update;
use crate::text_utils::wrap_text;
use crate::ui::styles::LyricStyles;
use crossterm::{
    event::{Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use std::io::{self};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};
use tui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Rect},
    text::{Span, Spans},
    widgets::Paragraph,
};

use crate::ui::modern_helpers::gather_visible_lines;

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

// VisibleLines and gather_visible_lines live in `modern_helpers` to keep this file small.

/// Display lyrics in modern TUI mode (centered, highlighted, real-time)
pub async fn display_lyrics_modern(
    _meta: crate::mpris::TrackMetadata,
    _pos: f64,
    poll_interval: Duration,
    db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
    mpris_config: crate::Config,
    karaoke_enabled: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::channel(32);
    let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
    tokio::spawn(pool::listen(
        tx,
        poll_interval,
        db.clone(),
        db_path.clone(),
        shutdown_rx,
        mpris_config.clone(),
    ));
    enable_raw_mode().map_err(to_boxed_err)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(to_boxed_err)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(to_boxed_err)?;
    let styles = LyricStyles::default();
    let mut state = ModernUIState::new();
    state.karaoke_enabled = karaoke_enabled;
    // use state.last_track_id for track-change detection; avoid redundant local copy
    // UI tick for smooth in-line progress updates (e.g. karaoke highlighting)
    let mut ui_tick = tokio::time::interval(Duration::from_millis(100));
    while !state.should_exit {
        tokio::select! {
            update = rx.recv() => {
                // Robust track change detection for TUI mode
                if let Some(ref upd) = update {
                    let track_id = crate::ui::track_id(upd);
                    if state.last_track_id.as_ref() != Some(&track_id) {
                        // Optionally, reset scroll or state here if needed
                        state.last_track_id = None;
                        // set new track id so update_state/draw see it immediately
                        state.last_track_id = Some(track_id);
                    }
                }
                process_update(update, &mut state, &mut terminal, &styles)?;
            }
        // Periodic UI tick: update the displayed position based on elapsed time
            _ = ui_tick.tick() => {
                // If we have a last update, use its position + elapsed (when playing)
                if let Some(ref last_upd) = state.last_update {
                    let mut displayed = last_upd.clone();
                    if displayed.playing {
                        if let Some(since) = state.last_update_instant {
                            displayed.position = displayed.position + since.elapsed().as_secs_f64();
                        }
                    }
            // Redraw using the estimated position
            draw_ui_with_cache(&mut terminal, &Some(displayed), &state.cached_lines, &styles, state.karaoke_enabled)?;
                }
            }
            maybe_event = tokio::task::spawn_blocking(|| crossterm::event::poll(std::time::Duration::from_millis(100))) => {
                if let Ok(Ok(true)) = maybe_event {
                    let event = crossterm::event::read().map_err(to_boxed_err)?;
                    process_event(event, &mut state, &mut terminal, &styles)?;
                }
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

/// Prepares the visible spans for rendering, given the state and styles.
fn prepare_visible_spans<'a>(
    last_update: &Option<Update>,
    cached_lines: &Option<Vec<String>>,
    w: usize,
    h: usize,
    styles: &'a LyricStyles,
    karaoke_enabled: bool,
) -> Vec<Spans<'a>> {
    if let Some(update) = last_update {
        if let Some(ref err) = update.err {
            // If there's an error, wrap it and prepare to render it.
            return wrap_text(err, w)
                .into_iter()
                .map(|line| Spans::from(Span::styled(line, styles.current)))
                .collect();
            } else if let Some(cached) = cached_lines
            && !cached.is_empty()
            && update.index < cached.len()
        {
            return gather_visible_lines(update, cached, w, h, styles, update.position, karaoke_enabled).into_vec();
        }
    }
    Vec::new()
}

/// Handle incoming update from the lyrics source (now simplified)
fn process_update<B: tui::backend::Backend>(
    update: Option<Update>,
    state: &mut ModernUIState,
    terminal: &mut Terminal<B>,
    styles: &LyricStyles,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    update_state(state, update);
    draw_ui_with_cache(terminal, &state.last_update, &state.cached_lines, styles, state.karaoke_enabled)?;
    Ok(())
}

/// Handle user input events (keyboard)
fn process_event<B: tui::backend::Backend>(
    event: Event,
    state: &mut ModernUIState,
    terminal: &mut Terminal<B>,
    styles: &LyricStyles,
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
    // Only redraw if state changed (scroll or exit)
    if !state.should_exit {
    draw_ui_with_cache(terminal, &state.last_update, &state.cached_lines, styles, state.karaoke_enabled)?;
    }
    Ok(())
}

fn to_boxed_err<E: std::error::Error + Send + Sync + 'static>(
    e: E,
) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(e)
}

// Helpers for wrapping and visible-line selection live in `modern_helpers`.

/// Renders the UI, now using the TUI Paragraph widget for centering.
fn draw_ui_with_cache<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    last_update: &Option<Update>,
    cached_lines: &Option<Vec<String>>,
    styles: &LyricStyles,
    karaoke_enabled: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    terminal
        .draw(|f| {
            let size = f.size();
            let w = size.width as usize;
            let h = size.height as usize;
            let visible_spans = prepare_visible_spans(last_update, cached_lines, w, h, styles, karaoke_enabled);

            if visible_spans.is_empty() {
                // Render an empty paragraph to clear the area and avoid zero-height rendering.
                let paragraph =
                    Paragraph::new(vec![Spans::from(Span::raw(""))]).alignment(Alignment::Center);
                f.render_widget(paragraph, size);
            } else {
                // Calculate vertical padding to center the entire block of text.
                let top_padding = h.saturating_sub(visible_spans.len()) / 2;
                let render_area = Rect {
                    x: size.x,
                    y: size.y + top_padding as u16,
                    width: size.width,
                    // Ensure height doesn't exceed the terminal boundary
                    height: (visible_spans.len() as u16).min(size.height),
                };

                // Create a Paragraph and let it handle the horizontal centering.
                let paragraph = Paragraph::new(visible_spans).alignment(Alignment::Center);
                f.render_widget(paragraph, render_area);
            }
        })
        .map_err(to_boxed_err)?;
    Ok(())
}
