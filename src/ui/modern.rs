use crate::lyricsdb::LyricsDB;
use crate::pool;
use crate::state::Update;
use crate::state::Provider;
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
    position: f64,
    karaoke_enabled: bool,
) -> VisibleLines<'a> {
    let wrapped_blocks: Vec<Vec<String>> = cached.iter().map(|l| wrap_text(l, w)).collect();
    let current_block = &wrapped_blocks[update.index];
    let current_height = current_block.len();

    // For the current block, produce word-level spans with partial highlight based on position.
    let mut current = Vec::new();
    // Determine current and next timestamp
    let start_time = update.lines.get(update.index).map(|l| l.time).unwrap_or(0.0);
    let _end_time = update
        .lines
        .get(update.index + 1)
        .map(|l| l.time)
        .unwrap_or(start_time + 3.0);
    // progress not used when only per-word timings drive karaoke
    for line in current_block.iter() {
        // If provider supplied per-word timings, use them for precise karaoke highlighting.
        if let Some(ly) = update.lines.get(update.index) {
            // Only enable karaoke when the user enabled it and the provider is musixmatch.richsync or subtitles
            if karaoke_enabled && matches!(update.provider, Some(Provider::MusixmatchRichsync)) {
                if let Some(words) = &ly.words {
                let mut spans = Vec::new();
                for w in words {
                    if position >= w.end {
                        // fully past: fully highlighted
                        spans.push(Span::styled(format!("{} ", w.text), styles.current));
                        continue;
                    }
                    if position < w.start {
                        // not yet reached: render as after
                        spans.push(Span::styled(format!("{} ", w.text), styles.after));
                        continue;
                    }

                    // partially through this word: highlight progressively per character
                    let dur = (w.end - w.start).max(std::f64::EPSILON);
                    let frac = ((position - w.start) / dur).clamp(0.0, 1.0);
                    // split on unicode scalar (chars) boundaries; this is a pragmatic approach
                    let chars: Vec<char> = w.text.chars().collect();
                    let total = chars.len();
                    let highlight_chars = ((frac * total as f64).floor() as usize).min(total);

                    if highlight_chars == 0 {
                        spans.push(Span::styled(format!("{} ", w.text), styles.after));
                    } else if highlight_chars >= total {
                        spans.push(Span::styled(format!("{} ", w.text), styles.current));
                    } else {
                        let highlighted: String = chars[..highlight_chars].iter().collect();
                        let remaining: String = chars[highlight_chars..].iter().collect();
                        spans.push(Span::styled(highlighted, styles.current));
                        spans.push(Span::styled(format!("{} ", remaining), styles.after));
                    }
                }
                current.push(Spans::from(spans));
                continue;
                }
            }
        }

    // No per-word timings available (or karaoke disabled): render the current line fully highlighted
    current.push(Spans::from(Span::styled(line.clone(), styles.current)));
    }

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

    let before = collect_before_spans(
        update.index,
        &wrapped_blocks,
        lines_needed_before,
        styles.before,
    );
    let after = collect_after_spans(
        update.index,
        &wrapped_blocks,
        lines_needed_after,
        styles.after,
    );

    VisibleLines {
        before,
        current,
        after,
    }
}

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
