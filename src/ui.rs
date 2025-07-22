// ui.rs: Terminal UI for displaying lyrics in pipe and modern modes

use crate::state::Update;
use crate::pool;
use crossterm::{execute, terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen}, event::{Event, KeyCode}};
use std::io::{self};
use tui::{backend::CrosstermBackend, Terminal, widgets::Paragraph, text::{Span, Spans}, layout::{Alignment, Rect}};
use tokio::sync::{mpsc, Mutex};
use std::time::Duration;
use crate::lyricsdb::LyricsDB;
use std::sync::Arc;
// We no longer need pad_centered
use crate::text_utils::wrap_text;

/// UI state for the modern TUI mode
struct ModernUIState {
    last_update: Option<Update>,
    cached_lines: Option<Vec<String>>,
    last_track_id: Option<(String, String)>,
    should_exit: bool,
    paused_scroll_index: Option<usize>,
}

impl ModernUIState {
    fn new() -> Self {
        Self {
            last_update: None,
            cached_lines: None,
            last_track_id: None,
            should_exit: false,
            paused_scroll_index: None,
        }
    }
}

/// A collection of styled text lines (Spans) ready for rendering.
struct VisibleLines<'a> {
    before: Vec<Spans<'a>>,
    current: Vec<Spans<'a>>,
    after: Vec<Spans<'a>>,
}

impl<'a> VisibleLines<'a> {
    /// Combines before, current, and after spans into a single Vec for rendering.
    fn into_vec(self) -> Vec<Spans<'a>> {
        [self.before, self.current, self.after].concat()
    }
}


/// Display lyrics in pipe mode (stdout only, for scripting)
pub async fn display_lyrics_pipe(
    _meta: crate::mpris::TrackMetadata,
    _pos: f64,
    poll_interval: Duration,
    db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
    mpris_config: crate::Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::channel(32);
    let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
    tokio::spawn(pool::listen(tx, poll_interval, db.clone(), db_path.clone(), shutdown_rx, mpris_config.clone()));

    // State for track transitions and lyric printing
    let mut last_track_id: Option<(String, String)> = None;
    let mut last_track_had_lyric = false;
    let mut pending_newline = false;
    let mut last_line_idx = None;

    while let Some(upd) = rx.recv().await {
        // Track identity: use presence of lyrics and error string
        let track_id = (
            upd.lines.get(0).map(|_| "has_lyrics").unwrap_or("no_lyrics").to_string(),
            upd.err.clone().unwrap_or_default(),
        );

        // Detect track change
        let track_changed = last_track_id.as_ref() != Some(&track_id);
        if track_changed {
            // If previous track had lyrics, set flag to print newline after first update of new track
            pending_newline = last_track_id.is_some() && last_track_had_lyric;
            last_track_id = Some(track_id);
            last_line_idx = None;
            last_track_had_lyric = false;
            continue;
        }

        // On first update for new track, print newline if needed
        if pending_newline {
            if upd.lines.is_empty() {
                println!("");
            }
            pending_newline = false;
        }

        // Print lyric line if new lyric index
        if !upd.lines.is_empty() {
            if Some(upd.index) != last_line_idx {
                if let Some(line) = upd.lines.get(upd.index) {
                    println!("{}", line.text);
                    last_track_had_lyric = true;
                }
                last_line_idx = Some(upd.index);
            }
        }
    }
    Ok(())
}

/// Display lyrics in modern TUI mode (centered, highlighted, real-time)
pub async fn display_lyrics_modern(
    _meta: crate::mpris::TrackMetadata,
    _pos: f64,
    poll_interval: Duration,
    db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
    mpris_config: crate::Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::channel(32);
    let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
    tokio::spawn(pool::listen(tx, poll_interval, db.clone(), db_path.clone(), shutdown_rx, mpris_config.clone()));
    enable_raw_mode().map_err(to_boxed_err)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(to_boxed_err)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(to_boxed_err)?;
    let styles = LyricStyles::default();
    let mut state = ModernUIState::new();
    while !state.should_exit {
        tokio::select! {
            update = rx.recv() => {
                process_update(update, &mut state, &mut terminal, &styles)?;
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

/// Helper: Update paused scroll index
fn update_paused_scroll(state: &mut ModernUIState, update: &Update) {
    if !update.playing {
        if state.paused_scroll_index.is_none() {
            state.paused_scroll_index = Some(update.index);
        }
    } else {
        state.paused_scroll_index = None;
    }
}

/// Helper: Update cached lines and last update
fn update_cache_and_state(state: &mut ModernUIState, update: &Update) {
    state.cached_lines = Some(update.lines.iter().map(|l| l.text.clone()).collect());
    state.last_update = Some(update.clone());
}

/// Encapsulates all logic for updating ModernUIState from an Update.
fn update_state(state: &mut ModernUIState, update: Option<Update>) {
    if let Some(update) = update {
        let track_id = (
            update.lines.get(0).map(|_| "has_lyrics").unwrap_or("no_lyrics").to_string(),
            update.err.clone().unwrap_or_default(),
        );
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
            update_paused_scroll(state, &update);
        } else if let Some(ref mut last_upd) = state.last_update {
            last_upd.index = update.index;
            update_paused_scroll(state, &update);
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
) -> Vec<Spans<'a>> {
    if let Some(update) = last_update {
        if let Some(ref err) = update.err {
            // If there's an error, wrap it and prepare to render it.
            return wrap_text(err, w)
                .into_iter()
                .map(|line| Spans::from(Span::styled(line, styles.current)))
                .collect();
        } else if let Some(cached) = cached_lines {
            if !cached.is_empty() && update.index < cached.len() {
                return gather_visible_lines(update, cached, w, h, styles).into_vec();
            }
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
    draw_ui_with_cache(terminal, &state.last_update, &state.cached_lines, styles)?;
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
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                state.should_exit = true;
            },
            KeyCode::Up | KeyCode::Char('k') => {
                try_scroll_lyrics(state, -1);
            },
            KeyCode::Down | KeyCode::Char('j') => {
                try_scroll_lyrics(state, 1);
            },
            _ => {}
        }
    }
    // Only redraw if state changed (scroll or exit)
    if !state.should_exit {
        draw_ui_with_cache(terminal, &state.last_update, &state.cached_lines, styles)?;
    }
    Ok(())
}

/// Try to scroll the lyrics up or down by delta (if paused)
fn try_scroll_lyrics(state: &mut ModernUIState, delta: isize) {
    if let (Some(ref mut last_update), Some(ref lines)) = (state.last_update.as_mut(), state.cached_lines.as_ref()) {
        if let Some(idx) = state.paused_scroll_index.as_mut() {
            if !last_update.playing {
                let len = lines.len();
                let new_idx = (*idx as isize + delta).clamp(0, (len as isize).saturating_sub(1)) as usize;
                if new_idx != *idx {
                    *idx = new_idx;
                    last_update.index = *idx;
                }
            }
        }
    }
}

#[derive(Default)]
struct LyricStyles {
    before: tui::style::Style,
    current: tui::style::Style,
    after: tui::style::Style,
}

impl LyricStyles {
    fn default() -> Self {
        Self {
            before: tui::style::Style::default().add_modifier(tui::style::Modifier::ITALIC | tui::style::Modifier::DIM),
            current: tui::style::Style::default().fg(tui::style::Color::Green).add_modifier(tui::style::Modifier::BOLD),
            after: tui::style::Style::default(),
        }
    }
}

fn to_boxed_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(e)
}

/// Collects the styled lines that should appear *before* the current lyric.
fn collect_before_spans<'a>(
    current_index: usize,
    wrapped_blocks: &[Vec<String>],
    mut lines_needed: usize,
    style: tui::style::Style,
) -> Vec<Spans<'a>> {
    let mut before_spans = Vec::new();
    let mut i = current_index;
    while i > 0 && lines_needed > 0 {
        i -= 1;
        let block_to_take_from = &wrapped_blocks[i];
        let take = block_to_take_from.len().min(lines_needed);
        let start = block_to_take_from.len() - take;

        let spans = block_to_take_from[start..]
            .iter()
            .map(|line| Spans::from(Span::styled(line.clone(), style)));
        before_spans.splice(0..0, spans); // Prepend to maintain order
        lines_needed -= take;
    }
    before_spans
}

/// Collects the styled lines that should appear *after* the current lyric.
fn collect_after_spans<'a>(
    current_index: usize,
    wrapped_blocks: &[Vec<String>],
    mut lines_needed: usize,
    style: tui::style::Style,
) -> Vec<Spans<'a>> {
    let mut after_spans = Vec::new();
    let mut j = current_index + 1;
    while j < wrapped_blocks.len() && lines_needed > 0 {
        let block_to_take_from = &wrapped_blocks[j];
        let take = block_to_take_from.len().min(lines_needed);

        let spans = block_to_take_from[..take]
            .iter()
            .map(|line| Spans::from(Span::styled(line.clone(), style)));
        after_spans.extend(spans);
        lines_needed -= take;
        j += 1;
    }
    after_spans
}

/// Gathers all visible lines (before, current, after) based on the current state and screen size.
fn gather_visible_lines<'a>(
    update: &Update,
    cached: &[String],
    w: usize,
    h: usize,
    styles: &'a LyricStyles,
) -> VisibleLines<'a> {
    let wrapped_blocks: Vec<Vec<String>> = cached.iter().map(|l| wrap_text(l, w)).collect();
    let current_block = &wrapped_blocks[update.index];
    let current_height = current_block.len();

    let current = current_block
        .iter()
        .map(|line| Spans::from(Span::styled(line.clone(), styles.current)))
        .collect();

    if current_height >= h {
        // If the current line alone fills the screen, just show that.
        return VisibleLines {
            before: Vec::new(),
            current,
            after: Vec::new(),
        };
    }

    // Otherwise, calculate context lines to show before and after.
    let context_lines = h - current_height;
    let lines_needed_before = context_lines / 2;
    let lines_needed_after = context_lines - lines_needed_before;

    let before = collect_before_spans(update.index, &wrapped_blocks, lines_needed_before, styles.before);
    let after = collect_after_spans(update.index, &wrapped_blocks, lines_needed_after, styles.after);

    VisibleLines { before, current, after }
}


/// Renders the UI, now using the TUI Paragraph widget for centering.
fn draw_ui_with_cache<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    last_update: &Option<Update>,
    cached_lines: &Option<Vec<String>>,
    styles: &LyricStyles,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    terminal.draw(|f| {
        let size = f.size();
        let w = size.width as usize;
        let h = size.height as usize;
        let visible_spans = prepare_visible_spans(last_update, cached_lines, w, h, styles);

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
    })
    .map_err(to_boxed_err)?;
    Ok(())
}
