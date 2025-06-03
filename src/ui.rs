// ui.rs: Terminal UI for displaying lyrics in pipe and modern modes

use crate::pool::Update;
use crossterm::{execute, terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen}, event::{Event, KeyCode}};
use std::io::{self};
use tui::{backend::CrosstermBackend, Terminal, widgets::{Paragraph}, text::{Span, Spans}, layout::{Alignment}};
use tokio::sync::mpsc;
use std::time::Duration;
use crate::lyricsdb::LyricsDB;
use std::sync::{Arc, Mutex};

fn pad_centered(text: &str, width: usize) -> String {
    let line_width = text.chars().count();
    let pad_left = if width > line_width { (width - line_width) / 2 } else { 0 };
    let mut content = String::with_capacity(width.max(line_width));
    for _ in 0..pad_left { content.push(' '); }
    content.push_str(text);
    content
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.chars().count() + word.chars().count() + 1 > width && !current.is_empty() {
            lines.push(current);
            current = String::new();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

pub async fn display_lyrics_pipe(_meta: crate::mpris::TrackMetadata, _pos: f64, poll_interval: Duration, db: Option<Arc<Mutex<LyricsDB>>>, db_path: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let (tx, mut rx) = mpsc::channel(32);
    tokio::spawn(crate::pool::listen(tx, poll_interval, db.clone(), db_path.clone()));
    let mut last_line_idx = None;
    while let Some(upd) = rx.recv().await {
        if upd.err.is_some() || upd.lines.is_empty() { continue; }
        if Some(upd.index) != last_line_idx {
            println!("{}", upd.lines[upd.index].text);
            last_line_idx = Some(upd.index);
        }
    }
    Ok(())
}

pub async fn display_lyrics_modern(_meta: crate::mpris::TrackMetadata, _pos: f64, poll_interval: Duration, db: Option<Arc<Mutex<LyricsDB>>>, db_path: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let (tx, mut rx) = mpsc::channel(32);
    tokio::spawn(crate::pool::listen(tx, poll_interval, db.clone(), db_path.clone()));
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut last_update: Option<Update> = None;
    let mut cached_lines: Option<Vec<String>> = None;
    let style_before = tui::style::Style::default().add_modifier(tui::style::Modifier::ITALIC | tui::style::Modifier::DIM);
    let style_current = tui::style::Style::default().fg(tui::style::Color::Green).add_modifier(tui::style::Modifier::BOLD);
    let style_after = tui::style::Style::default();
    loop {
        tokio::select! {
            update = rx.recv() => {
                if let Some(update) = update {
                    // If lines is empty, reuse cached lines
                    if !update.lines.is_empty() {
                        cached_lines = Some(update.lines.iter().map(|l| l.text.clone()).collect());
                        last_update = Some(update);
                    } else if let Some(ref mut upd) = last_update {
                        // Only index changed, update index but reuse lines
                        upd.index = update.index;
                    }
                    draw_ui_with_cache(&mut terminal, &last_update, &cached_lines, style_before, style_current, style_after)?;
                } else {
                    break;
                }
            }
            maybe_event = tokio::task::spawn_blocking(|| crossterm::event::poll(std::time::Duration::from_millis(100))) => {
                if let Ok(Ok(true)) = maybe_event {
                    let event = crossterm::event::read()?;
                    match event {
                        Event::Key(key) => {
                            match key.code {
                                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => break,
                                _ => {}
                            }
                        },
                        _ => {},
                    }
                    draw_ui_with_cache(&mut terminal, &last_update, &cached_lines, style_before, style_current, style_after)?;
                }
            }
        }
    }
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

fn draw_ui_with_cache<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    last_update: &Option<Update>,
    cached_lines: &Option<Vec<String>>,
    style_before: tui::style::Style,
    style_current: tui::style::Style,
    style_after: tui::style::Style,
) -> Result<(), Box<dyn std::error::Error>> {
    terminal.draw(|f| {
        let size = f.size();
        let w = size.width as usize;
        let h = size.height as usize;
        let mut lines = Vec::new();
        if let Some(update) = last_update {
            if let Some(ref err) = update.err {
                let pad_top = h / 2;
                lines.extend((0..pad_top).map(|_| Spans::from(Span::raw(""))));
                for wrapped in wrap_text(err, w) {
                    lines.push(Spans::from(Span::styled(pad_centered(&wrapped, w), style_current)));
                }
            } else if let Some(cached) = cached_lines {
                if !cached.is_empty() && update.index < cached.len() {
                    let wrapped_lines: Vec<Vec<String>> = cached.iter().map(|l| wrap_text(l, w)).collect();
                    let visual_heights: Vec<usize> = wrapped_lines.iter().map(|v| v.len()).collect();
                    let current_block = &wrapped_lines[update.index];
                    let current_height = current_block.len();
                    let mut visible = Vec::new();
                    if current_height >= h {
                        // If the current line alone is taller than the window, show only its last h lines
                        for l in current_block[current_height.saturating_sub(h)..].iter() {
                            visible.push(Spans::from(Span::styled(pad_centered(l, w), style_current)));
                        }
                        // No context, no centering
                        lines.extend(visible);
                    } else {
                        // Center the current block with as much context as possible
                        let context_lines = h - current_height;
                        let mut before = Vec::new();
                        let mut after = Vec::new();
                        let mut lines_needed_before = context_lines / 2;
                        let mut lines_needed_after = context_lines - lines_needed_before;
                        // Fill before context
                        let mut i = update.index;
                        while i > 0 && lines_needed_before > 0 {
                            i -= 1;
                            let take = visual_heights[i].min(lines_needed_before);
                            let start = visual_heights[i] - take;
                            for l in &wrapped_lines[i][start..] {
                                before.push(Spans::from(Span::styled(pad_centered(l, w), style_before)));
                            }
                            lines_needed_before -= take;
                        }
                        before.reverse();
                        // Fill after context
                        let mut j = update.index + 1;
                        while j < wrapped_lines.len() && lines_needed_after > 0 {
                            let take = visual_heights[j].min(lines_needed_after);
                            for l in &wrapped_lines[j][..take] {
                                after.push(Spans::from(Span::styled(pad_centered(l, w), style_after)));
                            }
                            lines_needed_after -= take;
                            j += 1;
                        }
                        visible.extend(before);
                        for l in current_block {
                            visible.push(Spans::from(Span::styled(pad_centered(l, w), style_current)));
                        }
                        visible.extend(after);
                        // Pad top/bottom if not enough context
                        let pad_top = h.saturating_sub(visible.len()) / 2;
                        let pad_bottom = h.saturating_sub(visible.len() + pad_top);
                        lines.extend((0..pad_top).map(|_| Spans::from(Span::raw(""))));
                        lines.extend(visible);
                        lines.extend((0..pad_bottom).map(|_| Spans::from(Span::raw(""))));
                    }
                }
            } else if let Some(ref unsynced) = update.unsynced {
                if !unsynced.trim().is_empty() {
                    let pad_top = h / 2 - 1;
                    lines.extend((0..pad_top).map(|_| Spans::from(Span::raw(""))));
                    let unsynced_lines = vec![
                        "--- Unsynced Lyrics ---",
                        unsynced,
                        "----------------------",
                    ];
                    for l in unsynced_lines {
                        for wrapped in wrap_text(l, w) {
                            lines.push(Spans::from(Span::styled(pad_centered(&wrapped, w), style_current)));
                        }
                    }
                } else {
                    let pad_top = h / 2;
                    lines.extend((0..pad_top).map(|_| Spans::from(Span::raw(""))));
                    let msg = "No lyrics found for this track.";
                    for wrapped in wrap_text(msg, w) {
                        lines.push(Spans::from(Span::styled(pad_centered(&wrapped, w), style_current)));
                    }
                }
            } else {
                let pad_top = h / 2;
                lines.extend((0..pad_top).map(|_| Spans::from(Span::raw(""))));
                let msg = "No lyrics found for this track.";
                for wrapped in wrap_text(msg, w) {
                    lines.push(Spans::from(Span::styled(pad_centered(&wrapped, w), style_current)));
                }
            }
        } else {
            let pad_top = h / 2;
            lines.extend((0..pad_top).map(|_| Spans::from(Span::raw(""))));
            let msg = "Waiting for lyrics...";
            for wrapped in wrap_text(msg, w) {
                lines.push(Spans::from(Span::raw(pad_centered(&wrapped, w))));
            }
        }
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left);
        f.render_widget(paragraph, size);
    })?;
    Ok(())
}
