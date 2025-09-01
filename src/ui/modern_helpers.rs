use crate::text_utils::wrap_text;
use crate::state::Update;
use crate::ui::styles::LyricStyles;
use tui::text::Spans;
use std::time::Duration;
use std::time::Instant;
use tui::Terminal;
use tui::widgets::Paragraph;
use tui::layout::{Alignment, Rect};
use tui::backend::Backend;
use std::error::Error;

/// Draw the UI using cached lines and the modern helpers.
pub fn draw_ui_with_cache<B: Backend>(
    terminal: &mut Terminal<B>,
    last_update: Option<&Update>,
    cached_lines: Option<&[String]>,
    cached_wrapped: &mut Option<(usize, Vec<Vec<String>>)>,
    styles: &LyricStyles,
    karaoke_enabled: bool,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    terminal
        .draw(|f| {
            let size = f.size();
            let w = size.width as usize;
            let h = size.height as usize;
            // Prepare wrapped blocks, using cache when possible and matching width
            let wrapped_blocks: Vec<Vec<String>> = match cached_wrapped.as_ref() {
                Some((cached_w, blocks)) if *cached_w == w => blocks.clone(),
                _ => {
                    if let Some(cached) = cached_lines {
                        cached.iter().map(|l| wrap_text(l, w)).collect()
                    } else {
                        Vec::new()
                    }
                }
            };

            // Update cached_wrapped if we recomputed
            if cached_wrapped.as_ref().map(|(cw, _)| *cw != w).unwrap_or(true) {
                if !wrapped_blocks.is_empty() {
                    *cached_wrapped = Some((w, wrapped_blocks.clone()));
                } else {
                    *cached_wrapped = None;
                }
            }

            let visible_spans = {
                if let Some(update) = last_update {
                    if let Some(ref err) = update.err {
                        wrap_text(err, w)
                            .into_iter()
                            .map(|line| Spans::from(tui::text::Span::styled(line, styles.current)))
                            .collect()
                    } else if !wrapped_blocks.is_empty() && update.index < wrapped_blocks.len() {
                        gather_visible_lines(update, &wrapped_blocks, w, h, styles, update.position, karaoke_enabled).into_vec()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            };

            if visible_spans.is_empty() {
                let paragraph = Paragraph::new(vec![Spans::from(tui::text::Span::raw(""))]).alignment(Alignment::Center);
                f.render_widget(paragraph, size);
            } else {
                let top_padding = h.saturating_sub(visible_spans.len()) / 2;
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

/// Compute the duration until the next interesting karaoke boundary (word start/end or
/// intermediate grapheme boundary) for a richsync Update. Returns None when not
/// applicable (not playing or not richsync provider).
pub fn compute_next_word_sleep_from_update(
    upd: &Update,
) -> Option<Duration> {
    if !upd.playing || !matches!(upd.provider, Some(crate::state::Provider::MusixmatchRichsync)) {
        return None;
    }
    let pos = upd.position;
    let mut next_dur: Option<f64> = None;
    // scan current and subsequent lines for next word start or end > pos
    for i in upd.index..upd.lines.len() {
        if let Some(line) = upd.lines.get(i) && let Some(words) = &line.words {
            for w in words.iter() {
                // Word start
                if w.start > pos {
                    let d = w.start - pos;
                    next_dur = Some(next_dur.map_or(d, |nd| nd.min(d)));
                }
                // Word end
                if w.end > pos {
                    let d = w.end - pos;
                    next_dur = Some(next_dur.map_or(d, |nd| nd.min(d)));
                }
                // Also schedule intermediate grapheme boundaries so we can progressively
                // highlight parts of the word instead of jumping only at word end.
                // Approximate grapheme timings by evenly dividing the word duration
                // across graphemes (parse may not provide per-grapheme times).
                let total = w.graphemes.len();
                if total > 1 {
                    let dur = (w.end - w.start).max(f64::EPSILON);
                    for k in 1..total { // boundary after k graphemes
                        let boundary = w.start + (k as f64 / total as f64) * dur;
                        if boundary > pos {
                            let d = boundary - pos;
                            next_dur = Some(next_dur.map_or(d, |nd| nd.min(d)));
                        }
                    }
                }
            }
        }
        // If we already found a very near boundary, stop early
        if let Some(d) = next_dur && d <= 0.0 {
            break;
        }
    }
    if let Some(dur) = next_dur {
        let dur = dur.max(0.0);
        Some(Duration::from_secs_f64(dur))
    } else {
        None
    }
}

/// Estimate the current Update from the last known Update and the instant it was
/// received. Returns (estimated_update, optional_duration_to_next_boundary).
pub fn estimate_update_and_next_sleep(
    last_update: &Option<Update>,
    last_update_instant: Option<Instant>,
    karaoke_enabled: bool,
) -> (Option<Update>, Option<Duration>) {
    if let Some(upd) = last_update {
        let mut tmp = upd.clone();
        if tmp.playing
            && let Some(since) = last_update_instant {
                tmp.position += since.elapsed().as_secs_f64();
            }
        // Estimate the current line index locally from the estimated position so the UI
        // can advance lines (and not wait for backend updates) when richsync moves fast.
        // Mirrors the binary-search behavior in `state::LyricState::get_index`.
        if tmp.lines.len() <= 1 || tmp.position.is_nan() || tmp.lines.iter().any(|line| line.time.is_nan()) {
            tmp.index = 0;
        } else {
            tmp.index = match tmp
                .lines
                .binary_search_by(|line| match line.time.partial_cmp(&tmp.position) {
                    Some(ord) => ord,
                    _ => std::cmp::Ordering::Less,
                }) {
                Ok(idx) => idx,
                Err(0) => 0,
                Err(idx) => idx - 1,
            };
        }
        let next = if karaoke_enabled {
            compute_next_word_sleep_from_update(&tmp)
        } else {
            None
        };
        (Some(tmp), next)
    } else {
        (None, None)
    }
}

/// A collection of styled text lines (Spans) ready for rendering.
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

/// Collects the styled lines that should appear *before* the current lyric.
pub fn collect_before_spans<'a>(
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
            .map(|line| Spans::from(tui::text::Span::styled(line.clone(), style)));
        before_spans.splice(0..0, spans); // Prepend to maintain order
        lines_needed -= take;
    }
    before_spans
}

/// Collects the styled lines that should appear *after* the current lyric.
pub fn collect_after_spans<'a>(
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
            .map(|line| Spans::from(tui::text::Span::styled(line.clone(), style)));
        after_spans.extend(spans);
        lines_needed -= take;
        j += 1;
    }
    after_spans
}

/// Split an array of WordTiming into visual lines by width (chars). This keeps word timings
/// intact while producing lines that fit in a given width.
pub fn split_words_into_lines<'b>(
    words: &'b [crate::lyrics::types::WordTiming],
    width: usize,
) -> Vec<Vec<&'b crate::lyrics::types::WordTiming>> {
    let mut lines: Vec<Vec<&'b crate::lyrics::types::WordTiming>> = Vec::new();
    let mut current_line: Vec<&'b crate::lyrics::types::WordTiming> = Vec::new();
    let mut current_len: usize = 0;

    for w in words {
        let wlen = w.text.chars().count();
        let new_len = if current_line.is_empty() {
            wlen
        } else {
            current_len + 1 + wlen // space + word
        };
        if !current_line.is_empty() && new_len > width && width > 0 {
            lines.push(current_line);
            current_line = Vec::new();
            current_len = 0;
        }
        if current_line.is_empty() {
            current_line.push(w);
            current_len = wlen;
        } else {
            current_line.push(w);
            current_len += 1 + wlen;
        }
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// Build VisibleLines from update/cached lines. Keeps logic minimal and focused for tests.
pub fn gather_visible_lines<'a>(
    update: &Update,
    wrapped_blocks: &[Vec<String>],
    w: usize,
    h: usize,
    styles: &'a LyricStyles,
    position: f64,
    karaoke_enabled: bool,
) -> VisibleLines<'a> {
    let current_block = &wrapped_blocks[update.index];
    let current_height = current_block.len();

    let mut current = Vec::new();

    if let Some(ly) = update.lines.get(update.index)
        && karaoke_enabled && matches!(update.provider, Some(crate::state::Provider::MusixmatchRichsync))
            && let Some(words) = &ly.words {
                let word_lines = split_words_into_lines(words, w);
                for wl in word_lines.iter() {
                    let mut spans = Vec::new();
                    for (i, wt) in wl.iter().enumerate() {
                        if position >= wt.end {
                            if i + 1 < wl.len() {
                                spans.push(tui::text::Span::styled(format!("{} ", wt.text), styles.current));
                            } else {
                                spans.push(tui::text::Span::styled(wt.text.clone(), styles.current));
                            }
                            continue;
                        }
                        if position < wt.start {
                            if i + 1 < wl.len() {
                                spans.push(tui::text::Span::styled(format!("{} ", wt.text), styles.after));
                            } else {
                                spans.push(tui::text::Span::styled(wt.text.clone(), styles.after));
                            }
                            continue;
                        }

                        let dur = (wt.end - wt.start).max(f64::EPSILON);
                        let frac = ((position - wt.start) / dur).clamp(0.0, 1.0);
                        let total = wt.graphemes.len();
                        let highlight_graphemes = ((frac * total as f64).floor() as usize).min(total);

                        if highlight_graphemes == 0 {
                            if i + 1 < wl.len() {
                                spans.push(tui::text::Span::styled(format!("{} ", wt.text), styles.after));
                            } else {
                                spans.push(tui::text::Span::styled(wt.text.clone(), styles.after));
                            }
                        } else if highlight_graphemes >= total {
                            if i + 1 < wl.len() {
                                spans.push(tui::text::Span::styled(format!("{} ", wt.text), styles.current));
                            } else {
                                spans.push(tui::text::Span::styled(wt.text.clone(), styles.current));
                            }
                        } else {
                            // Build highlighted and remaining substrings using byte offsets into wt.text
                            let start_byte = wt.grapheme_byte_offsets[0];
                            let split_byte = wt.grapheme_byte_offsets[highlight_graphemes];
                            let highlighted = wt.text[start_byte..split_byte].to_string();
                            let remaining = wt.text[split_byte..].to_string();
                            spans.push(tui::text::Span::styled(highlighted, styles.current));
                            if i + 1 < wl.len() {
                                spans.push(tui::text::Span::styled(format!("{} ", remaining), styles.after));
                            } else {
                                spans.push(tui::text::Span::styled(remaining, styles.after));
                            }
                        }
                    }
                    current.push(Spans::from(spans));
                }
            }

    if current.is_empty() {
        for line in current_block.iter() {
            current.push(Spans::from(tui::text::Span::styled(line.clone(), styles.current)));
        }
    }

    if current_height >= h {
        return VisibleLines {
            before: Vec::new(),
            current,
            after: Vec::new(),
        };
    }

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
