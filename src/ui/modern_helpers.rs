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
use unicode_width::UnicodeWidthStr;

/// Returns true if `c` is a CJK ideograph, kana, hangul, or related CJK character.
#[inline]
pub fn is_cjk_char(c: char) -> bool {
    matches!(c as u32,
        // CJK Unified Ideographs & Extension A-F
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF |
        // Hiragana & Katakana
        0x3040..=0x309F | 0x30A0..=0x30FF | 0x31F0..=0x31FF |
        // Hangul Syllables & Jamo
        0xAC00..=0xD7AF | 0x1100..=0x11FF | 0x3130..=0x318F |
        // CJK Symbols and Punctuation & Halfwidth/Fullwidth Forms
        0x3000..=0x303F | 0xFF00..=0xFFEF
    )
}

/// Determines if a space should be inserted between `curr` word and `next` word.
/// Returns false if both adjacent boundaries are CJK characters.
pub fn should_add_space_between(curr: &str, next: &str) -> bool {
    let curr_last = curr.chars().rev().find(|c| !c.is_whitespace());
    let next_first = next.chars().find(|c| !c.is_whitespace());
    match (curr_last, next_first) {
        (Some(c1), Some(c2)) => !(is_cjk_char(c1) && is_cjk_char(c2)),
        _ => true,
    }
}

/// Returns true if the background vocal segment appears before any primary vocal text in `text`.
pub fn is_line_background_first(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.chars().next().map(is_open_paren).unwrap_or(false)
}

/// Determines whether background vocals precede primary vocals chronologically.
/// In richsync (when word timings are present), uses authoritative word start timestamps.
/// In text/LRC lines, checks whether the line starts with an opening parenthesis or bracket.
pub fn determine_background_first(
    line_text: &str,
    words: Option<&[crate::lyrics::types::WordTiming]>,
) -> bool {
    if let Some(words) = words {
        let (main_words, bg_words) = split_words_main_and_background(words);
        if let (Some(first_bg), Some(first_main)) = (bg_words.first(), main_words.first()) {
            if (first_bg.start - first_main.start).abs() > f64::EPSILON {
                return first_bg.start < first_main.start;
            }
        }
    }
    is_line_background_first(line_text)
}

/// A segment of text within an inline line, indicating whether it represents background vocals.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineSegment {
    pub text: String,
    pub is_background: bool,
}

/// A wrapped lyric line block containing primary text lines, background vocal lines, and optional romanized lines.
#[derive(Clone, Debug, PartialEq)]
pub struct WrappedBlock {
    pub primary: Vec<String>,
    pub background: Vec<String>,
    pub romanized_primary: Vec<String>,
    pub romanized_background: Vec<String>,
    /// Wrapped lines for inline inactive rendering (main + parenthetical background vocals inline)
    pub inline_lines: Vec<Vec<InlineSegment>>,
    /// Wrapped romanized lines for inline inactive rendering
    pub inline_romanized: Vec<Vec<InlineSegment>>,
    /// Whether background vocals precede primary vocals chronologically
    pub bg_first: bool,
}

impl WrappedBlock {
    pub fn len(&self) -> usize {
        self.primary.len()
            + self.background.len()
            + self.romanized_primary.len()
            + self.romanized_background.len()
    }

    #[allow(dead_code)]
    pub fn inline_len(&self) -> usize {
        self.inline_lines.len() + self.inline_romanized.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.primary.is_empty()
            && self.background.is_empty()
            && self.romanized_primary.is_empty()
            && self.romanized_background.is_empty()
            && self.inline_lines.is_empty()
    }

    pub fn styled_lines<'a>(
        &'a self,
        style: ratatui::style::Style,
        bg_style: ratatui::style::Style,
    ) -> impl Iterator<Item = Line<'a>> {
        let rom_style = style.add_modifier(ratatui::style::Modifier::ITALIC);
        let rom_bg_style = bg_style.add_modifier(ratatui::style::Modifier::ITALIC);

        let mut lines = Vec::with_capacity(self.len());
        if self.bg_first {
            for l in &self.background {
                lines.push(Line::from(Span::styled(l.as_str(), bg_style)));
            }
            for l in &self.romanized_background {
                lines.push(Line::from(Span::styled(l.as_str(), rom_bg_style)));
            }
            for l in &self.primary {
                lines.push(Line::from(Span::styled(l.as_str(), style)));
            }
            for l in &self.romanized_primary {
                lines.push(Line::from(Span::styled(l.as_str(), rom_style)));
            }
        } else {
            for l in &self.primary {
                lines.push(Line::from(Span::styled(l.as_str(), style)));
            }
            for l in &self.romanized_primary {
                lines.push(Line::from(Span::styled(l.as_str(), rom_style)));
            }
            for l in &self.background {
                lines.push(Line::from(Span::styled(l.as_str(), bg_style)));
            }
            for l in &self.romanized_background {
                lines.push(Line::from(Span::styled(l.as_str(), rom_bg_style)));
            }
        }

        lines.into_iter()
    }

    /// Renders lines with parenthetical background vocals inline (dimmed italic),
    /// used for inactive lines (before / after) to prevent vertical shifting and jumping.
    pub fn styled_lines_inline<'a>(
        &'a self,
        style: ratatui::style::Style,
        bg_style: ratatui::style::Style,
    ) -> impl Iterator<Item = Line<'a>> {
        let rom_style = style.add_modifier(ratatui::style::Modifier::ITALIC);
        let rom_bg_style = bg_style.add_modifier(ratatui::style::Modifier::ITALIC);

        let lines: Vec<Line<'a>> = self
            .inline_lines
            .iter()
            .map(|segments| {
                let spans: Vec<Span<'a>> = segments
                    .iter()
                    .map(|seg| {
                        let s = if seg.is_background { bg_style } else { style };
                        Span::styled(seg.text.as_str(), s)
                    })
                    .collect();
                Line::from(spans)
            })
            .chain(self.inline_romanized.iter().map(|segments| {
                let spans: Vec<Span<'a>> = segments
                    .iter()
                    .map(|seg| {
                        let s = if seg.is_background {
                            rom_bg_style
                        } else {
                            rom_style
                        };
                        Span::styled(seg.text.as_str(), s)
                    })
                    .collect();
                Line::from(spans)
            }))
            .collect();

        lines.into_iter()
    }

    #[allow(dead_code)]
    pub fn styled_lines_primary_only<'a>(
        &'a self,
        style: ratatui::style::Style,
        bg_style: ratatui::style::Style,
    ) -> impl Iterator<Item = Line<'a>> {
        self.styled_lines_inline(style, bg_style)
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
                let (main_text, bg_text) = split_line_main_and_background(&l.text);
                let primary = if let Some(m) = main_text.as_ref() {
                    wrap_text(m, width)
                } else {
                    Vec::new()
                };
                let background = if let Some(b) = bg_text.as_ref() {
                    wrap_text(b, width)
                } else {
                    Vec::new()
                };

                let (rom_main, rom_bg) = if romanize_enabled && let Some(ref rom) = l.romanized {
                    if bg_text.is_none() {
                        (Some(rom.clone()), None)
                    } else if main_text.is_none() {
                        (None, Some(rom.clone()))
                    } else {
                        let (r_main, r_bg) = split_line_main_and_background(rom);
                        if r_bg.is_some() {
                            (r_main, r_bg)
                        } else {
                            (Some(rom.clone()), None)
                        }
                    }
                } else {
                    (None, None)
                };

                let romanized_primary = if let Some(m) = rom_main {
                    wrap_text(&m, width)
                } else {
                    Vec::new()
                };
                let romanized_background = if let Some(b) = rom_bg {
                    wrap_text(&b, width)
                } else {
                    Vec::new()
                };

                let inline_lines = wrap_inline_segments(&l.text, width);
                let inline_romanized = if romanize_enabled && let Some(ref rom) = l.romanized {
                    wrap_inline_segments(rom, width)
                } else {
                    Vec::new()
                };

                let bg_first = determine_background_first(&l.text, l.words.as_deref());

                WrappedBlock {
                    primary,
                    background,
                    romanized_primary,
                    romanized_background,
                    inline_lines,
                    inline_romanized,
                    bg_first,
                }
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
        let bg_style = styles.get_background_style_by_distance(dist, true);
        let block = &wrapped_blocks[i];
        let block_lines: Vec<Line<'a>> = block.styled_lines(style, bg_style).collect();
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
        let bg_style = styles.get_background_style_by_distance(dist, false);
        let block = &wrapped_blocks[j];
        let block_lines: Vec<Line<'a>> = block.styled_lines(style, bg_style).collect();
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
        let bg_style = styles.get_background_style_by_distance(dist, true);
        chunks.push(
            wrapped_blocks[block_idx]
                .styled_lines(style, bg_style)
                .collect(),
        );
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
        let bg_style = styles.get_background_style_by_distance(dist, false);
        result.extend(block.styled_lines(style, bg_style));
    }

    result
}

/// Split a slice of WordTiming into visual lines that fit into `width` terminal columns.
fn split_words_into_lines<'b>(
    words: &[&'b crate::lyrics::types::WordTiming],
    width: usize,
) -> Vec<Vec<&'b crate::lyrics::types::WordTiming>> {
    let mut lines: Vec<Vec<&'b crate::lyrics::types::WordTiming>> = Vec::new();
    let mut current: Vec<&'b crate::lyrics::types::WordTiming> = Vec::new();
    let mut cur_len: usize = 0;

    for &w in words {
        let wlen = w.text.as_str().width();
        let space_len = if current.is_empty() {
            0
        } else {
            let prev = current.last().unwrap();
            if should_add_space_between(&prev.text, &w.text) {
                1
            } else {
                0
            }
        };
        let candidate = if current.is_empty() {
            wlen
        } else {
            cur_len + space_len + wlen
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
            let prev = current.last().unwrap();
            let add_space = should_add_space_between(&prev.text, &w.text);
            current.push(w);
            cur_len += (if add_space { 1 } else { 0 }) + wlen;
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
    let rom_style = styles.after.add_modifier(ratatui::style::Modifier::ITALIC);
    let rom_bg_style = styles
        .background_current
        .add_modifier(ratatui::style::Modifier::ITALIC);

    // Try to build richsync karaoke spans
    if let Some(idx) = update.index
        && karaoke_enabled
        && matches!(
            update.provider,
            Some(crate::state::Provider::MusixmatchRichsync)
        )
        && let Some((main_spans, bg_spans)) =
            try_build_karaoke_spans_split(update, idx, width, styles, position)
    {
        let mut lines = Vec::new();
        if current_block.bg_first {
            if let Some(bs) = bg_spans {
                lines.extend(bs);
                for rom in &current_block.romanized_background {
                    lines.push(Line::from(Span::styled(rom.as_str(), rom_bg_style)));
                }
            }
            if let Some(ms) = main_spans {
                lines.extend(ms);
                for rom in &current_block.romanized_primary {
                    lines.push(Line::from(Span::styled(rom.as_str(), rom_style)));
                }
            }
        } else {
            if let Some(ms) = main_spans {
                lines.extend(ms);
                for rom in &current_block.romanized_primary {
                    lines.push(Line::from(Span::styled(rom.as_str(), rom_style)));
                }
            }
            if let Some(bs) = bg_spans {
                lines.extend(bs);
                for rom in &current_block.romanized_background {
                    lines.push(Line::from(Span::styled(rom.as_str(), rom_bg_style)));
                }
            }
        }
        lines
    } else {
        let (style, bg_style) = if update.index.is_some() {
            (styles.current, styles.background_current)
        } else {
            (styles.after, styles.background_after)
        };

        let mut lines = Vec::new();
        if current_block.bg_first {
            for line in &current_block.background {
                lines.push(Line::from(Span::styled(line.as_str(), bg_style)));
            }
            for rom in &current_block.romanized_background {
                lines.push(Line::from(Span::styled(rom.as_str(), rom_bg_style)));
            }
            for line in &current_block.primary {
                lines.push(Line::from(Span::styled(line.as_str(), style)));
            }
            for rom in &current_block.romanized_primary {
                lines.push(Line::from(Span::styled(rom.as_str(), rom_style)));
            }
        } else {
            for line in &current_block.primary {
                lines.push(Line::from(Span::styled(line.as_str(), style)));
            }
            for rom in &current_block.romanized_primary {
                lines.push(Line::from(Span::styled(rom.as_str(), rom_style)));
            }
            for line in &current_block.background {
                lines.push(Line::from(Span::styled(line.as_str(), bg_style)));
            }
            for rom in &current_block.romanized_background {
                lines.push(Line::from(Span::styled(rom.as_str(), rom_bg_style)));
            }
        }
        lines
    }
}

type KaraokeSplitSpans<'a> = (Option<Vec<Line<'a>>>, Option<Vec<Line<'a>>>);

/// Try to build per-word karaoke spans for richsync lyrics, returning separate lines for main and background vocals.
fn try_build_karaoke_spans_split<'a>(
    update: &'a Update,
    idx: usize,
    width: usize,
    styles: &'a LyricStyles,
    position: f64,
) -> Option<KaraokeSplitSpans<'a>> {
    let line = update.lines.get(idx)?;
    let words = line.words.as_ref()?;

    let (main_words, bg_words) = split_words_main_and_background(words);

    let main_lines = if !main_words.is_empty() {
        let word_lines = split_words_into_lines(&main_words, width);
        let mut result = Vec::new();
        for word_line in word_lines {
            let line_spans = build_word_line_spans(&word_line, position, styles, false);
            result.push(Line::from(line_spans));
        }
        Some(result)
    } else {
        None
    };

    let bg_lines = if !bg_words.is_empty() {
        let word_lines = split_words_into_lines(&bg_words, width);
        let mut result = Vec::new();
        for word_line in word_lines {
            let line_spans = build_word_line_spans(&word_line, position, styles, true);
            result.push(Line::from(line_spans));
        }
        Some(result)
    } else {
        None
    };

    if main_lines.is_some() || bg_lines.is_some() {
        Some((main_lines, bg_lines))
    } else {
        None
    }
}

/// Try to build per-word karaoke spans for richsync lyrics.
#[cfg(test)]
fn try_build_karaoke_spans<'a>(
    update: &'a Update,
    idx: usize,
    width: usize,
    styles: &'a LyricStyles,
    position: f64,
) -> Option<Vec<Line<'a>>> {
    let (main_lines, bg_lines) =
        try_build_karaoke_spans_split(update, idx, width, styles, position)?;
    let line = update.lines.get(idx)?;
    let bg_first = determine_background_first(&line.text, line.words.as_deref());

    let mut result = Vec::new();
    if bg_first {
        if let Some(b) = bg_lines {
            result.extend(b);
        }
        if let Some(m) = main_lines {
            result.extend(m);
        }
    } else {
        if let Some(m) = main_lines {
            result.extend(m);
        }
        if let Some(b) = bg_lines {
            result.extend(b);
        }
    }
    Some(result)
}

/// Build spans for a single line of words with per-word/grapheme highlighting.
fn build_word_line_spans<'a>(
    words: &[&'a crate::lyrics::types::WordTiming],
    position: f64,
    styles: &'a LyricStyles,
    is_background: bool,
) -> Vec<Span<'a>> {
    let mut spans = Vec::new();

    for (i, word) in words.iter().enumerate() {
        let is_last = i + 1 >= words.len();
        let add_space = if is_last {
            false
        } else {
            should_add_space_between(&word.text, &words[i + 1].text)
        };
        let word_spans = build_word_spans(word, position, styles, add_space, is_background);
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

/// Build spans for a single word with partial grapheme highlighting and background vocal support.
fn build_word_spans<'a>(
    word: &'a crate::lyrics::types::WordTiming,
    position: f64,
    styles: &'a LyricStyles,
    has_trailing_space: bool,
    is_background: bool,
) -> Vec<Span<'a>> {
    let suffix = if has_trailing_space { " " } else { "" };

    let current_style = if is_background {
        styles.background_current
    } else {
        styles.current
    };
    let after_style = if is_background {
        styles.background_after
    } else {
        styles.after
    };

    // Word not yet reached
    if position < word.start {
        return vec![styled_word(&word.text, suffix, after_style)];
    }

    // Word fully passed
    if position >= word.end {
        return vec![styled_word(&word.text, suffix, current_style)];
    }

    // Word partially highlighted
    let duration = (word.end - word.start).max(f64::EPSILON);
    let fraction = ((position - word.start) / duration).clamp(0.0, 1.0);
    let total_graphemes = word.grapheme_count();

    // For single-grapheme words ("I", "a", "O"), if position >= word.start,
    // the single character is currently being sung -> highlight it immediately!
    // For multi-grapheme words, clamp to at least min_highlight so the active word provides
    // immediate visual feedback without an initial dead-zone. If word starts with an open paren,
    // ensure both the paren and first letter illuminate together.
    let starts_with_paren = word.text.chars().next().map(is_open_paren).unwrap_or(false);
    let min_highlight = if starts_with_paren && total_graphemes > 1 {
        2
    } else {
        1
    };

    let highlighted_count = if total_graphemes <= min_highlight {
        min_highlight.min(total_graphemes)
    } else {
        ((fraction * total_graphemes as f64).floor() as usize).clamp(min_highlight, total_graphemes)
    };

    if highlighted_count >= total_graphemes {
        return vec![styled_word(&word.text, suffix, current_style)];
    }

    // Split at grapheme boundary using the precomputed boundaries
    let split_byte = word.grapheme_boundaries[highlighted_count];
    let remaining = &word.text[split_byte..];

    vec![
        Span::styled(&word.text[..split_byte], current_style),
        styled_word(remaining, suffix, after_style),
    ]
}

#[inline]
pub fn is_open_paren(c: char) -> bool {
    c == '(' || c == '（' || c == '[' || c == '【' || c == '「'
}

#[inline]
pub fn is_close_paren(c: char) -> bool {
    c == ')' || c == '）' || c == ']' || c == '】' || c == '」'
}

#[inline]
pub fn matching_close_paren(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '（' => Some('）'),
        '[' => Some(']'),
        '【' => Some('】'),
        '「' => Some('」'),
        _ => None,
    }
}

/// Returns the byte ranges within `text` that represent parenthetical background vocals.
pub fn get_background_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let char_indices: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;

    while i < char_indices.len() {
        let (byte_pos, ch) = char_indices[i];
        if is_open_paren(ch) {
            let target_close = matching_close_paren(ch).unwrap_or(')');
            let mut depth = 1;
            let mut j = i + 1;
            let mut close_info = None;
            while j < char_indices.len() {
                let (c_byte, c_ch) = char_indices[j];
                if c_ch == ch {
                    depth += 1;
                } else if c_ch == target_close {
                    depth -= 1;
                    if depth == 0 {
                        close_info = Some((c_byte, c_ch.len_utf8(), j));
                        break;
                    }
                }
                j += 1;
            }

            if let Some((close_byte, close_len, close_j)) = close_info {
                ranges.push(byte_pos..close_byte + close_len);
                i = close_j + 1;
            } else {
                ranges.push(byte_pos..text.len());
                break;
            }
        } else {
            i += 1;
        }
    }

    ranges
}

/// Wraps text into lines where each line is composed of `InlineSegment`s,
/// distinguishing lead vocals from parenthetical background vocals.
pub fn wrap_inline_segments(text: &str, width: usize) -> Vec<Vec<InlineSegment>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let bg_ranges = get_background_ranges(text);
    let wrapped_lines = wrap_text(text, width);

    if bg_ranges.is_empty() {
        return wrapped_lines
            .into_iter()
            .map(|line| {
                vec![InlineSegment {
                    text: line,
                    is_background: false,
                }]
            })
            .collect();
    }

    let is_byte_bg = |byte_idx: usize| -> bool { bg_ranges.iter().any(|r| r.contains(&byte_idx)) };

    let mut result = Vec::new();
    let mut search_byte = 0;

    for line in wrapped_lines {
        let mut segments: Vec<InlineSegment> = Vec::new();
        let mut cur_seg_text = String::new();
        let mut cur_seg_is_bg = None;

        for ch in line.chars() {
            let ch_len = ch.len_utf8();
            let mut found_byte = search_byte;
            while found_byte < text.len() {
                if text[found_byte..].starts_with(ch) {
                    break;
                }
                found_byte += text[found_byte..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(1);
            }

            let ch_is_bg = if found_byte < text.len() {
                search_byte = found_byte + ch_len;
                is_byte_bg(found_byte)
            } else {
                false
            };

            match cur_seg_is_bg {
                Some(is_bg) if is_bg == ch_is_bg => {
                    cur_seg_text.push(ch);
                }
                Some(is_bg) => {
                    segments.push(InlineSegment {
                        text: std::mem::take(&mut cur_seg_text),
                        is_background: is_bg,
                    });
                    cur_seg_text.push(ch);
                    cur_seg_is_bg = Some(ch_is_bg);
                }
                None => {
                    cur_seg_text.push(ch);
                    cur_seg_is_bg = Some(ch_is_bg);
                }
            }
        }

        if !cur_seg_text.is_empty() {
            segments.push(InlineSegment {
                text: cur_seg_text,
                is_background: cur_seg_is_bg.unwrap_or(false),
            });
        }

        result.push(segments);
    }

    result
}

/// Partitions word timings into main vocal words and background vocal words based on parentheses.
/// Supports ASCII `()` `[]` and CJK `（）` `【】` `「」`.
pub fn split_words_main_and_background(
    words: &[crate::lyrics::types::WordTiming],
) -> (
    Vec<&crate::lyrics::types::WordTiming>,
    Vec<&crate::lyrics::types::WordTiming>,
) {
    let mut main_words = Vec::new();
    let mut bg_words = Vec::new();

    let mut i = 0;
    while i < words.len() {
        let has_open = words[i].text.chars().any(is_open_paren);
        if has_open {
            let mut j = i;
            let mut found_close = false;
            while j < words.len() {
                if words[j].text.chars().any(is_close_paren) {
                    found_close = true;
                    break;
                }
                j += 1;
            }
            let end_idx = if found_close { j } else { words.len() - 1 };

            for word in &words[i..=end_idx] {
                bg_words.push(word);
            }
            i = end_idx + 1;
        } else {
            main_words.push(&words[i]);
            i += 1;
        }
    }

    (main_words, bg_words)
}

/// Extracts main text and parenthetical background vocals from a lyric line.
/// Supports standard parentheses `()`, square brackets `[]`, and CJK brackets `（）`, `【】`, `「」`.
///
/// Returns `(main_text, background_text)`:
/// - If line contains no background vocals: `(Some(text.to_string()), None)`
/// - If line contains only background vocals: `(None, Some(text.to_string()))`
/// - If line contains both: `(Some(main), Some(bg))`
pub fn split_line_main_and_background(text: &str) -> (Option<String>, Option<String>) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return (None, None);
    }

    if !text.chars().any(is_open_paren) {
        return (Some(trimmed.to_string()), None);
    }

    let mut main_parts = Vec::new();
    let mut bg_parts = Vec::new();
    let mut current_idx = 0;

    let char_indices: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;

    while i < char_indices.len() {
        let (byte_pos, ch) = char_indices[i];
        if is_open_paren(ch) {
            if byte_pos > current_idx {
                let part = text[current_idx..byte_pos].trim();
                if !part.is_empty() {
                    main_parts.push(part);
                }
            }

            let target_close = matching_close_paren(ch).unwrap_or(')');
            let mut depth = 1;
            let mut close_info = None;
            let mut j = i + 1;
            while j < char_indices.len() {
                let (c_byte, c_ch) = char_indices[j];
                if c_ch == ch {
                    depth += 1;
                } else if c_ch == target_close {
                    depth -= 1;
                    if depth == 0 {
                        close_info = Some((c_byte, c_ch.len_utf8(), j));
                        break;
                    }
                }
                j += 1;
            }

            if let Some((close_byte, close_len, close_j)) = close_info {
                let paren_slice = text[byte_pos..close_byte + close_len].trim();
                if !paren_slice.is_empty() {
                    bg_parts.push(paren_slice);
                }

                current_idx = close_byte + close_len;
                i = close_j + 1;
            } else {
                let paren_slice = text[byte_pos..].trim();
                if !paren_slice.is_empty() {
                    bg_parts.push(paren_slice);
                }

                current_idx = text.len();
                break;
            }
        } else {
            i += 1;
        }
    }

    if current_idx < text.len() {
        let part = text[current_idx..].trim();
        if !part.is_empty() {
            main_parts.push(part);
        }
    }

    let main_text = if main_parts.is_empty() {
        None
    } else {
        let mut stitched = String::new();
        for part in main_parts {
            if stitched.is_empty()
                || part.starts_with(',')
                || part.starts_with('.')
                || part.starts_with(';')
                || part.starts_with(':')
                || part.starts_with('!')
                || part.starts_with('?')
            {
                stitched.push_str(part);
            } else {
                stitched.push(' ');
                stitched.push_str(part);
            }
        }
        let trimmed_main = stitched.trim();
        if trimmed_main.is_empty() {
            None
        } else {
            Some(trimmed_main.to_string())
        }
    };

    let bg_text = if bg_parts.is_empty() {
        None
    } else {
        Some(bg_parts.join(" "))
    };

    (main_text, bg_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrapped_block_styled_lines() {
        let block = WrappedBlock {
            primary: vec!["こんにちは".to_string()],
            background: Vec::new(),
            romanized_primary: vec!["konnichiwa".to_string()],
            romanized_background: Vec::new(),
            inline_lines: vec![vec![InlineSegment {
                text: "こんにちは".to_string(),
                is_background: false,
            }]],
            inline_romanized: vec![vec![InlineSegment {
                text: "konnichiwa".to_string(),
                is_background: false,
            }]],
            bg_first: false,
        };

        assert_eq!(block.len(), 2);
        assert!(!block.is_empty());

        let styles = LyricStyles::default();
        let lines: Vec<Line> = block
            .styled_lines(styles.current, styles.background_current)
            .collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_split_line_main_and_background() {
        let (main, bg) = split_line_main_and_background(
            "'Cause all I need (all I need is love) is a beauty and a beat",
        );
        assert_eq!(
            main.as_deref(),
            Some("'Cause all I need is a beauty and a beat")
        );
        assert_eq!(bg.as_deref(), Some("(all I need is love)"));

        let (main, bg) =
            split_line_main_and_background("Baby, do it like you do (uh, uh), 'cause all");
        assert_eq!(main.as_deref(), Some("Baby, do it like you do, 'cause all"));
        assert_eq!(bg.as_deref(), Some("(uh, uh)"));

        let (main, bg) = split_line_main_and_background("(yeah, yeah, yeah)");
        assert_eq!(main, None);
        assert_eq!(bg.as_deref(), Some("(yeah, yeah, yeah)"));

        let (main, bg) = split_line_main_and_background("In time, ink lines");
        assert_eq!(main.as_deref(), Some("In time, ink lines"));
        assert_eq!(bg, None);
    }

    #[test]
    fn test_split_line_cjk_fullwidth_parentheses() {
        let (main, bg) = split_line_main_and_background("夜空に光る（星のように）");
        assert_eq!(main.as_deref(), Some("夜空に光る"));
        assert_eq!(bg.as_deref(), Some("（星のように）"));

        let (main, bg) = split_line_main_and_background("（今夜だけ）");
        assert_eq!(main, None);
        assert_eq!(bg.as_deref(), Some("（今夜だけ）"));
    }

    #[test]
    fn test_split_line_parentheses_background() {
        let (main, bg) = split_line_main_and_background("Take me home (yeah yeah)");
        assert_eq!(main.as_deref(), Some("Take me home"));
        assert_eq!(bg.as_deref(), Some("(yeah yeah)"));

        let (main, bg) = split_line_main_and_background("I love you (so much)");
        assert_eq!(main.as_deref(), Some("I love you"));
        assert_eq!(bg.as_deref(), Some("(so much)"));

        let (main, bg) = split_line_main_and_background("Don't stop (ooh) (yeah yeah)");
        assert_eq!(main.as_deref(), Some("Don't stop"));
        assert_eq!(bg.as_deref(), Some("(ooh) (yeah yeah)"));
    }

    #[test]
    fn test_split_words_main_and_background_ascii_and_cjk() {
        let words = vec![
            crate::lyrics::types::WordTiming {
                start: 0.0,
                end: 0.5,
                text: "Sing".to_string(),
                grapheme_boundaries: vec![0, 4],
                grapheme_times: vec![],
            },
            crate::lyrics::types::WordTiming {
                start: 0.6,
                end: 1.0,
                text: "(yeah)".to_string(),
                grapheme_boundaries: vec![0, 6],
                grapheme_times: vec![],
            },
            crate::lyrics::types::WordTiming {
                start: 1.1,
                end: 1.5,
                text: "（星のように）".to_string(),
                grapheme_boundaries: vec![0, 21],
                grapheme_times: vec![],
            },
        ];

        let (main, bg) = split_words_main_and_background(&words);
        assert_eq!(main.len(), 1);
        assert_eq!(main[0].text, "Sing");

        assert_eq!(bg.len(), 2);
        assert_eq!(bg[0].text, "(yeah)");
        assert_eq!(bg[1].text, "（星のように）");
    }

    #[test]
    fn test_wrap_inline_segments_and_styled_lines_inline() {
        let text = "'Cause all I need (all I need is love) is a beat";
        let segments = wrap_inline_segments(text, 80);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].len(), 3);
        assert_eq!(segments[0][0].text, "'Cause all I need ");
        assert!(!segments[0][0].is_background);
        assert_eq!(segments[0][1].text, "(all I need is love)");
        assert!(segments[0][1].is_background);
        assert_eq!(segments[0][2].text, " is a beat");
        assert!(!segments[0][2].is_background);

        let block = WrappedBlock {
            primary: vec!["'Cause all I need is a beat".to_string()],
            background: vec!["(all I need is love)".to_string()],
            romanized_primary: Vec::new(),
            romanized_background: Vec::new(),
            inline_lines: segments,
            inline_romanized: Vec::new(),
            bg_first: false,
        };

        let styles = LyricStyles::default();
        let rendered: Vec<Line> = block
            .styled_lines_inline(styles.after, styles.background_after)
            .collect();
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].spans.len(), 3);
        assert_eq!(rendered[0].spans[0].content, "'Cause all I need ");
        assert_eq!(rendered[0].spans[0].style, styles.after);
        assert_eq!(rendered[0].spans[1].content, "(all I need is love)");
        assert_eq!(rendered[0].spans[1].style, styles.background_after);
        assert_eq!(rendered[0].spans[2].content, " is a beat");
        assert_eq!(rendered[0].spans[2].style, styles.after);
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
        assert!(blocks[0].background.is_empty());
        assert!(blocks[0].romanized_primary.is_empty());
        assert!(blocks[0].romanized_background.is_empty());

        // Romanize enabled (cache invalidates and rebuilds)
        let blocks = ensure_wrapped_cache(&mut cache, &lines, 80, true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].primary.len(), 1);
        assert!(blocks[0].background.is_empty());
        assert_eq!(blocks[0].romanized_primary.len(), 1);
        assert_eq!(blocks[0].romanized_primary[0], "konnichiwa");
        assert!(blocks[0].romanized_background.is_empty());
    }

    #[test]
    fn test_ensure_wrapped_cache_romanization_with_background_vocals() {
        let mut cache = None;
        let lines = vec![
            crate::lyrics::LyricLine::new(0.0, "夜空に光る（星のように）")
                .with_romanized(Some("yozora ni hikaru (hoshi no you ni)".to_string())),
        ];

        let blocks = ensure_wrapped_cache(&mut cache, &lines, 80, true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].primary, vec!["夜空に光る"]);
        assert_eq!(blocks[0].background, vec!["（星のように）"]);
        assert_eq!(blocks[0].romanized_primary, vec!["yozora ni hikaru"]);
        assert_eq!(blocks[0].romanized_background, vec!["(hoshi no you ni)"]);
    }

    #[test]
    fn test_split_words_into_lines_unicode_width() {
        // CJK characters have display width 2 each
        let w1 = crate::lyrics::types::WordTiming {
            start: 0.0,
            end: 0.5,
            text: "日本語".to_string(), // width = 6
            grapheme_boundaries: vec![0, 3, 6, 9],
            grapheme_times: vec![],
        };
        let w2 = crate::lyrics::types::WordTiming {
            start: 0.5,
            end: 1.0,
            text: "東京".to_string(), // width = 4
            grapheme_boundaries: vec![0, 3, 6],
            grapheme_times: vec![],
        };

        // width 8: w1 (6) fits, candidate for w2 is 6 + 1 + 4 = 11 > 8, so it wraps to line 2!
        let lines = split_words_into_lines(&[&w1, &w2], 8);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 1);
        assert_eq!(lines[0][0].text, "日本語");
        assert_eq!(lines[1].len(), 1);
        assert_eq!(lines[1][0].text, "東京");

        // width 12: 6 + 1 + 4 = 11 <= 12, so both fit on one line!
        let lines = split_words_into_lines(&[&w1, &w2], 12);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 2);
    }

    #[test]
    fn test_karaoke_word_spans_with_background_vocals() {
        let styles = LyricStyles::default();
        let words = vec![
            crate::lyrics::types::WordTiming {
                start: 0.0,
                end: 0.4,
                text: "you".to_string(),
                grapheme_boundaries: vec![0, 3],
                grapheme_times: vec![],
            },
            crate::lyrics::types::WordTiming {
                start: 1.2,
                end: 1.6,
                text: "(all".to_string(),
                grapheme_boundaries: vec![0, 1, 2, 3, 4],
                grapheme_times: vec![],
            },
            crate::lyrics::types::WordTiming {
                start: 2.0,
                end: 2.5,
                text: "you)".to_string(),
                grapheme_boundaries: vec![0, 1, 2, 3, 4],
                grapheme_times: vec![],
            },
        ];

        let line = crate::lyrics::LyricLine::new(0.0, "you (all you)").with_words(Some(words));
        let update = Update {
            lines: std::sync::Arc::new(vec![line]),
            index: Some(0),
            position: 1.7,
            playing: true,
            provider: Some(crate::state::Provider::MusixmatchRichsync),
            ..Default::default()
        };

        // At position 0.5s (before background vocal starts at 1.5s):
        // Main line + Background sub-line are both shown immediately (length 2)
        // Main line: "you" is active -> styles.current (Green)
        // Background sub-line: "(all" and "you)" have not started yet -> styles.background_after (Dim Italic)
        let lines_early =
            try_build_karaoke_spans(&update, 0, 80, &styles, 0.5).expect("should build spans");
        assert_eq!(lines_early.len(), 2);
        assert_eq!(lines_early[0].spans[0].content, "you");
        assert_eq!(lines_early[0].spans[0].style, styles.current);
        assert_eq!(lines_early[1].spans[0].content, "(all ");
        assert_eq!(lines_early[1].spans[0].style, styles.background_after);
        assert_eq!(lines_early[1].spans[1].content, "you)");
        assert_eq!(lines_early[1].spans[1].style, styles.background_after);

        // At position 1.7s (after background vocal started at 1.5s):
        // Main line + Background sub-line are shown (length 2)
        // Main line: "you" has passed -> styles.current (Green)
        // Background sub-line: "(all" is active (1.5s to 2.0s) -> styles.background_current (Cyan)
        // "you)" has not been reached -> styles.background_after (Dim Italic)
        let lines =
            try_build_karaoke_spans(&update, 0, 80, &styles, 1.7).expect("should build spans");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].content, "you");
        assert_eq!(lines[0].spans[0].style, styles.current);
        assert_eq!(lines[1].spans[0].content, "(all ");
        assert_eq!(lines[1].spans[0].style, styles.background_current);
        assert_eq!(lines[1].spans[1].content, "you)");
        assert_eq!(lines[1].spans[1].style, styles.background_after);
    }

    #[test]
    fn test_single_grapheme_word_highlighting_active() {
        let styles = LyricStyles::default();
        let word = crate::lyrics::types::WordTiming {
            start: 1.0,
            end: 3.0,
            text: "I".to_string(),
            grapheme_boundaries: vec![0, 1],
            grapheme_times: vec![],
        };

        // Before start: unhighlighted
        let spans_before = build_word_spans(&word, 0.5, &styles, true, false);
        assert_eq!(spans_before.len(), 1);
        assert_eq!(spans_before[0].content, "I ");
        assert_eq!(spans_before[0].style, styles.after);

        // While active (1.0 <= position < 3.0): MUST BE HIGHLIGHTED!
        let spans_active = build_word_spans(&word, 1.5, &styles, true, false);
        assert_eq!(spans_active.len(), 1);
        assert_eq!(spans_active[0].content, "I ");
        assert_eq!(spans_active[0].style, styles.current);

        // After end: highlighted
        let spans_after = build_word_spans(&word, 3.5, &styles, true, false);
        assert_eq!(spans_after.len(), 1);
        assert_eq!(spans_after[0].content, "I ");
        assert_eq!(spans_after[0].style, styles.current);
    }

    #[test]
    fn test_two_grapheme_immediate_feedback() {
        let styles = LyricStyles::default();
        let word = crate::lyrics::types::WordTiming {
            start: 1.0,
            end: 2.0,
            text: "to".to_string(),
            grapheme_boundaries: vec![0, 1, 2],
            grapheme_times: vec![1.5],
        };

        // At exact start (position = 1.0), the first grapheme 't' should highlight immediately
        let spans = build_word_spans(&word, 1.0, &styles, false, false);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "t");
        assert_eq!(spans[0].style, styles.current);
        assert_eq!(spans[1].content, "o");
        assert_eq!(spans[1].style, styles.after);
    }

    #[test]
    fn test_cjk_word_spacing_omission() {
        let styles = LyricStyles::default();
        assert!(!should_add_space_between("夜空", "に"));
        assert!(!should_add_space_between("こんにちは", "世界"));
        assert!(should_add_space_between("Hello", "world"));
        assert!(should_add_space_between("Hello", "世界"));

        let w1 = crate::lyrics::types::WordTiming {
            start: 0.0,
            end: 0.5,
            text: "夜空".to_string(),
            grapheme_boundaries: vec![0, 3, 6],
            grapheme_times: vec![0.25],
        };
        let w2 = crate::lyrics::types::WordTiming {
            start: 0.5,
            end: 1.0,
            text: "に".to_string(),
            grapheme_boundaries: vec![0, 3],
            grapheme_times: vec![],
        };

        let spans = build_word_line_spans(&[&w1, &w2], 0.25, &styles, false);
        // w1 ("夜空") should not have a trailing space before w2 ("に")
        let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rendered, "夜空に");
    }

    #[test]
    fn test_square_bracket_and_nested_parentheses_parsing() {
        // Square bracket backing vocal
        let (main, bg) = split_line_main_and_background("Yeah [Backing: ooh ooh]");
        assert_eq!(main.as_deref(), Some("Yeah"));
        assert_eq!(bg.as_deref(), Some("[Backing: ooh ooh]"));

        // Japanese bracket backing vocal
        let (main, bg) = split_line_main_and_background("夜空【星のように】");
        assert_eq!(main.as_deref(), Some("夜空"));
        assert_eq!(bg.as_deref(), Some("【星のように】"));

        // Nested parentheses
        let (main, bg) = split_line_main_and_background("Don't leave (say (goodbye)) now");
        assert_eq!(main.as_deref(), Some("Don't leave now"));
        assert_eq!(bg.as_deref(), Some("(say (goodbye))"));
    }

    #[test]
    fn test_sung_word_highlighting_break_and_common_words() {
        let styles = LyricStyles::default();
        let break_word = crate::lyrics::types::WordTiming {
            start: 1.0,
            end: 2.0,
            text: "break".to_string(),
            grapheme_boundaries: vec![0, 1, 2, 3, 4, 5],
            grapheme_times: vec![],
        };

        // Before start: unhighlighted (after style)
        let spans_before = build_word_spans(&break_word, 0.5, &styles, false, false);
        assert_eq!(spans_before[0].style, styles.after);

        // While active: highlighted!
        let spans_active = build_word_spans(&break_word, 1.5, &styles, false, false);
        assert_eq!(spans_active[0].style, styles.current);

        // While passed: highlighted!
        let spans_passed = build_word_spans(&break_word, 2.5, &styles, false, false);
        assert_eq!(spans_passed[0].style, styles.current);

        // In background vocals: uses background_current when active
        let bg_spans_active = build_word_spans(&break_word, 1.5, &styles, false, true);
        assert_eq!(bg_spans_active[0].style, styles.background_current);
    }

    #[test]
    fn test_viewport_block_height_invariance() {
        let block = WrappedBlock {
            primary: vec!["Lead vocal line".to_string()],
            background: vec!["(Backing vocal harmony)".to_string()],
            romanized_primary: Vec::new(),
            romanized_background: Vec::new(),
            inline_lines: Vec::new(),
            inline_romanized: Vec::new(),
            bg_first: false,
        };

        let styles = LyricStyles::default();
        // Inactive before: 2 lines
        let before_lines: Vec<Line> = block
            .styled_lines(styles.before, styles.background_after)
            .collect();
        assert_eq!(before_lines.len(), 2);

        // Inactive after: 2 lines
        let after_lines: Vec<Line> = block
            .styled_lines(styles.after, styles.background_after)
            .collect();
        assert_eq!(after_lines.len(), 2);

        // Block height matches across all states
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn test_chronological_background_first_ordering() {
        let styles = LyricStyles::default();

        // 1. Line string begins with background vocal: "(Look out for yourself) but I'm ready"
        assert!(is_line_background_first(
            "(Look out for yourself) but I'm ready"
        ));
        assert!(!is_line_background_first(
            "but I'm ready (Look out for yourself)"
        ));

        // 2. Richsync word timings: background words start earlier than main words
        let bg_word = crate::lyrics::types::WordTiming {
            start: 54.16,
            end: 54.30,
            text: "(Look)".to_string(),
            grapheme_boundaries: vec![0, 6],
            grapheme_times: vec![],
        };
        let main_word = crate::lyrics::types::WordTiming {
            start: 54.31,
            end: 54.49,
            text: "ready".to_string(),
            grapheme_boundaries: vec![0, 5],
            grapheme_times: vec![],
        };
        assert!(determine_background_first(
            "(Look out for yourself) but I'm ready",
            Some(&[bg_word.clone(), main_word.clone()])
        ));
        let later_bg_word = crate::lyrics::types::WordTiming {
            start: 54.50,
            end: 54.70,
            text: "(free)".to_string(),
            grapheme_boundaries: vec![0, 6],
            grapheme_times: vec![],
        };
        assert!(!determine_background_first(
            "but I'm ready (free)",
            Some(&[main_word.clone(), later_bg_word])
        ));

        // 3. WrappedBlock styling: background appears on line 0, primary on line 1
        let block = WrappedBlock {
            primary: vec!["but I'm ready".to_string()],
            background: vec!["(Look out for yourself)".to_string()],
            romanized_primary: Vec::new(),
            romanized_background: Vec::new(),
            inline_lines: Vec::new(),
            inline_romanized: Vec::new(),
            bg_first: true,
        };

        let rendered: Vec<Line> = block
            .styled_lines(styles.current, styles.background_current)
            .collect();
        assert_eq!(rendered.len(), 2);
        assert_eq!(rendered[0].spans[0].content, "(Look out for yourself)");
        assert_eq!(rendered[0].spans[0].style, styles.background_current);
        assert_eq!(rendered[1].spans[0].content, "but I'm ready");
        assert_eq!(rendered[1].spans[0].style, styles.current);

        // 4. Karaoke spans ordering: background line is index 0, main line is index 1
        let line = crate::lyrics::LyricLine::new(54.16, "(Look out for yourself) but I'm ready")
            .with_words(Some(vec![bg_word, main_word]));
        let update = Update {
            lines: std::sync::Arc::new(vec![line]),
            index: Some(0),
            position: 54.20,
            playing: true,
            provider: Some(crate::state::Provider::MusixmatchRichsync),
            ..Default::default()
        };

        let spans = try_build_karaoke_spans(&update, 0, 80, &styles, 54.20)
            .expect("should build karaoke spans");
        assert_eq!(spans.len(), 2);
        // First line is background vocal, currently active (cyan)
        assert_eq!(spans[0].spans[0].content, "(Look)");
        assert_eq!(spans[0].spans[0].style, styles.background_current);
        // Second line is lead vocal, not yet reached (dim)
        assert_eq!(spans[1].spans[0].content, "ready");
        assert_eq!(spans[1].spans[0].style, styles.after);
    }
}
