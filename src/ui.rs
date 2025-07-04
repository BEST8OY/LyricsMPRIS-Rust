// ui.rs: Terminal UI for displaying lyrics in pipe and modern modes

use crate::state::Update;
use crate::pool;
use crossterm::{execute, terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen}, event::{Event, KeyCode}};
use std::io::{self};
use tui::{backend::CrosstermBackend, Terminal, widgets::Paragraph, text::{Span, Spans}, layout::Alignment};
use tokio::sync::{mpsc, Mutex};
use std::time::Duration;
use crate::lyricsdb::LyricsDB;
use std::sync::Arc;
use crate::text_utils::{pad_centered, wrap_text};

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
    let mut last_line_idx = None;
    while let Some(upd) = rx.recv().await {
        if upd.lines.is_empty() && upd.err.is_some() {
            last_line_idx = None;
            continue;
        }
        if upd.err.is_some() { continue; }
        if Some(upd.index) != last_line_idx {
            if let Some(line) = upd.lines.get(upd.index) {
                println!("{}", line.text);
            }
            last_line_idx = Some(upd.index);
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

/// Helper: Handle error updates and UI
fn handle_update_error<B: tui::backend::Backend>(
    update: &Update,
    state: &mut ModernUIState,
    terminal: &mut Terminal<B>,
    styles: &LyricStyles,
    track_id: &(String, String),
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(ref upd_err) = update.err {
        if upd_err.contains("MprisError") || upd_err.contains("LyricsError") {
            draw_ui_with_cache(terminal, &None, &state.cached_lines, styles)?;
            state.last_track_id = Some(track_id.clone());
            return Ok(true);
        }
    }
    Ok(false)
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

/// Handle incoming update from the lyrics source
fn process_update<B: tui::backend::Backend>(
    update: Option<Update>,
    state: &mut ModernUIState,
    terminal: &mut Terminal<B>,
    styles: &LyricStyles,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            if handle_update_error(&update, state, terminal, styles, &track_id)? {
                return Ok(());
            }
            draw_ui_with_cache(terminal, &state.last_update, &state.cached_lines, styles)?;
            state.last_track_id = Some(track_id);
            return Ok(());
        }
        // Clear UI if no lyrics and no error (e.g. instrumental or missing lyrics)
        if update.lines.is_empty() && update.err.is_none() {
            state.cached_lines = None;
            state.last_update = None;
            draw_ui_with_cache(terminal, &state.last_update, &state.cached_lines, styles)?;
            state.last_track_id = Some(track_id);
            return Ok(());
        }
        if !update.lines.is_empty() {
            update_cache_and_state(state, &update);
            update_paused_scroll(state, &update);
        } else if let Some(ref mut last_upd) = state.last_update {
            last_upd.index = update.index;
            update_paused_scroll(state, &update);
        }
        draw_ui_with_cache(terminal, &state.last_update, &state.cached_lines, styles)?;
        state.last_track_id = Some(track_id);
    } else {
        state.should_exit = true;
    }
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

fn render_wrapped_centered_lines<'a>(
    lines: impl Iterator<Item = String>,
    width: usize,
    style: tui::style::Style,
) -> Vec<tui::text::Spans<'a>> {
    lines
        .flat_map(|l| {
            wrap_text(&l, width)
                .into_iter()
                .map(|wrapped| tui::text::Spans::from(tui::text::Span::styled(pad_centered(&wrapped, width), style)))
                .collect::<Vec<_>>()
        })
        .collect()
}

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
        let mut lines = Vec::new();
        if let Some(update) = last_update {
            if let Some(ref err) = update.err {
                let pad_top = h / 2;
                lines.extend((0..pad_top).map(|_| Spans::from(Span::raw(""))));
                lines.extend(render_wrapped_centered_lines(std::iter::once(err.clone()), w, styles.current));
            } else if let Some(cached) = cached_lines {
                if !cached.is_empty() && update.index < cached.len() {
                    let wrapped_lines: Vec<Vec<String>> = cached.iter().map(|l| wrap_text(l, w)).collect();
                    let visual_heights: Vec<usize> = wrapped_lines.iter().map(|v| v.len()).collect();
                    let current_block = &wrapped_lines[update.index];
                    let current_height = current_block.len();
                    let mut visible = Vec::new();
                    if current_height >= h {
                        visible.extend(render_wrapped_centered_lines(current_block.iter().cloned(), w, styles.current));
                        lines.extend(visible);
                    } else {
                        let context_lines = h - current_height;
                        let mut before = Vec::new();
                        let mut after = Vec::new();
                        let mut lines_needed_before = context_lines / 2;
                        let mut lines_needed_after = context_lines - lines_needed_before;
                        let mut i = update.index;
                        while i > 0 && lines_needed_before > 0 {
                            i -= 1;
                            let take = visual_heights[i].min(lines_needed_before);
                            let start = visual_heights[i] - take;
                            before.extend(render_wrapped_centered_lines(wrapped_lines[i][start..].iter().cloned(), w, styles.before));
                            lines_needed_before -= take;
                        }
                        before.reverse();
                        let mut j = update.index + 1;
                        while j < wrapped_lines.len() && lines_needed_after > 0 {
                            let take = visual_heights[j].min(lines_needed_after);
                            after.extend(render_wrapped_centered_lines(wrapped_lines[j][..take].iter().cloned(), w, styles.after));
                            lines_needed_after -= take;
                            j += 1;
                        }
                        visible.extend(before);
                        visible.extend(render_wrapped_centered_lines(current_block.iter().cloned(), w, styles.current));
                        visible.extend(after);
                        let pad_top = h.saturating_sub(visible.len()) / 2;
                        let pad_bottom = h.saturating_sub(visible.len() + pad_top);
                        lines.extend((0..pad_top).map(|_| Spans::from(Span::raw(""))));
                        lines.extend(visible);
                        lines.extend((0..pad_bottom).map(|_| Spans::from(Span::raw(""))));
                    }
                }
            }
        }
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left);
        f.render_widget(paragraph, size);
    }).map_err(to_boxed_err)?;
    Ok(())
}
