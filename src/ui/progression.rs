use crate::state::Update;
use std::pin::Pin;
use tokio::time::Sleep;
use std::time::{Duration, Instant};

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

    // If richsync per-word timings are available, use them for fine-grained
    // scheduling. Otherwise, fall back to scheduling based on the next line
    // timestamp (line-by-line progression).
    let is_richsync = matches!(upd.provider, Some(crate::state::Provider::MusixmatchRichsync));
    if !is_richsync {
        // Find next line start after current position
        let pos = upd.position;
        for i in upd.index.unwrap()..upd.lines.len() {
            if let Some(line) = upd.lines.get(i) {
                if line.time.is_finite() && line.time > pos {
                    let dur = (line.time - pos).max(0.0);
                    let when = tokio::time::Instant::now() + Duration::from_secs_f64(dur);
                    return Some(Box::pin(tokio::time::sleep_until(when)));
                }
            }
        }
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
    _karaoke_enabled: bool,
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

    // Always compute next sleep when possible. `compute_next_word_sleep_from_update`
    // will return fine-grained per-word sleeps for richsync or line-level sleeps
    // for non-richsync lyrics. The `karaoke_enabled` flag only affects rendering
    // (whether per-word highlighting is used), not whether we schedule wakes
    // to advance lines.
    let next = compute_next_word_sleep_from_update(&tmp);
    (Some(tmp), next)
}
