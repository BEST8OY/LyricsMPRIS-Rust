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
    if let Some(words) = words
        && !words.is_empty()
    {
        let mut first_main_start = None;
        let mut first_bg_start = None;
        let mut in_bg = false;

        for word in words {
            let has_open = word.text.chars().any(is_open_paren);
            let has_close = word.text.chars().any(is_close_paren);

            if has_open {
                in_bg = true;
            }

            if in_bg {
                if first_bg_start.is_none() {
                    first_bg_start = Some(word.start);
                }
            } else if first_main_start.is_none() {
                first_main_start = Some(word.start);
            }

            if has_close {
                in_bg = false;
            }

            if first_main_start.is_some() && first_bg_start.is_some() {
                break;
            }
        }

        if let (Some(first_bg), Some(first_main)) = (first_bg_start, first_main_start)
            && (first_bg - first_main).abs() > f64::EPSILON
        {
            return first_bg < first_main;
        }
    }
    is_line_background_first(line_text)
}

/// A wrapped lyric line block containing primary text lines and background vocal lines.
#[derive(Clone, Debug, PartialEq)]
pub struct WrappedBlock {
    pub primary: Vec<String>,
    pub background: Vec<String>,
    /// Whether background vocals precede primary vocals chronologically
    pub bg_first: bool,
}

impl WrappedBlock {
    pub fn len(&self) -> usize {
        self.primary.len() + self.background.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.primary.is_empty() && self.background.is_empty()
    }

    pub fn styled_lines<'a>(
        &'a self,
        style: ratatui::style::Style,
        bg_style: ratatui::style::Style,
    ) -> impl Iterator<Item = Line<'a>> {
        let mut lines = Vec::with_capacity(self.len());
        if self.bg_first {
            for l in &self.background {
                lines.push(Line::from(Span::styled(l.as_str(), bg_style)));
            }
            for l in &self.primary {
                lines.push(Line::from(Span::styled(l.as_str(), style)));
            }
        } else {
            for l in &self.primary {
                lines.push(Line::from(Span::styled(l.as_str(), style)));
            }
            for l in &self.background {
                lines.push(Line::from(Span::styled(l.as_str(), bg_style)));
            }
        }

        lines.into_iter()
    }
}

/// Return the index of the parent `!is_bg` line for `idx` (rewinding past `is_bg` siblings).
pub fn parent_group_start(lines: &[crate::lyrics::LyricLine], idx: usize) -> usize {
    if lines.is_empty() {
        return 0;
    }
    let mut cur = idx.min(lines.len() - 1);
    while cur > 0 && lines[cur].is_bg {
        cur -= 1;
    }
    if lines[cur].is_bg {
        lines.iter().position(|l| !l.is_bg).unwrap_or(0)
    } else {
        cur
    }
}

/// Return the exclusive end index after `group_start` and all contiguous trailing `is_bg` lines.
pub fn group_end(lines: &[crate::lyrics::LyricLine], group_start: usize) -> usize {
    let mut end = (group_start + 1).min(lines.len());
    while lines.get(end).is_some_and(|l| l.is_bg) {
        end += 1;
    }
    end
}

/// Resolve the paused manual scroll index, snapping off standalone `is_bg` lines onto their parent.
pub fn resolve_scrolled_index(
    lines: &[crate::lyrics::LyricLine],
    base_index: usize,
    scroll_offset: isize,
) -> usize {
    if lines.is_empty() {
        return 0;
    }
    let raw = (base_index as isize + scroll_offset)
        .max(0)
        .min(lines.len().saturating_sub(1) as isize) as usize;
    parent_group_start(lines, raw)
}

/// Step manual scroll offset by `delta` primary lyric groups (skipping `is_bg` lines).
pub fn step_scroll_offset(
    lines: &[crate::lyrics::LyricLine],
    base_index: usize,
    current_offset: isize,
    delta: isize,
) -> isize {
    if lines.is_empty() {
        return current_offset;
    }
    let primary_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| (!l.is_bg).then_some(i))
        .collect();
    if primary_indices.is_empty() {
        let cur =
            (base_index as isize + current_offset).clamp(0, lines.len().saturating_sub(1) as isize);
        return (cur + delta).clamp(0, lines.len().saturating_sub(1) as isize)
            - base_index as isize;
    }
    let cur = resolve_scrolled_index(lines, base_index, current_offset);
    let pos = primary_indices
        .binary_search(&cur)
        .unwrap_or_else(|ins| ins.saturating_sub(1));
    let target_pos = if delta < 0 {
        pos.saturating_sub(delta.unsigned_abs())
    } else {
        pos.saturating_add(delta as usize)
            .min(primary_indices.len() - 1)
    };
    primary_indices[target_pos] as isize - base_index as isize
}

/// Compute scroll offset targeting the first (`to_end = false`) or last (`to_end = true`) primary line.
pub fn edge_scroll_offset(
    lines: &[crate::lyrics::LyricLine],
    base_index: usize,
    to_end: bool,
) -> isize {
    if lines.is_empty() {
        return 0;
    }
    let target = if to_end {
        lines
            .iter()
            .rposition(|l| !l.is_bg)
            .unwrap_or(lines.len() - 1)
    } else {
        lines.iter().position(|l| !l.is_bg).unwrap_or(0)
    };
    target as isize - base_index as isize
}

fn collect_group_ranges(
    lines: &[crate::lyrics::LyricLine],
    start: usize,
    end: usize,
) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut i = start;
    while i < end {
        if lines.get(i).is_some_and(|l| l.is_bg) {
            groups.push((i, i + 1));
            i += 1;
        } else {
            let mut g_end = i + 1;
            while g_end < end && lines.get(g_end).is_some_and(|l| l.is_bg) {
                g_end += 1;
            }
            groups.push((i, g_end));
            i = g_end;
        }
    }
    groups
}

/// Draw the UI using cached wrapped lines.
///
/// This function handles:
/// - Error message rendering
/// - Wrapped text caching (invalidated on width change)
/// - Visible line computation with context
/// - Vertical centering
#[allow(clippy::too_many_arguments)]
pub fn draw_ui_with_cache<B: Backend>(
    terminal: &mut Terminal<B>,
    last_update: &Option<Update>,
    wrapped_cache: &mut Option<(usize, Vec<WrappedBlock>)>,
    styles: &LyricStyles,
    karaoke_enabled: bool,
    split_background: bool,
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
                split_background,
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
    wrapped_cache: &'a mut Option<(usize, Vec<WrappedBlock>)>,
    width: usize,
    height: usize,
    styles: &'a LyricStyles,
    karaoke_enabled: bool,
    split_background: bool,
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
    if update.lines.is_empty() || update.index.is_some_and(|i| i >= update.lines.len()) {
        return Vec::new();
    }

    let blocks = ensure_wrapped_cache(wrapped_cache, &update.lines, width, split_background);
    let visible = gather_visible_lines_with_split(
        update,
        blocks,
        width,
        height,
        styles,
        update.position,
        karaoke_enabled,
        split_background,
        max_visible_lines,
        scroll_offset,
    );

    visible.into_vec()
}

/// Ensure wrapped cache is valid for current width and line count.
/// Returns a reference to the cached blocks.
fn ensure_wrapped_cache<'a>(
    wrapped_cache: &'a mut Option<(usize, Vec<WrappedBlock>)>,
    lines: &[crate::lyrics::LyricLine],
    width: usize,
    split_background: bool,
) -> &'a Vec<WrappedBlock> {
    let needs_rebuild = match wrapped_cache {
        Some((cached_w, blocks)) => *cached_w != width || blocks.len() != lines.len(),
        None => true,
    };

    if needs_rebuild {
        let new_blocks: Vec<WrappedBlock> = lines
            .iter()
            .map(|l| {
                if l.is_bg {
                    return WrappedBlock {
                        primary: Vec::new(),
                        background: wrap_text(&l.text, width),
                        bg_first: true,
                    };
                }
                if !split_background {
                    return WrappedBlock {
                        primary: wrap_text(&l.text, width),
                        background: Vec::new(),
                        bg_first: false,
                    };
                }
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

                let bg_first = determine_background_first(&l.text, l.words.as_deref());

                WrappedBlock {
                    primary,
                    background,
                    bg_first,
                }
            })
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

fn group_bg_first(lines: &[crate::lyrics::LyricLine], g_start: usize, g_end: usize) -> bool {
    if g_end <= g_start + 1 {
        return false;
    }
    let Some(main_line) = lines.get(g_start) else {
        return false;
    };
    if main_line.is_bg {
        return false;
    }
    let main_start = main_line.vocal_start();
    lines[g_start + 1..g_end]
        .iter()
        .filter(|l| l.is_bg)
        .map(crate::lyrics::LyricLine::vocal_start)
        .min_by(f64::total_cmp)
        .is_some_and(|bg_start| bg_start + f64::EPSILON < main_start)
}

fn is_line_actively_singing(line: &crate::lyrics::LyricLine, position: f64) -> bool {
    if !position.is_finite() {
        return false;
    }
    let start = line.vocal_start();
    let Some(end) = line.vocal_end() else {
        return false;
    };
    start.is_finite() && end > start && position >= start && position < end
}

fn append_karaoke_split_lines<'a>(
    out: &mut Vec<Line<'a>>,
    bg_first: bool,
    main_spans: Option<Vec<Line<'a>>>,
    bg_spans: Option<Vec<Line<'a>>>,
) {
    if bg_first {
        if let Some(bs) = bg_spans {
            out.extend(bs);
        }
        if let Some(ms) = main_spans {
            out.extend(ms);
        }
    } else {
        if let Some(ms) = main_spans {
            out.extend(ms);
        }
        if let Some(bs) = bg_spans {
            out.extend(bs);
        }
    }
}

struct ContextRenderCtx<'a> {
    update: &'a Update,
    wrapped_blocks: &'a [WrappedBlock],
    width: usize,
    styles: &'a LyricStyles,
    position: f64,
    use_karaoke: bool,
    allow_overlap_active: bool,
    split_background: bool,
}

fn render_group_block_lines<'a>(
    ctx: &ContextRenderCtx<'a>,
    g_start: usize,
    g_end: usize,
    dist: usize,
    is_before: bool,
) -> Vec<Line<'a>> {
    let style = ctx.styles.get_line_style_by_distance(dist, is_before);
    let bg_style = ctx.styles.get_background_style_by_distance(dist, is_before);
    let ordered: Vec<usize> = if group_bg_first(&ctx.update.lines, g_start, g_end) {
        ((g_start + 1)..g_end)
            .chain(std::iter::once(g_start))
            .collect()
    } else {
        (g_start..g_end).collect()
    };

    let mut out = Vec::new();
    for idx in ordered {
        let block = &ctx.wrapped_blocks[idx];
        let line_opt = ctx.update.lines.get(idx);
        let overlap_active = ctx.allow_overlap_active
            && line_opt.is_some_and(|l| is_line_actively_singing(l, ctx.position));

        if overlap_active {
            let line = line_opt.unwrap();
            let line_has_words = ctx
                .update
                .provider
                .as_ref()
                .is_some_and(crate::state::Provider::is_word_synced)
                || line.has_word_timings();
            if ctx.use_karaoke
                && line_has_words
                && let Some((main_spans, bg_spans)) = try_build_karaoke_spans_split(
                    ctx.update,
                    idx,
                    ctx.width,
                    ctx.styles,
                    ctx.position,
                    ctx.split_background,
                )
            {
                append_karaoke_split_lines(&mut out, block.bg_first, main_spans, bg_spans);
                continue;
            }
            let (active_style, active_bg_style) = if line.is_bg {
                (ctx.styles.background_current, ctx.styles.background_current)
            } else {
                (ctx.styles.current, ctx.styles.background_current)
            };
            out.extend(block.styled_lines(active_style, active_bg_style));
        } else {
            out.extend(block.styled_lines(style, bg_style));
        }
    }
    out
}

/// Collect lines before the current group. Returns Lines in visual top->down order.
fn collect_before_spans<'a>(
    ctx: &ContextRenderCtx<'a>,
    before_groups: &[(usize, usize)],
    mut lines_needed: usize,
) -> Vec<Line<'a>> {
    let mut chunks: Vec<Vec<Line<'a>>> = Vec::new();

    for (idx_from_end, &(g_start, g_end)) in before_groups.iter().rev().enumerate() {
        if lines_needed == 0 {
            break;
        }
        let dist = idx_from_end + 1;
        let mut group_lines = render_group_block_lines(ctx, g_start, g_end, dist, true);
        let group_len = group_lines.len();
        let take = group_len.min(lines_needed);
        let skip = group_len - take;
        chunks.push(group_lines.drain(skip..).collect());
        lines_needed -= take;
    }

    chunks.reverse();
    chunks.into_iter().flatten().collect()
}

/// Collect lines after the current group. Returns Lines in visual top->down order.
fn collect_after_spans<'a>(
    ctx: &ContextRenderCtx<'a>,
    after_groups: &[(usize, usize)],
    mut lines_needed: usize,
) -> Vec<Line<'a>> {
    let mut result = Vec::new();

    for (k, &(g_start, g_end)) in after_groups.iter().enumerate() {
        if lines_needed == 0 {
            break;
        }
        let dist = k + 1;
        for line in render_group_block_lines(ctx, g_start, g_end, dist, false)
            .into_iter()
            .take(lines_needed)
        {
            result.push(line);
            lines_needed -= 1;
        }
    }
    result
}

/// Collect complete logical lyric groups before the current group (for max_visible_lines mode).
/// Returns all wrapped lines from each group in visual top->down order.
fn collect_before_groups<'a>(
    ctx: &ContextRenderCtx<'a>,
    before_groups: &[(usize, usize)],
    groups_needed: usize,
) -> Vec<Line<'a>> {
    let mut result = Vec::new();
    let start = before_groups.len().saturating_sub(groups_needed);
    let selected = &before_groups[start..];

    for (offset, &(g_start, g_end)) in selected.iter().enumerate() {
        let dist = selected.len() - offset;
        result.extend(render_group_block_lines(ctx, g_start, g_end, dist, true));
    }

    result
}

/// Collect complete logical lyric groups after the current group (for max_visible_lines mode).
/// Returns all wrapped lines from each group in visual top->down order.
fn collect_after_groups<'a>(
    ctx: &ContextRenderCtx<'a>,
    after_groups: &[(usize, usize)],
    groups_needed: usize,
) -> Vec<Line<'a>> {
    let mut result = Vec::new();
    let take = groups_needed.min(after_groups.len());

    for (offset, &(g_start, g_end)) in after_groups[..take].iter().enumerate() {
        let dist = offset + 1;
        result.extend(render_group_block_lines(ctx, g_start, g_end, dist, false));
    }

    result
}

/// Split a slice of WordTiming into visual lines that fit into `width` terminal columns.
#[cfg_attr(not(test), allow(dead_code))]
fn split_words_into_lines<'b>(
    words: &[&'b crate::lyrics::types::WordTiming],
    width: usize,
) -> Vec<Vec<&'b crate::lyrics::types::WordTiming>> {
    split_words_into_lines_mode(words, width, false)
}

fn line_uses_explicit_spaces(words: &[crate::lyrics::types::WordTiming], line_text: &str) -> bool {
    words.iter().take(words.len().saturating_sub(1)).any(|w| {
        w.ends_with_space
            || w.text.chars().next_back().is_some_and(char::is_whitespace)
            || w.text.chars().next().is_some_and(char::is_whitespace)
    }) || !line_text.chars().any(char::is_whitespace)
}

fn split_words_into_lines_mode<'b>(
    words: &[&'b crate::lyrics::types::WordTiming],
    width: usize,
    use_explicit_spaces: bool,
) -> Vec<Vec<&'b crate::lyrics::types::WordTiming>> {
    let mut lines: Vec<Vec<&'b crate::lyrics::types::WordTiming>> = Vec::new();
    let mut current: Vec<&'b crate::lyrics::types::WordTiming> = Vec::new();
    let mut cur_len: usize = 0;

    for &w in words {
        let wlen = w.text.as_str().width();
        let add_space = if let Some(prev) = current.last() {
            if use_explicit_spaces {
                prev.ends_with_space
            } else {
                should_add_space_between(&prev.text, &w.text)
            }
        } else {
            false
        };
        let space_len = if add_space { 1 } else { 0 };

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
            current.push(w);
            cur_len += space_len + wlen;
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
#[cfg(test)]
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
    gather_visible_lines_with_split(
        update,
        wrapped_blocks,
        w,
        h,
        styles,
        position,
        karaoke_enabled,
        true,
        max_visible_lines,
        scroll_offset,
    )
}

/// Build `VisibleLines` with configurable visual background bracket splitting.
#[allow(clippy::too_many_arguments)]
pub fn gather_visible_lines_with_split<'a>(
    update: &'a Update,
    wrapped_blocks: &'a [WrappedBlock],
    w: usize,
    h: usize,
    styles: &'a LyricStyles,
    position: f64,
    karaoke_enabled: bool,
    split_background: bool,
    max_visible_lines: Option<usize>,
    scroll_offset: isize,
) -> VisibleLines<'a> {
    // Calculate the effective index considering scroll offset when paused
    let base_index = update.index.unwrap_or(0);
    let effective_index = if !update.playing {
        resolve_scrolled_index(&update.lines, base_index, scroll_offset)
            .min(wrapped_blocks.len().saturating_sub(1))
    } else {
        parent_group_start(&update.lines, base_index).min(wrapped_blocks.len().saturating_sub(1))
    };

    let trailing_bg_end = group_end(&update.lines, effective_index).min(wrapped_blocks.len());

    let current_block = wrapped_blocks.get(effective_index);
    let current_height: usize = (effective_index..trailing_bg_end)
        .filter_map(|i| wrapped_blocks.get(i))
        .map(WrappedBlock::len)
        .sum();

    // Build current line spans (with karaoke if applicable, but only when not scrolled)
    let use_karaoke = karaoke_enabled && scroll_offset == 0 && update.playing;
    let main_spans = match current_block {
        Some(block) => build_current_spans(
            update,
            block,
            w,
            styles,
            position,
            use_karaoke,
            split_background,
        ),
        None => Vec::new(),
    };
    let mut bg_spans = Vec::new();
    for bg_idx in (effective_index + 1)..trailing_bg_end {
        if let Some(bg_block) = wrapped_blocks.get(bg_idx) {
            let bg_active = (!update.playing && scroll_offset != 0)
                || update.lines.get(bg_idx).is_some_and(|l| position >= l.time);
            if use_karaoke
                && bg_active
                && let Some((_, Some(bg_karaoke))) = try_build_karaoke_spans_split(
                    update,
                    bg_idx,
                    w,
                    styles,
                    position,
                    split_background,
                )
            {
                bg_spans.extend(bg_karaoke);
            } else {
                let (style, bg_style) = if bg_active {
                    (styles.background_current, styles.background_current)
                } else {
                    (styles.background_after, styles.background_after)
                };
                bg_spans.extend(bg_block.styled_lines(style, bg_style));
            }
        }
    }
    let mut current_spans = Vec::with_capacity(main_spans.len() + bg_spans.len());
    if group_bg_first(&update.lines, effective_index, trailing_bg_end) {
        current_spans.extend(bg_spans);
        current_spans.extend(main_spans);
    } else {
        current_spans.extend(main_spans);
        current_spans.extend(bg_spans);
    }

    // If current block fills or exceeds available terminal height, no context can fit
    if current_height >= h {
        return VisibleLines {
            before: Vec::new(),
            current: current_spans,
            after: Vec::new(),
        };
    }

    let before_groups = collect_group_ranges(&update.lines, 0, effective_index);
    let after_groups = collect_group_ranges(&update.lines, trailing_bg_end, wrapped_blocks.len());

    // Calculate context lines for max_visible_lines
    let (lines_before, lines_after) = if let Some(max) = max_visible_lines {
        // Limit to available context groups constrained by max_visible_lines and remaining screen rows
        let max_context_blocks = max.saturating_sub(1);
        let total_context = max_context_blocks.min(h.saturating_sub(current_height));
        let ideal_before = total_context / 2;
        let ideal_after = total_context - ideal_before;

        let available_before = before_groups.len();
        let available_after = after_groups.len();

        // Dynamically rebalance context if either side is starved at boundaries (e.g. at track start or end)
        let (before_blocks, after_blocks) = if available_before < ideal_before {
            let deficit = ideal_before - available_before;
            let after = (ideal_after + deficit).min(available_after);
            (available_before, after)
        } else if available_after < ideal_after {
            let deficit = ideal_after - available_after;
            let before = (ideal_before + deficit).min(available_before);
            (before, available_after)
        } else {
            (ideal_before, ideal_after)
        };

        (before_blocks, after_blocks)
    } else {
        // Unlimited blocks: fill available screen height with wrapped lines
        let context_lines = h.saturating_sub(current_height);
        let lines_before = context_lines / 2;
        let lines_after = context_lines - lines_before;
        (lines_before, lines_after)
    };

    let ctx = ContextRenderCtx {
        update,
        wrapped_blocks,
        width: w,
        styles,
        position,
        use_karaoke,
        allow_overlap_active: scroll_offset == 0,
        split_background,
    };

    let before = if max_visible_lines.is_some() {
        collect_before_groups(&ctx, &before_groups, lines_before)
    } else {
        collect_before_spans(&ctx, &before_groups, lines_before)
    };

    let after = if max_visible_lines.is_some() {
        collect_after_groups(&ctx, &after_groups, lines_after)
    } else {
        collect_after_spans(&ctx, &after_groups, lines_after)
    };

    VisibleLines {
        before,
        current: current_spans,
        after,
    }
}

/// Build spans for the current line, applying karaoke highlighting if appropriate.
fn build_current_spans<'a>(
    update: &'a Update,
    current_block: &'a WrappedBlock,
    width: usize,
    styles: &'a LyricStyles,
    position: f64,
    karaoke_enabled: bool,
    split_background: bool,
) -> Vec<Line<'a>> {
    // Try to build richsync karaoke spans
    if let Some(idx) = update.index
        && karaoke_enabled
        && (update
            .provider
            .as_ref()
            .is_some_and(crate::state::Provider::is_word_synced)
            || update
                .lines
                .get(idx)
                .is_some_and(crate::lyrics::LyricLine::has_word_timings))
        && let Some((main_spans, bg_spans)) =
            try_build_karaoke_spans_split(update, idx, width, styles, position, split_background)
    {
        let mut lines = Vec::new();
        append_karaoke_split_lines(&mut lines, current_block.bg_first, main_spans, bg_spans);
        lines
    } else {
        let (style, bg_style) = if update.index.is_some() {
            (styles.current, styles.background_current)
        } else {
            (styles.after, styles.background_after)
        };

        current_block.styled_lines(style, bg_style).collect()
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
    split_background: bool,
) -> Option<KaraokeSplitSpans<'a>> {
    let line = update.lines.get(idx)?;
    let words = line.words.as_ref()?;

    let (main_words, bg_words): (
        Vec<&crate::lyrics::types::WordTiming>,
        Vec<&crate::lyrics::types::WordTiming>,
    ) = if line.is_bg {
        (Vec::new(), words.iter().collect())
    } else if !split_background {
        (words.iter().collect(), Vec::new())
    } else {
        split_words_main_and_background(words)
    };

    let use_explicit_spaces = line_uses_explicit_spaces(words, &line.text);
    let main_lines = if !main_words.is_empty() {
        let word_lines = split_words_into_lines_mode(&main_words, width, use_explicit_spaces);
        let mut result = Vec::new();
        for word_line in word_lines {
            let line_spans = build_word_line_spans_mode(
                &word_line,
                position,
                styles,
                false,
                use_explicit_spaces,
            );
            result.push(Line::from(line_spans));
        }
        Some(result)
    } else {
        None
    };

    let bg_lines = if !bg_words.is_empty() {
        let word_lines = split_words_into_lines_mode(&bg_words, width, use_explicit_spaces);
        let mut result = Vec::new();
        for word_line in word_lines {
            let line_spans =
                build_word_line_spans_mode(&word_line, position, styles, true, use_explicit_spaces);
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
        try_build_karaoke_spans_split(update, idx, width, styles, position, true)?;
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
#[cfg_attr(not(test), allow(dead_code))]
fn build_word_line_spans<'a>(
    words: &[&'a crate::lyrics::types::WordTiming],
    position: f64,
    styles: &'a LyricStyles,
    is_background: bool,
) -> Vec<Span<'a>> {
    build_word_line_spans_mode(words, position, styles, is_background, false)
}

fn build_word_line_spans_mode<'a>(
    words: &[&'a crate::lyrics::types::WordTiming],
    position: f64,
    styles: &'a LyricStyles,
    is_background: bool,
    use_explicit_spaces: bool,
) -> Vec<Span<'a>> {
    let mut spans = Vec::new();

    for (i, word) in words.iter().enumerate() {
        let is_last = i + 1 >= words.len();
        let add_space = if is_last {
            false
        } else if use_explicit_spaces {
            word.ends_with_space
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
            if stitched.is_empty() {
                stitched.push_str(part);
            } else {
                let first_ch = part.chars().next().unwrap();
                let is_leading_punct = matches!(
                    first_ch,
                    ',' | '.'
                        | ';'
                        | ':'
                        | '!'
                        | '?'
                        | '，'
                        | '。'
                        | '！'
                        | '？'
                        | '；'
                        | '：'
                        | '、'
                        | ')'
                        | '）'
                        | ']'
                        | '】'
                        | '」'
                );
                if is_leading_punct || !should_add_space_between(&stitched, part) {
                    stitched.push_str(part);
                } else {
                    stitched.push(' ');
                    stitched.push_str(part);
                }
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
            background: vec!["(hello)".to_string()],
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
    fn test_split_line_cjk_punctuation_no_extra_space() {
        // CJK full stop after parenthetical background vocal
        let (main, bg) = split_line_main_and_background("夜空に光る（星のように）。");
        assert_eq!(main.as_deref(), Some("夜空に光る。"));
        assert_eq!(bg.as_deref(), Some("（星のように）"));

        // CJK comma between parts
        let (main, bg) = split_line_main_and_background("光る（星）、そして月");
        assert_eq!(main.as_deref(), Some("光る、そして月"));
        assert_eq!(bg.as_deref(), Some("（星）"));

        // Adjacent CJK characters without punctuation
        let (main, bg) = split_line_main_and_background("夜空（星）光る");
        assert_eq!(main.as_deref(), Some("夜空光る"));
        assert_eq!(bg.as_deref(), Some("（星）"));
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
                ..Default::default()
            },
            crate::lyrics::types::WordTiming {
                start: 0.6,
                end: 1.0,
                text: "(yeah)".to_string(),
                grapheme_boundaries: vec![0, 6],
                grapheme_times: vec![],
                ..Default::default()
            },
            crate::lyrics::types::WordTiming {
                start: 1.1,
                end: 1.5,
                text: "（星のように）".to_string(),
                grapheme_boundaries: vec![0, 21],
                grapheme_times: vec![],
                ..Default::default()
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
    fn test_determine_background_first_zero_alloc() {
        let lead_word = crate::lyrics::types::WordTiming {
            start: 1.0,
            end: 2.0,
            text: "Hello".to_string(),
            grapheme_boundaries: vec![0, 5],
            grapheme_times: vec![],
            ..Default::default()
        };
        let bg_word = crate::lyrics::types::WordTiming {
            start: 0.5,
            end: 1.5,
            text: "(harmony)".to_string(),
            grapheme_boundaries: vec![0, 9],
            grapheme_times: vec![],
            ..Default::default()
        };
        // Background starts earlier: 0.5 < 1.0
        assert!(determine_background_first(
            "Hello (harmony)",
            Some(&[lead_word.clone(), bg_word.clone()])
        ));
        // Reverse order timestamps: lead starts earlier: 0.2 < 0.5
        let lead_earlier = crate::lyrics::types::WordTiming {
            start: 0.2,
            end: 1.0,
            text: "Hello".to_string(),
            grapheme_boundaries: vec![0, 5],
            grapheme_times: vec![],
            ..Default::default()
        };
        assert!(!determine_background_first(
            "Hello (harmony)",
            Some(&[lead_earlier, bg_word])
        ));
    }

    #[test]
    fn test_ensure_wrapped_cache() {
        let mut cache = None;
        let lines = vec![crate::lyrics::LyricLine::new(
            0.0,
            "夜空に光る（星のように）",
        )];

        let blocks = ensure_wrapped_cache(&mut cache, &lines, 80, true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].primary, vec!["夜空に光る"]);
        assert_eq!(blocks[0].background, vec!["（星のように）"]);
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
            ..Default::default()
        };
        let w2 = crate::lyrics::types::WordTiming {
            start: 0.5,
            end: 1.0,
            text: "東京".to_string(), // width = 4
            grapheme_boundaries: vec![0, 3, 6],
            grapheme_times: vec![],
            ..Default::default()
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
                ..Default::default()
            },
            crate::lyrics::types::WordTiming {
                start: 1.2,
                end: 1.6,
                text: "(all".to_string(),
                grapheme_boundaries: vec![0, 1, 2, 3, 4],
                grapheme_times: vec![],
                ..Default::default()
            },
            crate::lyrics::types::WordTiming {
                start: 2.0,
                end: 2.5,
                text: "you)".to_string(),
                grapheme_boundaries: vec![0, 1, 2, 3, 4],
                grapheme_times: vec![],
                ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        let w2 = crate::lyrics::types::WordTiming {
            start: 0.5,
            end: 1.0,
            text: "に".to_string(),
            grapheme_boundaries: vec![0, 3],
            grapheme_times: vec![],
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        let main_word = crate::lyrics::types::WordTiming {
            start: 54.31,
            end: 54.49,
            text: "ready".to_string(),
            grapheme_boundaries: vec![0, 5],
            grapheme_times: vec![],
            ..Default::default()
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
            ..Default::default()
        };
        assert!(!determine_background_first(
            "but I'm ready (free)",
            Some(&[main_word.clone(), later_bg_word])
        ));

        // 3. WrappedBlock styling: background appears on line 0, primary on line 1
        let block = WrappedBlock {
            primary: vec!["but I'm ready".to_string()],
            background: vec!["(Look out for yourself)".to_string()],
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

    #[test]
    fn test_gather_visible_lines_max_visible_lines_boundary_rebalancing() {
        let styles = LyricStyles::default();
        let lines: Vec<crate::lyrics::LyricLine> = (0..10)
            .map(|i| crate::lyrics::LyricLine::new(i as f64 * 5.0, format!("Line {i}")))
            .collect();
        let blocks: Vec<WrappedBlock> = lines
            .iter()
            .map(|l| WrappedBlock {
                primary: vec![l.text.clone()],
                background: Vec::new(),
                bg_first: false,
            })
            .collect();

        // 1. At start of track (index 0): rebalances to 0 before, 1 current, 3 after = 4 lines
        let mut update = Update {
            lines: std::sync::Arc::new(lines.clone()),
            index: Some(0),
            position: 0.0,
            playing: true,
            ..Default::default()
        };
        let visible =
            gather_visible_lines(&update, &blocks, 80, 24, &styles, 0.0, false, Some(4), 0);
        let total = visible.before.len() + visible.current.len() + visible.after.len();
        assert_eq!(total, 4);
        assert_eq!(visible.before.len(), 0);
        assert_eq!(visible.current.len(), 1);
        assert_eq!(visible.after.len(), 3);

        // 2. In middle of track (index 2): 1 before, 1 current, 2 after = 4 lines
        update.index = Some(2);
        update.position = 10.0;
        let visible =
            gather_visible_lines(&update, &blocks, 80, 24, &styles, 10.0, false, Some(4), 0);
        let total = visible.before.len() + visible.current.len() + visible.after.len();
        assert_eq!(total, 4);
        assert_eq!(visible.before.len(), 1);
        assert_eq!(visible.current.len(), 1);
        assert_eq!(visible.after.len(), 2);

        // 3. Near end of track (index 8 = second to last): 2 before, 1 current, 1 after = 4 lines
        update.index = Some(8);
        update.position = 40.0;
        let visible =
            gather_visible_lines(&update, &blocks, 80, 24, &styles, 40.0, false, Some(4), 0);
        let total = visible.before.len() + visible.current.len() + visible.after.len();
        assert_eq!(total, 4);
        assert_eq!(visible.before.len(), 2);
        assert_eq!(visible.current.len(), 1);
        assert_eq!(visible.after.len(), 1);

        // 4. At very last line (index 9): 3 before, 1 current, 0 after = 4 lines
        update.index = Some(9);
        update.position = 45.0;
        let visible =
            gather_visible_lines(&update, &blocks, 80, 24, &styles, 45.0, false, Some(4), 0);
        let total = visible.before.len() + visible.current.len() + visible.after.len();
        assert_eq!(total, 4);
        assert_eq!(visible.before.len(), 3);
        assert_eq!(visible.current.len(), 1);
        assert_eq!(visible.after.len(), 0);
    }

    #[test]
    fn test_gather_visible_lines_multiline_block_with_max_visible_lines() {
        let styles = LyricStyles::default();
        // Create 5 blocks, where block 2 has 3 lines (e.g. wrapped primary + background)
        let mut blocks: Vec<WrappedBlock> = (0..5)
            .map(|i| WrappedBlock {
                primary: vec![format!("Line {i}")],
                background: Vec::new(),
                bg_first: false,
            })
            .collect();

        blocks[2] = WrappedBlock {
            primary: vec!["Line 2 part 1".to_string(), "Line 2 part 2".to_string()],
            background: vec!["(Backing)".to_string()],
            bg_first: false,
        };
        assert_eq!(blocks[2].len(), 3); // 3 screen lines tall

        let lines: Vec<crate::lyrics::LyricLine> = (0..5)
            .map(|i| crate::lyrics::LyricLine::new(i as f64 * 5.0, format!("Line {i}")))
            .collect();

        let update = Update {
            lines: std::sync::Arc::new(lines),
            index: Some(2),
            position: 10.0,
            playing: true,
            ..Default::default()
        };

        // On a 30-line terminal with max_visible_lines = Some(3),
        // previously current_height (3) >= available_height (3) incorrectly suppressed all context!
        // Now it must show 1 block before, 1 current block (3 lines), and 1 block after!
        let visible =
            gather_visible_lines(&update, &blocks, 80, 30, &styles, 10.0, false, Some(3), 0);
        assert_eq!(visible.before.len(), 1);
        assert_eq!(visible.current.len(), 3);
        assert_eq!(visible.after.len(), 1);
    }

    #[test]
    fn test_syllable_word_spacing_no_bogus_spaces() {
        let styles = LyricStyles::default();
        // Multi-word line split into Latin syllables using `ends_with_space`
        let words = vec![
            crate::lyrics::types::create_word_timing(0.0, 0.3, "Hel"),
            crate::lyrics::types::create_word_timing(0.3, 0.6, "lo").with_space(true),
            crate::lyrics::types::create_word_timing(0.6, 0.9, "wor"),
            crate::lyrics::types::create_word_timing(0.9, 1.2, "ld"),
        ];
        let line = crate::lyrics::LyricLine::new(0.0, "Hello world").with_words(Some(words));
        let update = Update {
            lines: std::sync::Arc::new(vec![line]),
            index: Some(0),
            position: 1.5,
            playing: true,
            ..Default::default()
        };
        let spans =
            try_build_karaoke_spans(&update, 0, 80, &styles, 1.5).expect("should build spans");
        let rendered: String = spans[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rendered, "Hello world");
    }

    #[test]
    fn test_trailing_is_bg_line_highlights_with_parent() {
        let styles = LyricStyles::default();
        let lines = vec![
            crate::lyrics::LyricLine::new(0.0, "Lead line"),
            crate::lyrics::LyricLine::new(1.0, "Echo vocal").with_bg(true),
            crate::lyrics::LyricLine::new(5.0, "Next main"),
        ];
        let blocks: Vec<WrappedBlock> = lines
            .iter()
            .map(|l| WrappedBlock {
                primary: if l.is_bg {
                    Vec::new()
                } else {
                    vec![l.text.clone()]
                },
                background: if l.is_bg {
                    vec![l.text.clone()]
                } else {
                    Vec::new()
                },
                bg_first: l.is_bg,
            })
            .collect();
        let update = Update {
            lines: std::sync::Arc::new(lines),
            index: Some(0),
            position: 1.5,
            playing: true,
            ..Default::default()
        };
        let visible = gather_visible_lines(&update, &blocks, 80, 24, &styles, 1.5, false, None, 0);
        assert_eq!(visible.current.len(), 2);
        assert_eq!(visible.current[0].spans[0].content, "Lead line");
        assert_eq!(visible.current[1].spans[0].content, "Echo vocal");
        assert_eq!(visible.current[1].spans[0].style, styles.background_current);
        assert_eq!(visible.after.len(), 1);
        assert_eq!(visible.after[0].spans[0].content, "Next main");
    }

    #[test]
    fn test_no_split_bg_keeps_parenthetical_inline() {
        let styles = LyricStyles::default();
        let words = vec![
            crate::lyrics::types::create_word_timing(0.0, 0.3, "I").with_space(true),
            crate::lyrics::types::create_word_timing(0.3, 0.6, "want").with_space(true),
            crate::lyrics::types::create_word_timing(0.6, 0.9, "you").with_space(true),
            crate::lyrics::types::create_word_timing(0.9, 1.2, "(Oh").with_space(true),
            crate::lyrics::types::create_word_timing(1.2, 1.5, "yeah)"),
        ];
        let lines = vec![
            crate::lyrics::LyricLine::new(0.0, "I want you (Oh yeah)")
                .with_end(Some(1.5))
                .with_words(Some(words)),
        ];

        let mut cache = None;
        let blocks = ensure_wrapped_cache(&mut cache, &lines, 80, false);
        assert_eq!(blocks[0].primary, vec!["I want you (Oh yeah)"]);
        assert!(blocks[0].background.is_empty());

        let update = Update {
            lines: std::sync::Arc::new(lines),
            index: Some(0),
            position: 1.6,
            playing: true,
            ..Default::default()
        };
        let visible = gather_visible_lines_with_split(
            &update, blocks, 80, 24, &styles, 1.6, true, false, None, 0,
        );
        assert_eq!(visible.current.len(), 1);
        let rendered: String = visible.current[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(rendered, "I want you (Oh yeah)");
    }

    #[test]
    fn test_max_visible_lines_counts_main_plus_bg_as_one_group() {
        let styles = LyricStyles::default();
        let lines = vec![
            crate::lyrics::LyricLine::new(0.0, "Lead 0"),
            crate::lyrics::LyricLine::new(1.0, "Echo 0").with_bg(true),
            crate::lyrics::LyricLine::new(5.0, "Lead 1"),
            crate::lyrics::LyricLine::new(6.0, "Echo 1").with_bg(true),
            crate::lyrics::LyricLine::new(10.0, "Lead 2"),
            crate::lyrics::LyricLine::new(11.0, "Echo 2").with_bg(true),
        ];
        let mut cache = None;
        let blocks = ensure_wrapped_cache(&mut cache, &lines, 80, true);
        let update = Update {
            lines: std::sync::Arc::new(lines),
            index: Some(2),
            position: 6.5,
            playing: true,
            ..Default::default()
        };

        let visible = gather_visible_lines_with_split(
            &update,
            blocks,
            80,
            24,
            &styles,
            6.5,
            false,
            true,
            Some(3),
            0,
        );
        assert_eq!(visible.before.len(), 2);
        assert_eq!(visible.before[0].spans[0].content, "Lead 0");
        assert_eq!(visible.before[1].spans[0].content, "Echo 0");
        assert_eq!(visible.current.len(), 2);
        assert_eq!(visible.current[0].spans[0].content, "Lead 1");
        assert_eq!(visible.current[1].spans[0].content, "Echo 1");
        assert_eq!(visible.after.len(), 2);
        assert_eq!(visible.after[0].spans[0].content, "Lead 2");
        assert_eq!(visible.after[1].spans[0].content, "Echo 2");
    }

    #[test]
    fn test_leading_is_bg_renders_above_main_in_current_and_context() {
        let styles = LyricStyles::default();
        // Matches lrc.red TTML structure (e.g. L38 of Rolling In the Deep):
        // x-bg vocal is sung chronologically before main words, even though parse_ttml emits [main, bg].
        let lines = vec![
            crate::lyrics::LyricLine::new(165.0, "Prev main"),
            crate::lyrics::LyricLine::new(170.072, "We could've had it all").with_words(Some(
                vec![
                    crate::lyrics::types::create_word_timing(173.177, 173.477, "We")
                        .with_space(true),
                    crate::lyrics::types::create_word_timing(173.477, 177.443, "all"),
                ],
            )),
            crate::lyrics::LyricLine::new(170.072, "You're gonna wish")
                .with_words(Some(vec![
                    crate::lyrics::types::create_word_timing(170.072, 170.500, "You're")
                        .with_space(true),
                    crate::lyrics::types::create_word_timing(170.500, 171.200, "wish"),
                ]))
                .with_bg(true),
            crate::lyrics::LyricLine::new(180.0, "Next main"),
        ];
        let mut cache = None;
        let blocks = ensure_wrapped_cache(&mut cache, &lines, 80, true);
        let update = Update {
            lines: std::sync::Arc::new(lines),
            index: Some(1),
            position: 171.0,
            playing: true,
            ..Default::default()
        };

        let visible = gather_visible_lines_with_split(
            &update, blocks, 80, 24, &styles, 171.0, true, true, None, 0,
        );
        assert_eq!(visible.current.len(), 2);
        let first_line: String = visible.current[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let second_line: String = visible.current[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(first_line, "You're wish");
        assert_eq!(second_line, "We all");

        // When focus advances to Next main, the leading bg group in `before` also keeps bg above main.
        let update_after = Update {
            lines: update.lines.clone(),
            index: Some(3),
            position: 181.0,
            playing: true,
            ..Default::default()
        };
        let visible_after = gather_visible_lines_with_split(
            &update_after,
            blocks,
            80,
            24,
            &styles,
            181.0,
            false,
            true,
            None,
            0,
        );
        assert_eq!(
            visible_after.before[1].spans[0].content,
            "You're gonna wish"
        );
        assert_eq!(
            visible_after.before[2].spans[0].content,
            "We could've had it all"
        );
    }

    #[test]
    fn test_overlapping_before_line_continues_karaoke_until_sung_end() {
        let styles = LyricStyles::default();
        // L38 main held "all" until 177.443 while L39 x-bg enters at 174.553.
        let lines = vec![
            crate::lyrics::LyricLine::new(170.072, "We could've had it all").with_words(Some(
                vec![
                    crate::lyrics::types::create_word_timing(173.177, 173.477, "We")
                        .with_space(true),
                    crate::lyrics::types::create_word_timing(173.477, 174.655, "had")
                        .with_space(true),
                    crate::lyrics::types::create_word_timing(174.655, 177.443, "all"),
                ],
            )),
            crate::lyrics::LyricLine::new(170.072, "You're gonna wish")
                .with_words(Some(vec![crate::lyrics::types::create_word_timing(
                    170.072, 173.050, "wish",
                )]))
                .with_bg(true),
            crate::lyrics::LyricLine::new(174.553, "We could've had it all").with_words(Some(
                vec![
                    crate::lyrics::types::create_word_timing(177.701, 178.000, "We")
                        .with_space(true),
                    crate::lyrics::types::create_word_timing(178.000, 182.000, "all"),
                ],
            )),
            crate::lyrics::LyricLine::new(174.553, "Tears are gonna fall")
                .with_words(Some(vec![crate::lyrics::types::create_word_timing(
                    174.553, 176.000, "Tears",
                )]))
                .with_bg(true),
        ];
        let mut cache = None;
        let blocks = ensure_wrapped_cache(&mut cache, &lines, 80, true);

        // At 175.0: index is L39 (2), while L38 main (0) is still singing "all" and L38 bg (1) already ended at 173.050.
        let upd_mid = Update {
            lines: std::sync::Arc::new(lines.clone()),
            index: Some(2),
            position: 175.0,
            playing: true,
            ..Default::default()
        };
        let visible_mid = gather_visible_lines_with_split(
            &upd_mid, blocks, 80, 24, &styles, 175.0, true, true, None, 0,
        );
        // L38 bg (before[0]) already ended -> dimmed cyan before style
        assert_eq!(
            visible_mid.before[0].spans[0].style,
            styles.before.fg(ratatui::style::Color::Cyan)
        );
        // L38 main (before[1]) still active -> first word highlighted green, last word actively animating!
        assert_eq!(visible_mid.before[1].spans[0].style, styles.current);
        assert!(visible_mid.before[1].spans.len() >= 3);

        // At 178.0: L38 main ended at 177.443 -> now both L38 bg and L38 main use dim before styles.
        let upd_after = Update {
            lines: std::sync::Arc::new(lines),
            index: Some(2),
            position: 178.0,
            playing: true,
            ..Default::default()
        };
        let visible_after = gather_visible_lines_with_split(
            &upd_after, blocks, 80, 24, &styles, 178.0, true, true, None, 0,
        );
        assert_eq!(visible_after.before[1].spans[0].style, styles.before);
    }
}
