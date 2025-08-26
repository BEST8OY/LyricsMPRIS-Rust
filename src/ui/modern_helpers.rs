use crate::text_utils::wrap_text;
use crate::state::Update;
use crate::ui::styles::LyricStyles;
use tui::text::Spans;

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

    let mut current = Vec::new();

    if let Some(ly) = update.lines.get(update.index) {
        if karaoke_enabled && matches!(update.provider, Some(crate::state::Provider::MusixmatchRichsync)) {
            if let Some(words) = &ly.words {
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

                        let dur = (wt.end - wt.start).max(std::f64::EPSILON);
                        let frac = ((position - wt.start) / dur).clamp(0.0, 1.0);
                        let chars: Vec<char> = wt.text.chars().collect();
                        let total = chars.len();
                        let highlight_chars = ((frac * total as f64).floor() as usize).min(total);

                        if highlight_chars == 0 {
                            if i + 1 < wl.len() {
                                spans.push(tui::text::Span::styled(format!("{} ", wt.text), styles.after));
                            } else {
                                spans.push(tui::text::Span::styled(wt.text.clone(), styles.after));
                            }
                        } else if highlight_chars >= total {
                            if i + 1 < wl.len() {
                                spans.push(tui::text::Span::styled(format!("{} ", wt.text), styles.current));
                            } else {
                                spans.push(tui::text::Span::styled(wt.text.clone(), styles.current));
                            }
                        } else {
                            let highlighted: String = chars[..highlight_chars].iter().collect();
                            let remaining: String = chars[highlight_chars..].iter().collect();
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
