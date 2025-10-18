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
use tui::{
    backend::Backend,
    layout::{Alignment, Rect},
    terminal::Terminal,
    text::{Span, Spans},
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
) -> Result<(), Box<dyn Error + Send + Sync>> {
    terminal
        .draw(|f| {
            let size = f.size();
            let width = size.width as usize;
            let height = size.height as usize;

            let visible_spans = compute_visible_spans(
                last_update,
                wrapped_cache,
                width,
                height,
                styles,
                karaoke_enabled,
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
) -> Vec<Spans<'a>> {
    let Some(update) = last_update else {
        return Vec::new();
    };

    // Render error messages
    if let Some(err) = &update.err {
        return wrap_text(err, width)
            .into_iter()
            .map(|l| Spans::from(Span::styled(l, styles.current)))
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
fn render_centered_paragraph<B: Backend>(
    frame: &mut tui::Frame<B>,
    size: Rect,
    spans: Vec<Spans>,
    height: usize,
) {
    if spans.is_empty() {
        let paragraph = Paragraph::new(vec![Spans::from(Span::raw(""))])
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
    pub before: Vec<Spans<'a>>,
    pub current: Vec<Spans<'a>>,
    pub after: Vec<Spans<'a>>,
}

impl<'a> VisibleLines<'a> {
    pub fn into_vec(self) -> Vec<Spans<'a>> {
        [self.before, self.current, self.after].concat()
    }
}

/// Collect lines before the current index. Returns Spans in visual top->down order.
fn collect_before_spans<'a>(
    current_index: usize,
    wrapped_blocks: &[Vec<String>],
    mut lines_needed: usize,
    style: tui::style::Style,
) -> Vec<Spans<'a>> {
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
            .map(|l| Spans::from(Span::styled(l.clone(), style)))
            .collect::<Vec<_>>();
        // prepend
        result.splice(0..0, spans);
        lines_needed -= take;
    }

    result
}

/// Collect lines after the current index. Returns Spans in visual top->down order.
fn collect_after_spans<'a>(
    current_index: usize,
    wrapped_blocks: &[Vec<String>],
    mut lines_needed: usize,
    style: tui::style::Style,
) -> Vec<Spans<'a>> {
    let mut result = Vec::new();
    let mut j = current_index + 1;
    while j < wrapped_blocks.len() && lines_needed > 0 {
        let block = &wrapped_blocks[j];
        let take = block.len().min(lines_needed);
        for line in block.iter().take(take) {
            result.push(Spans::from(Span::styled(line.clone(), style)));
        }
        lines_needed -= take;
        j += 1;
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
pub fn gather_visible_lines<'a>(
    update: &Update,
    wrapped_blocks: &[Vec<String>],
    w: usize,
    h: usize,
    styles: &'a LyricStyles,
    position: f64,
    karaoke_enabled: bool,
) -> VisibleLines<'a> {
    let idx_for_context = update.index.unwrap_or(0);
    let current_block = wrapped_blocks
        .get(idx_for_context)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let current_height = current_block.len();

    // Build current line spans (with karaoke if applicable)
    let current_spans = build_current_spans(
        update,
        current_block,
        w,
        styles,
        position,
        karaoke_enabled,
    );

    // If current block fills the screen, no context needed
    if current_height >= h {
        return VisibleLines {
            before: Vec::new(),
            current: current_spans,
            after: Vec::new(),
        };
    }

    // Calculate context lines
    let context_lines = h.saturating_sub(current_height);
    let lines_before = context_lines / 2;
    let lines_after = context_lines - lines_before;

    let before = collect_before_spans(idx_for_context, wrapped_blocks, lines_before, styles.before);
    let after = collect_after_spans(idx_for_context, wrapped_blocks, lines_after, styles.after);

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
) -> Vec<Spans<'a>> {
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
        .map(|line| Spans::from(Span::styled(line.clone(), style)))
        .collect()
}

/// Try to build per-word karaoke spans for richsync lyrics.
fn try_build_karaoke_spans<'a>(
    update: &Update,
    idx: usize,
    width: usize,
    styles: &'a LyricStyles,
    position: f64,
) -> Option<Vec<Spans<'a>>> {
    let line = update.lines.get(idx)?;
    let words = line.words.as_ref()?;

    let word_lines = split_words_into_lines(words, width);
    let mut result = Vec::new();

    for word_line in word_lines {
        let line_spans = build_word_line_spans(&word_line, position, styles);
        result.push(Spans::from(line_spans));
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
    let total_graphemes = word.graphemes.len();
    let highlighted_count = ((fraction * total_graphemes as f64).floor() as usize).min(total_graphemes);

    if highlighted_count == 0 {
        return vec![Span::styled(format!("{}{}", word.text, suffix), styles.after)];
    }

    if highlighted_count >= total_graphemes {
        return vec![Span::styled(format!("{}{}", word.text, suffix), styles.current)];
    }

    // Split at grapheme boundary
    let start_byte = word.grapheme_byte_offsets[0];
    let split_byte = word.grapheme_byte_offsets[highlighted_count];
    let highlighted = &word.text[start_byte..split_byte];
    let remaining = &word.text[split_byte..];

    vec![
        Span::styled(highlighted.to_string(), styles.current),
        Span::styled(format!("{}{}", remaining, suffix), styles.after),
    ]
}
