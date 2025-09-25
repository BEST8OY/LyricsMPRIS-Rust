use crate::text_utils::wrap_text;
use crate::state::Update;
use crate::ui::styles::LyricStyles;
use std::pin::Pin;
use tokio::time::Sleep;
use std::time::{Duration, Instant};
use tui::{
    backend::Backend,
    layout::{Alignment, Rect},
    terminal::Terminal,
    text::{Span, Spans},
    widgets::Paragraph,
};
use std::error::Error;

/// A compact, clearer rewrite of the original `modern_helpers.rs` helpers.
///
/// The module exposes the same high-level helpers used by the UI:
///
/// - `draw_ui_with_cache` to render cached/wrapped lyric lines
/// - `estimate_update_and_next_sleep` to locally estimate the position/index
///   and optionally return a per-word wakeup for richsync karaoke
/// - `compute_next_word_sleep_from_update` which schedules the next tokio
///   sleep boundary when richsync timings are available
/// - `gather_visible_lines` and helpers to produce styled Spans ready for
///   render
///
/// Draw the UI using cached wrapped lines and modern helpers. This keeps
/// the external contract identical to the original helper so it can be
/// swapped in without changes elsewhere.
pub fn draw_ui_with_cache<B: Backend>(
    terminal: &mut Terminal<B>,
    last_update: &Option<Update>,
    wrapped_cache: &mut Option<(usize, Vec<Vec<String>>)>,
    cached_lines: &Option<Vec<String>>,
    styles: &LyricStyles,
    karaoke_enabled: bool,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    terminal
        .draw(|f| {
            let size = f.size();
            let width = size.width as usize;
            let height = size.height as usize;

            let visible_spans = if let Some(update) = last_update {
                // If there's an error message on the update, render it centered.
                if let Some(err) = &update.err {
                    wrap_text(err, width)
                        .into_iter()
                        .map(|l| Spans::from(Span::styled(l, styles.current)))
                        .collect()

                // Otherwise, if we have cached cached_lines and it's valid for rendering,
                // use them (wrapping if necessary) to compute visible spans.
                } else if let Some(cached) = cached_lines
                    && !cached.is_empty()
                    && update.index.map(|i| i < cached.len()).unwrap_or(true)
                {
                    // Ensure wrapped cache matches current width and number of blocks
                    let blocks_ref: &Vec<Vec<String>> = match wrapped_cache {
                        Some((cached_w, blocks)) if *cached_w == width && blocks.len() == cached.len() => blocks,
                        _ => {
                            let new_blocks: Vec<Vec<String>> = cached.iter().map(|l| wrap_text(l, width)).collect();
                            *wrapped_cache = Some((width, new_blocks));
                            &wrapped_cache.as_ref().unwrap().1
                        }
                    };

                    let visible = gather_visible_lines(update, blocks_ref, width, height, styles, update.position, karaoke_enabled);
                    visible.into_vec()

                // Nothing to render
                } else {
                    Vec::new()
                }
            } else { Vec::new() };

            if visible_spans.is_empty() {
                let paragraph = Paragraph::new(vec![Spans::from(Span::raw(""))]).alignment(Alignment::Center);
                f.render_widget(paragraph, size);
            } else {
                let top_padding = height.saturating_sub(visible_spans.len()) / 2;
                let render_area = Rect {
                    x: size.x,
                    y: size.y + top_padding as u16,
                    width: size.width,
                    height: (visible_spans.len() as u16).min(size.height),
                };
                let paragraph = Paragraph::new(visible_spans).alignment(Alignment::Center);
                f.render_widget(paragraph, render_area);
            }
        })
        .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

    Ok(())
}

/// Compute the next tokio Sleep based on per-word timings inside `upd`.
/// Returns `None` when scheduling is not necessary or possible.
pub fn compute_next_word_sleep_from_update(upd: &Update) -> Option<Pin<Box<Sleep>>> {
    if !upd.playing {
        return None;
    }

    // If there's no index yet, schedule a wake at the first line's start
    // if it lies in the future. This prevents starting mid-line when
    // backend updates are coarse.
    if upd.index.is_none() {
        if let Some(first) = upd.lines.first() {
            let pos = upd.position;
            if first.time.is_finite() && first.time > pos {
                let dur = (first.time - pos).max(0.0);
                let when = tokio::time::Instant::now() + Duration::from_secs_f64(dur);
                return Some(Box::pin(tokio::time::sleep_until(when)));
            }
        }
        return None;
    }

    // Only richsync provider includes per-word timings.
    if !matches!(upd.provider, Some(crate::state::Provider::MusixmatchRichsync)) {
        return None;
    }

    let pos = upd.position;
    let mut best_future: Option<f64> = None;

    // Scan from current index forward to find the next start/end or grapheme boundary
    for i in upd.index.unwrap()..upd.lines.len() {
        if let Some(line) = upd.lines.get(i) {
            if let Some(words) = &line.words {
                for w in words.iter() {
                    // word start
                    if w.start > pos {
                        let d = w.start - pos;
                        best_future = Some(best_future.map_or(d, |b| b.min(d)));
                    }
                    // word end
                    if w.end > pos {
                        let d = w.end - pos;
                        best_future = Some(best_future.map_or(d, |b| b.min(d)));
                    }
                    // grapheme boundaries (approximate)
                    let total = w.graphemes.len();
                    if total > 1 {
                        let dur = (w.end - w.start).max(f64::EPSILON);
                        for k in 1..total {
                            let boundary = w.start + (k as f64 / total as f64) * dur;
                            if boundary > pos {
                                let d = boundary - pos;
                                best_future = Some(best_future.map_or(d, |b| b.min(d)));
                            }
                        }
                    }
                }
            }
        }

        // If we found a zero/negative difference (shouldn't happen) we can break early
        if let Some(d) = best_future { if d <= 0.0 { break; } }
    }

    best_future.map(|d| {
        let dur = d.max(0.0);
        let when = tokio::time::Instant::now() + Duration::from_secs_f64(dur);
        Box::pin(tokio::time::sleep_until(when))
    })
}

/// Estimate an `Update` locally by advancing position based on `last_update_instant`.
/// Also returns an optional per-word sleep if karaoke is enabled and computed.
pub fn estimate_update_and_next_sleep(
    last_update: &Option<Update>,
    last_update_instant: Option<Instant>,
    karaoke_enabled: bool,
) -> (Option<Update>, Option<Pin<Box<Sleep>>>) {
    let maybe = if let Some(u) = last_update { u.clone() } else { return (None, None); };

    let mut tmp = maybe;
    if tmp.playing {
        if let Some(since) = last_update_instant {
            tmp.position += since.elapsed().as_secs_f64();
        }
    }

    // Recompute index from position in a safe way
    tmp.index = if tmp.lines.len() <= 1
        || tmp.position.is_nan()
        || tmp.lines.iter().any(|l| l.time.is_nan())
        || tmp.lines.first().map(|l| tmp.position < l.time).unwrap_or(false)
    {
        None
    } else {
        match tmp.lines.binary_search_by(|line| line.time.partial_cmp(&tmp.position).unwrap_or(std::cmp::Ordering::Less)) {
            Ok(idx) => Some(idx),
            Err(0) => None,
            Err(idx) => Some(idx - 1),
        }
    };

    let next = if karaoke_enabled { compute_next_word_sleep_from_update(&tmp) } else { None };
    (Some(tmp), next)
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
/// Keeps logic explicit and testable: if `update.index` is None we don't render a
/// highlighted current block; instead render the block using `styles.after`.
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
    let current_block: &[String] = wrapped_blocks.get(idx_for_context).map(|v| v.as_slice()).unwrap_or(&[]);
    let current_height = current_block.len();

    let mut current_spans: Vec<Spans<'a>> = Vec::new();

    // If we have an explicit index and karaoke is enabled with richsync words,
    // build word-by-word spans with partial grapheme highlighting.
    if let Some(idx) = update.index {
        if karaoke_enabled && matches!(update.provider, Some(crate::state::Provider::MusixmatchRichsync)) {
            if let Some(ly) = update.lines.get(idx) {
                if let Some(words) = &ly.words {
                    let word_lines = split_words_into_lines(words, w);
                    for wl in word_lines.iter() {
                        let mut line_spans: Vec<Span> = Vec::new();
                        for (i, wt) in wl.iter().enumerate() {
                            // Fully past the word
                            if position >= wt.end {
                                let txt = if i + 1 < wl.len() { format!("{} ", wt.text) } else { wt.text.clone() };
                                line_spans.push(Span::styled(txt, styles.current));
                                continue;
                            }
                            // Not reached this word yet
                            if position < wt.start {
                                let txt = if i + 1 < wl.len() { format!("{} ", wt.text) } else { wt.text.clone() };
                                line_spans.push(Span::styled(txt, styles.after));
                                continue;
                            }

                            // Word is partially highlighted
                            let dur = (wt.end - wt.start).max(f64::EPSILON);
                            let frac = ((position - wt.start) / dur).clamp(0.0, 1.0);
                            let total = wt.graphemes.len();
                            let highlight_graphemes = ((frac * total as f64).floor() as usize).min(total);

                            if highlight_graphemes == 0 {
                                let txt = if i + 1 < wl.len() { format!("{} ", wt.text) } else { wt.text.clone() };
                                line_spans.push(Span::styled(txt, styles.after));
                            } else if highlight_graphemes >= total {
                                let txt = if i + 1 < wl.len() { format!("{} ", wt.text) } else { wt.text.clone() };
                                line_spans.push(Span::styled(txt, styles.current));
                            } else {
                                // Use grapheme byte offsets to slice text safely
                                let start_byte = wt.grapheme_byte_offsets[0];
                                let split_byte = wt.grapheme_byte_offsets[highlight_graphemes];
                                let highlighted = wt.text[start_byte..split_byte].to_string();
                                let remaining = wt.text[split_byte..].to_string();
                                line_spans.push(Span::styled(highlighted, styles.current));
                                let rem_txt = if i + 1 < wl.len() { format!("{} ", remaining) } else { remaining };
                                line_spans.push(Span::styled(rem_txt, styles.after));
                            }
                        }
                        current_spans.push(Spans::from(line_spans));
                    }
                }
            }
        }
    }

    // If no spans were built above, fall back to rendering the wrapped block
    // either as "current" (if index present) or as "after" (if index is None).
    if current_spans.is_empty() {
        let use_current_style = update.index.is_some();
        let style = if use_current_style { styles.current } else { styles.after };
        for line in current_block.iter() {
            current_spans.push(Spans::from(Span::styled(line.clone(), style)));
        }
    }

    // If current block already fills the height, return it centered without context.
    if current_height >= h {
        return VisibleLines { before: Vec::new(), current: current_spans, after: Vec::new() };
    }

    let context_lines = h.saturating_sub(current_height);
    let lines_before = context_lines / 2;
    let lines_after = context_lines - lines_before;

    let before = collect_before_spans(idx_for_context, wrapped_blocks, lines_before, styles.before);
    let after = collect_after_spans(idx_for_context, wrapped_blocks, lines_after, styles.after);

    VisibleLines { before, current: current_spans, after }
}
