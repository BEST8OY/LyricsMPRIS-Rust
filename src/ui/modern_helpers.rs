//! Rendering helpers for the modern TUI mode.
//!
//! This module provides:
//! - Wrapped text caching for efficient re-rendering
//! - Visible line selection with context (before/after current line)
//! - Per-word karaoke span generation for richsync lyrics
//! - Centered vertical layout calculation

use crate::state::Update;
use crate::text_utils::wrap_text;
use crate::ui::styles::LyricStyles;
use ratatui::{
    Terminal,
    backend::Backend,
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::error::Error;
/// A wrapped lyric line block containing primary text lines and optional romanized lines.
#[derive(Clone, Debug, PartialEq)]
pub struct WrappedBlock {
    pub primary: Vec<String>,
    pub romanized: Vec<String>,
}

impl WrappedBlock {
    pub fn len(&self) -> usize {
        self.primary.len() + self.romanized.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.primary.is_empty() && self.romanized.is_empty()
    }

    pub fn styled_lines<'a>(
        &'a self,
        style: ratatui::style::Style,
    ) -> impl Iterator<Item = Line<'a>> {
        let rom_style = style.add_modifier(ratatui::style::Modifier::ITALIC);
        self.primary
            .iter()
            .map(move |l| Line::from(Span::styled(l.as_str(), style)))
            .chain(
                self.romanized
                    .iter()
                    .map(move |l| Line::from(Span::styled(l.as_str(), rom_style))),
            )
    }
}

/// Draw the UI using cached wrapped lines.
///
/// This function handles:
/// - Error message rendering
/// - Wrapped text caching (invalidated on width or romanize change)
/// - Visible line computation with context
/// - Vertical centering
#[allow(clippy::too_many_arguments)]
pub fn draw_ui_with_cache<B: Backend>(
    terminal: &mut Terminal<B>,
    last_update: &Option<Update>,
    wrapped_cache: &mut Option<(usize, bool, Vec<WrappedBlock>)>,
    styles: &LyricStyles,
    karaoke_enabled: bool,
    romanize_enabled: bool,
    max_visible_lines: Option<usize>,
    scroll_offset: isize,
) -> Result<(), Box<dyn Error + Send + Sync>>
where
    B::Error: Error + Send + Sync + 'static,
{
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
                romanize_enabled,
                max_visible_lines,
                scroll_offset,
            );

            render_centered_paragraph(f, size, visible_spans, height);
        })
        .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

    Ok(())
}

/// Compute the visible spans to render based on current state.
#[allow(clippy::too_many_arguments)]
fn compute_visible_spans<'a>(
    last_update: &'a Option<Update>,
    wrapped_cache: &'a mut Option<(usize, bool, Vec<WrappedBlock>)>,
    width: usize,
    height: usize,
    styles: &'a LyricStyles,
    karaoke_enabled: bool,
    romanize_enabled: bool,
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

    let blocks = ensure_wrapped_cache(wrapped_cache, &update.lines, width, romanize_enabled);
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

/// Ensure wrapped cache is valid for current width, line count, and romanize state.
/// Returns a reference to the cached blocks.
fn ensure_wrapped_cache<'a>(
    wrapped_cache: &'a mut Option<(usize, bool, Vec<WrappedBlock>)>,
    lines: &[crate::lyrics::LyricLine],
    width: usize,
    romanize_enabled: bool,
) -> &'a Vec<WrappedBlock> {
    let needs_rebuild = match wrapped_cache {
        Some((cached_w, cached_rom, blocks)) => {
            *cached_w != width || *cached_rom != romanize_enabled || blocks.len() != lines.len()
        }
        None => true,
    };

    if needs_rebuild {
        let new_blocks: Vec<WrappedBlock> = lines
            .iter()
            .map(|l| {
                let primary = wrap_text(&l.text, width);
                let romanized = if romanize_enabled && let Some(ref rom) = l.romanized {
                    wrap_text(rom, width)
                } else {
                    Vec::new()
                };
                WrappedBlock { primary, romanized }
            })
            .collect();
        *wrapped_cache = Some((width, romanize_enabled, new_blocks));
    }

    &wrapped_cache.as_ref().unwrap().2
}

/// Render a paragraph centered vertically in the given area.
fn render_centered_paragraph(
    frame: &mut ratatui::Frame,
    size: Rect,
    spans: Vec<Line>,
    height: usize,
) {
    if spans.is_empty() {
        let paragraph =
            Paragraph::new(vec![Line::from(Span::raw(""))]).alignment(Alignment::Center);
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
        let total = self.before.len() + self.current.len() + self.after.len();
        let mut result = Vec::with_capacity(total);
        result.extend(self.before);
        result.extend(self.current);
        result.extend(self.after);
        result
    }
}

/// Collect lines before the current index. Returns Lines in visual top->down order.
fn collect_before_spans<'a>(
    current_index: usize,
    wrapped_blocks: &'a [WrappedBlock],
    mut lines_needed: usize,
    styles: &'a LyricStyles,
) -> Vec<Line<'a>> {
    // Walk backwards collecting per-block tails; blocks are gathered bottom-up
    // and reversed once at the end (avoids O(n^2) front-splicing).
    let mut chunks: Vec<Vec<Line<'a>>> = Vec::new();

    let mut i = current_index;
    while i > 0 && lines_needed > 0 {
        i -= 1;
        let dist = current_index - i;
        let style = styles.get_line_style_by_distance(dist, true);
        let block = &wrapped_blocks[i];
        let block_lines: Vec<Line<'a>> = block.styled_lines(style).collect();
        let take = block_lines.len().min(lines_needed);
        let start = block_lines.len() - take;
        chunks.push(block_lines[start..].to_vec());
        lines_needed -= take;
    }

    chunks.reverse();
    chunks.into_iter().flatten().collect()
}

/// Collect lines after the current index. Returns Lines in visual top->down order.
fn collect_after_spans<'a>(
    current_index: usize,
    wrapped_blocks: &'a [WrappedBlock],
    mut lines_needed: usize,
    styles: &'a LyricStyles,
) -> Vec<Line<'a>> {
    let mut result = Vec::new();
    let mut j = current_index + 1;
    while j < wrapped_blocks.len() && lines_needed > 0 {
        let dist = j - current_index;
        let style = styles.get_line_style_by_distance(dist, false);
        let block = &wrapped_blocks[j];
        let block_lines: Vec<Line<'a>> = block.styled_lines(style).collect();
        let take = block_lines.len().min(lines_needed);
        for line in block_lines.into_iter().take(take) {
            result.push(line);
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
    wrapped_blocks: &'a [WrappedBlock],
    blocks_needed: usize,
    styles: &'a LyricStyles,
) -> Vec<Line<'a>> {
    // Blocks are gathered bottom-up and reversed once at the end.
    let mut chunks: Vec<Vec<Line<'a>>> = Vec::new();
    let start_index = current_index.saturating_sub(blocks_needed);

    for block_idx in (start_index..current_index).rev() {
        let dist = current_index - block_idx;
        let style = styles.get_line_style_by_distance(dist, true);
        chunks.push(wrapped_blocks[block_idx].styled_lines(style).collect());
    }

    chunks.reverse();
    chunks.into_iter().flatten().collect()
}

/// Collect complete lyric blocks after the current index (for max_visible_lines mode).
/// Returns all wrapped lines from each block in visual top->down order.
fn collect_after_blocks<'a>(
    current_index: usize,
    wrapped_blocks: &'a [WrappedBlock],
    blocks_needed: usize,
    styles: &'a LyricStyles,
) -> Vec<Line<'a>> {
    let mut result = Vec::new();
    let end_index = (current_index + 1 + blocks_needed).min(wrapped_blocks.len());

    for (block_idx, block) in wrapped_blocks
        .iter()
        .enumerate()
        .take(end_index)
        .skip(current_index + 1)
    {
        let dist = block_idx - current_index;
        let style = styles.get_line_style_by_distance(dist, false);
        result.extend(block.styled_lines(style));
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
        let candidate = if current.is_empty() {
            wlen
        } else {
            cur_len + 1 + wlen
        };
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

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
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
#[allow(clippy::too_many_arguments)]
pub fn gather_visible_lines<'a>(
    update: &'a Update,
    wrapped_blocks: &'a [WrappedBlock],
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

    let current_block = wrapped_blocks.get(effective_index);
    let current_height = current_block.map(|b| b.len()).unwrap_or(0);

    // Build current line spans (with karaoke if applicable, but only when not scrolled)
    let use_karaoke = karaoke_enabled && scroll_offset == 0 && update.playing;
    let current_spans = match current_block {
        Some(block) => build_current_spans(update, block, w, styles, position, use_karaoke),
        None => Vec::new(),
    };

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
        collect_before_blocks(effective_index, wrapped_blocks, lines_before, styles)
    } else {
        collect_before_spans(effective_index, wrapped_blocks, lines_before, styles)
    };

    let after = if max_visible_lines.is_some() {
        collect_after_blocks(effective_index, wrapped_blocks, lines_after, styles)
    } else {
        collect_after_spans(effective_index, wrapped_blocks, lines_after, styles)
    };

    VisibleLines {
        before,
        current: current_spans,
        after,
    }
}

/// Build spans for the current line, applying karaoke highlighting and romanized subtitle if appropriate.
fn build_current_spans<'a>(
    update: &'a Update,
    current_block: &'a WrappedBlock,
    width: usize,
    styles: &'a LyricStyles,
    position: f64,
    karaoke_enabled: bool,
) -> Vec<Line<'a>> {
    // Try to build richsync karaoke spans
    let mut lines = if let Some(idx) = update.index
        && karaoke_enabled
        && matches!(
            update.provider,
            Some(crate::state::Provider::MusixmatchRichsync)
        )
        && let Some(spans) = try_build_karaoke_spans(update, idx, width, styles, position)
    {
        spans
    } else {
        let style = if update.index.is_some() {
            styles.current
        } else {
            styles.after
        };
        current_block
            .primary
            .iter()
            .map(|line| Line::from(Span::styled(line.as_str(), style)))
            .collect()
    };

    // Append romanized subtitle lines if present
    let rom_style = styles.after.add_modifier(ratatui::style::Modifier::ITALIC);
    for rom in &current_block.romanized {
        lines.push(Line::from(Span::styled(rom.as_str(), rom_style)));
    }

    lines
}

/// Try to build per-word karaoke spans for richsync lyrics.
fn try_build_karaoke_spans<'a>(
    update: &'a Update,
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
    words: &[&'a crate::lyrics::types::WordTiming],
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

/// Styled span for a full word, borrowing the text unless a space suffix must be appended.
fn styled_word<'a>(text: &'a str, suffix: &str, style: ratatui::style::Style) -> Span<'a> {
    if suffix.is_empty() {
        Span::styled(text, style)
    } else {
        Span::styled(format!("{text}{suffix}"), style)
    }
}

/// Build spans for a single word with partial grapheme highlighting.
///
/// Word text is borrowed whenever possible; allocation only occurs for the
/// partially highlighted remainder followed by an inter-word space.
fn build_word_spans<'a>(
    word: &'a crate::lyrics::types::WordTiming,
    position: f64,
    styles: &'a LyricStyles,
    is_last_in_line: bool,
) -> Vec<Span<'a>> {
    let suffix = if is_last_in_line { "" } else { " " };

    // Word not yet reached
    if position < word.start {
        return vec![styled_word(&word.text, suffix, styles.after)];
    }

    // Word fully passed
    if position >= word.end {
        return vec![styled_word(&word.text, suffix, styles.current)];
    }

    // Word partially highlighted
    let duration = (word.end - word.start).max(f64::EPSILON);
    let fraction = ((position - word.start) / duration).clamp(0.0, 1.0);
    let total_graphemes = word.grapheme_count();
    let highlighted_count =
        ((fraction * total_graphemes as f64).floor() as usize).min(total_graphemes);

    if highlighted_count == 0 {
        return vec![styled_word(&word.text, suffix, styles.after)];
    }

    if highlighted_count >= total_graphemes {
        return vec![styled_word(&word.text, suffix, styles.current)];
    }

    // Split at grapheme boundary using the precomputed boundaries
    let split_byte = word.grapheme_boundaries[highlighted_count];
    let remaining = &word.text[split_byte..];

    vec![
        Span::styled(&word.text[..split_byte], styles.current),
        styled_word(remaining, suffix, styles.after),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrapped_block_styled_lines() {
        let block = WrappedBlock {
            primary: vec!["こんにちは".to_string()],
            romanized: vec!["konnichiwa".to_string()],
        };

        assert_eq!(block.len(), 2);
        assert!(!block.is_empty());

        let styles = LyricStyles::default();
        let lines: Vec<Line> = block.styled_lines(styles.current).collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_ensure_wrapped_cache_with_romanize() {
        let mut cache = None;
        let lines = vec![
            crate::lyrics::LyricLine::new(0.0, "こんにちは")
                .with_romanized(Some("konnichiwa".to_string())),
        ];

        // Romanize disabled
        let blocks = ensure_wrapped_cache(&mut cache, &lines, 80, false);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].primary.len(), 1);
        assert!(blocks[0].romanized.is_empty());

        // Romanize enabled (cache invalidates and rebuilds)
        let blocks = ensure_wrapped_cache(&mut cache, &lines, 80, true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].primary.len(), 1);
        assert_eq!(blocks[0].romanized.len(), 1);
        assert_eq!(blocks[0].romanized[0], "konnichiwa");
    }
}
