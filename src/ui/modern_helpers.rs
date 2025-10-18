//! Rendering helpers for the modern TUI mode.
//!
//! This module provides:
//! - Wrapped text caching for efficient re-rendering
//! - Visible line selection with context (before/after current line)
//! - Per-word karaoke span generation for richsync lyrics
//! - Centered vertical layout calculation

use crate::text_utils::wrap_text;
use crate::state::Update;
use crate::ui::styles::LyricStyles;
use ratatui::{
    backend::Backend,
    layout::{Alignment, Rect},
    Terminal,
    text::{Span, Line},
    widgets::Paragraph,
};
use std::error::Error;
/// Draw the UI using cached wrapped lines.
///
/// This function handles:
/// - Error message rendering
/// - Wrapped text caching (invalidated on width change)
/// - Visible line computation with context
/// - Vertical centering
pub fn draw_ui_with_cache<B: Backend>(
    terminal: &mut Terminal<B>,
    last_update: &Option<Update>,
    wrapped_cache: &mut Option<(usize, Vec<Vec<String>>)>,
    styles: &LyricStyles,
    karaoke_enabled: bool,
    max_visible_lines: Option<usize>,
    scroll_offset: isize,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    terminal
        .draw(|f| {
            let size = f.area();
            let width = size.width as usize;
            let height = size.height as usize;

            let visible_spans = compute_visible_spans(
                last_update,
                wrapped_cache,
                width,
                height,
                styles,
                karaoke_enabled,
                max_visible_lines,
                scroll_offset,
            );

            render_centered_paragraph(f, size, visible_spans, height);
        })
        .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

    Ok(())
}

/// Compute the visible spans to render based on current state.
fn compute_visible_spans<'a>(
    last_update: &Option<Update>,
    wrapped_cache: &mut Option<(usize, Vec<Vec<String>>)>,
    width: usize,
    height: usize,
    styles: &'a LyricStyles,
    karaoke_enabled: bool,
    max_visible_lines: Option<usize>,
    scroll_offset: isize,
) -> Vec<Line<'a>> {
    let Some(update) = last_update else {
        return Vec::new();
    };

    // Render error messages
    if let Some(err) = &update.err {
        return wrap_text(err, width)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, styles.current)))
            .collect();
    }

    // Check if we have lyrics
    if update.lines.is_empty() || !update.index.map(|i| i < update.lines.len()).unwrap_or(true) {
        return Vec::new();
    }

    let blocks = ensure_wrapped_cache(wrapped_cache, &update.lines, width);
    let visible = gather_visible_lines(
        update,
        blocks,
        width,
        height,
        styles,
        update.position,
        karaoke_enabled,
        max_visible_lines,
        scroll_offset,
    );

    visible.into_vec()
}

/// Ensure wrapped cache is valid for current width and line count.
/// Returns a reference to the cached blocks.
fn ensure_wrapped_cache<'a>(
    wrapped_cache: &'a mut Option<(usize, Vec<Vec<String>>)>,
    lines: &[crate::lyrics::LyricLine],
    width: usize,
) -> &'a Vec<Vec<String>> {
    let needs_rebuild = match wrapped_cache {
        Some((cached_w, blocks)) => *cached_w != width || blocks.len() != lines.len(),
        None => true,
    };

    if needs_rebuild {
        let new_blocks: Vec<Vec<String>> = lines
            .iter()
            .map(|l| wrap_text(&l.text, width))
            .collect();
        *wrapped_cache = Some((width, new_blocks));
    }

    &wrapped_cache.as_ref().unwrap().1
}

/// Render a paragraph centered vertically in the given area.
fn render_centered_paragraph(
    frame: &mut ratatui::Frame,
    size: Rect,
    spans: Vec<Line>,
    height: usize,
) {
    if spans.is_empty() {
        let paragraph = Paragraph::new(vec![Line::from(Span::raw(""))])
            .alignment(Alignment::Center);
        frame.render_widget(paragraph, size);
        return;
    }

    let top_padding = height.saturating_sub(spans.len()) / 2;
    let render_area = Rect {
        x: size.x,
        y: size.y + top_padding as u16,
        width: size.width,
        height: (spans.len() as u16).min(size.height),
    };

    let paragraph = Paragraph::new(spans).alignment(Alignment::Center);
    frame.render_widget(paragraph, render_area);
}



/// Collection of styled lines to render.
pub struct VisibleLines<'a> {
    pub before: Vec<Line<'a>>,
    pub current: Vec<Line<'a>>,
    pub after: Vec<Line<'a>>,
}

impl<'a> VisibleLines<'a> {
    pub fn into_vec(self) -> Vec<Line<'a>> {
        [self.before, self.current, self.after].concat()
    }
}

/// Collect lines before the current index. Returns Line in visual top->down order.
fn collect_before_spans<'a>(
    current_index: usize,
    wrapped_blocks: &[Vec<String>],
    mut lines_needed: usize,
    style: ratatui::style::Style,
) -> Vec<Line<'a>> {
    let mut result = Vec::new();

    // Walk backwards collecting lines; prepend each block's tail to maintain order
    let mut i = current_index;
    while i > 0 && lines_needed > 0 {
        i -= 1;
        let block = &wrapped_blocks[i];
        let take = block.len().min(lines_needed);
        let start = block.len() - take;
        // We want these in the same order they appear visually, so collect and then
        // insert at the front.
        let spans = block[start..]
            .iter()
            .map(|l| Line::from(Span::styled(l.clone(), style)))
            .collect::<Vec<_>>();
        // prepend
        result.splice(0..0, spans);
        lines_needed -= take;
    }

    result
}

/// Collect lines after the current index. Returns Line in visual top->down order.
fn collect_after_spans<'a>(
    current_index: usize,
    wrapped_blocks: &[Vec<String>],
    mut lines_needed: usize,
    style: ratatui::style::Style,
) -> Vec<Line<'a>> {
    let mut result = Vec::new();
    let mut j = current_index + 1;
    while j < wrapped_blocks.len() && lines_needed > 0 {
        let block = &wrapped_blocks[j];
        let take = block.len().min(lines_needed);
        for line in block.iter().take(take) {
            result.push(Line::from(Span::styled(line.clone(), style)));
        }
        lines_needed -= take;
        j += 1;
    }
    result
}

/// Collect complete lyric blocks before the current index (for max_visible_lines mode).
/// Returns all wrapped lines from each block in visual top->down order.
fn collect_before_blocks<'a>(
    current_index: usize,
    wrapped_blocks: &[Vec<String>],
    blocks_needed: usize,
    style: ratatui::style::Style,
) -> Vec<Line<'a>> {
    let mut result = Vec::new();
    let start_index = current_index.saturating_sub(blocks_needed);
    
    for i in start_index..current_index {
        let block = &wrapped_blocks[i];
        for line in block {
            result.push(Line::from(Span::styled(line.clone(), style)));
        }
    }
    
    result
}

/// Collect complete lyric blocks after the current index (for max_visible_lines mode).
/// Returns all wrapped lines from each block in visual top->down order.
fn collect_after_blocks<'a>(
    current_index: usize,
    wrapped_blocks: &[Vec<String>],
    blocks_needed: usize,
    style: ratatui::style::Style,
) -> Vec<Line<'a>> {
    let mut result = Vec::new();
    let end_index = (current_index + 1 + blocks_needed).min(wrapped_blocks.len());
    
    for i in (current_index + 1)..end_index {
        let block = &wrapped_blocks[i];
        for line in block {
            result.push(Line::from(Span::styled(line.clone(), style)));
        }
    }
    
    result
}

/// Split a slice of WordTiming into visual lines that fit into `width` characters.
fn split_words_into_lines<'b>(
    words: &'b [crate::lyrics::types::WordTiming],
    width: usize,
) -> Vec<Vec<&'b crate::lyrics::types::WordTiming>> {
    let mut lines: Vec<Vec<&'b crate::lyrics::types::WordTiming>> = Vec::new();
    let mut current: Vec<&'b crate::lyrics::types::WordTiming> = Vec::new();
    let mut cur_len: usize = 0;

    for w in words {
        let wlen = w.text.chars().count();
        let candidate = if current.is_empty() { wlen } else { cur_len + 1 + wlen };
        if !current.is_empty() && candidate > width && width > 0 {
            lines.push(current);
            current = Vec::new();
            cur_len = 0;
        }
        if current.is_empty() {
            current.push(w);
            cur_len = wlen;
        } else {
            current.push(w);
            cur_len += 1 + wlen;
        }
    }

    if !current.is_empty() { lines.push(current); }
    if lines.is_empty() { lines.push(Vec::new()); }
    lines
}

/// Build VisibleLines from an Update and wrapped_blocks.
///
/// If `update.index` is None, renders using `styles.after` (dimmed).
/// For richsync with karaoke enabled, builds per-word spans with partial highlighting.
/// 
/// # Arguments
/// * `max_visible_lines` - Maximum number of lyric blocks to display (None = unlimited)
/// * `scroll_offset` - Manual scroll offset in lyric blocks when paused
pub fn gather_visible_lines<'a>(
    update: &Update,
    wrapped_blocks: &[Vec<String>],
    w: usize,
    h: usize,
    styles: &'a LyricStyles,
    position: f64,
    karaoke_enabled: bool,
    max_visible_lines: Option<usize>,
    scroll_offset: isize,
) -> VisibleLines<'a> {
    // Calculate the effective index considering scroll offset when paused
    let base_index = update.index.unwrap_or(0);
    let effective_index = if !update.playing {
        // When paused, allow scrolling
        (base_index as isize + scroll_offset)
            .max(0)
            .min(wrapped_blocks.len().saturating_sub(1) as isize) as usize
    } else {
        base_index
    };
    
    let current_block = wrapped_blocks
        .get(effective_index)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let current_height = current_block.len();

    // Build current line spans (with karaoke if applicable, but only when not scrolled)
    let use_karaoke = karaoke_enabled && scroll_offset == 0 && update.playing;
    let current_spans = build_current_spans(
        update,
        current_block,
        w,
        styles,
        position,
        use_karaoke,
    );

    // Calculate available height considering max_visible_lines
    let available_height = if let Some(max) = max_visible_lines {
        // max_visible_lines is in terms of lyric blocks, not wrapped screen lines
        // We need to limit the total number of blocks (before + current + after)
        h.min(max)
    } else {
        h
    };

    // If current block fills the available space, no context needed
    if current_height >= available_height {
        return VisibleLines {
            before: Vec::new(),
            current: current_spans,
            after: Vec::new(),
        };
    }

    // Calculate context lines for max_visible_lines
    let (lines_before, lines_after) = if let Some(max) = max_visible_lines {
        // Limit to max blocks total
        let context_blocks = max.saturating_sub(1); // -1 for current block
        let before_blocks = context_blocks / 2;
        let after_blocks = context_blocks - before_blocks;
        
        // Count how many wrapped lines each block would contribute
        // For simplicity, we'll use a heuristic approach
        (before_blocks, after_blocks)
    } else {
        // Original behavior: fill screen with wrapped lines
        let context_lines = available_height.saturating_sub(current_height);
        let lines_before = context_lines / 2;
        let lines_after = context_lines - lines_before;
        (lines_before, lines_after)
    };

    let before = if max_visible_lines.is_some() {
        collect_before_blocks(effective_index, wrapped_blocks, lines_before, styles.before)
    } else {
        collect_before_spans(effective_index, wrapped_blocks, lines_before, styles.before)
    };
    
    let after = if max_visible_lines.is_some() {
        collect_after_blocks(effective_index, wrapped_blocks, lines_after, styles.after)
    } else {
        collect_after_spans(effective_index, wrapped_blocks, lines_after, styles.after)
    };

    VisibleLines {
        before,
        current: current_spans,
        after,
    }
}

/// Build spans for the current line, applying karaoke highlighting if appropriate.
fn build_current_spans<'a>(
    update: &Update,
    current_block: &[String],
    width: usize,
    styles: &'a LyricStyles,
    position: f64,
    karaoke_enabled: bool,
) -> Vec<Line<'a>> {
    // Try to build richsync karaoke spans
    if let Some(idx) = update.index
        && karaoke_enabled && matches!(update.provider, Some(crate::state::Provider::MusixmatchRichsync))
            && let Some(spans) = try_build_karaoke_spans(update, idx, width, styles, position) {
                return spans;
            }

    // Fallback: render wrapped block with appropriate style
    let style = if update.index.is_some() {
        styles.current
    } else {
        styles.after
    };

    current_block
        .iter()
        .map(|line| Line::from(Span::styled(line.clone(), style)))
        .collect()
}

/// Try to build per-word karaoke spans for richsync lyrics.
fn try_build_karaoke_spans<'a>(
    update: &Update,
    idx: usize,
    width: usize,
    styles: &'a LyricStyles,
    position: f64,
) -> Option<Vec<Line<'a>>> {
    let line = update.lines.get(idx)?;
    let words = line.words.as_ref()?;

    let word_lines = split_words_into_lines(words, width);
    let mut result = Vec::new();

    for word_line in word_lines {
        let line_spans = build_word_line_spans(&word_line, position, styles);
        result.push(Line::from(line_spans));
    }

    Some(result)
}

/// Build spans for a single line of words with per-word/grapheme highlighting.
fn build_word_line_spans<'a>(
    words: &[&crate::lyrics::types::WordTiming],
    position: f64,
    styles: &'a LyricStyles,
) -> Vec<Span<'a>> {
    let mut spans = Vec::new();

    for (i, word) in words.iter().enumerate() {
        let is_last = i + 1 >= words.len();
        let word_spans = build_word_spans(word, position, styles, is_last);
        spans.extend(word_spans);
    }

    spans
}

/// Build spans for a single word with partial grapheme highlighting.
fn build_word_spans<'a>(
    word: &crate::lyrics::types::WordTiming,
    position: f64,
    styles: &'a LyricStyles,
    is_last_in_line: bool,
) -> Vec<Span<'a>> {
    let suffix = if is_last_in_line { "" } else { " " };

    // Word not yet reached
    if position < word.start {
        return vec![Span::styled(format!("{}{}", word.text, suffix), styles.after)];
    }

    // Word fully passed
    if position >= word.end {
        return vec![Span::styled(format!("{}{}", word.text, suffix), styles.current)];
    }

    // Word partially highlighted
    let duration = (word.end - word.start).max(f64::EPSILON);
    let fraction = ((position - word.start) / duration).clamp(0.0, 1.0);
    let total_graphemes = word.grapheme_count();
    let highlighted_count = ((fraction * total_graphemes as f64).floor() as usize).min(total_graphemes);

    if highlighted_count == 0 {
        return vec![Span::styled(format!("{}{}", word.text, suffix), styles.after)];
    }

    if highlighted_count >= total_graphemes {
        return vec![Span::styled(format!("{}{}", word.text, suffix), styles.current)];
    }

    // Split at grapheme boundary using the precomputed boundaries
    let split_byte = word.grapheme_boundaries[highlighted_count];
    let highlighted = &word.text[..split_byte];
    let remaining = &word.text[split_byte..];

    vec![
        Span::styled(highlighted.to_string(), styles.current),
        Span::styled(format!("{}{}", remaining, suffix), styles.after),
    ]
}
