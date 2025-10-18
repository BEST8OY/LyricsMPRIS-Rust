//! Position estimation and timer scheduling for smooth lyrics progression.
//!
//! This module provides:
//! - Local position estimation based on elapsed time since last MPRIS update
//! - Per-word and per-grapheme boundary scheduling for richsync karaoke
//! - Line-level scheduling for standard synchronized lyrics

use crate::state::Update;
use std::pin::Pin;
use tokio::time::Sleep;
use std::time::{Duration, Instant};

/// Compute the next tokio Sleep based on lyrics timing.
///
/// For richsync lyrics, schedules wakeups at word/grapheme boundaries.
/// For standard lyrics, schedules wakeups at line transitions.
/// Returns `None` when playback is paused or no future boundary exists.
pub fn compute_next_word_sleep_from_update(upd: &Update) -> Option<Pin<Box<Sleep>>> {
    if !upd.playing {
        return None;
    }

    // Before any line is active, schedule wake at first line start
    if upd.index.is_none() {
        return schedule_first_line_start(upd);
    }

    let is_richsync = matches!(upd.provider, Some(crate::state::Provider::MusixmatchRichsync));
    
    if is_richsync {
        schedule_next_richsync_boundary(upd)
    } else {
        schedule_next_line_start(upd)
    }
}

/// Schedule a wakeup at the first line's start time.
fn schedule_first_line_start(upd: &Update) -> Option<Pin<Box<Sleep>>> {
    let first = upd.lines.first()?;
    
    if !first.time.is_finite() || first.time <= upd.position {
        return None;
    }

    let delay = (first.time - upd.position).max(0.0);
    Some(create_sleep(delay))
}

/// Schedule a wakeup at the next line start (non-richsync).
fn schedule_next_line_start(upd: &Update) -> Option<Pin<Box<Sleep>>> {
    let current_idx = upd.index?;
    
    // Search from current line onward for next line after current position
    for line in upd.lines.iter().skip(current_idx) {
        if line.time.is_finite() && line.time > upd.position {
            let delay = (line.time - upd.position).max(0.0);
            return Some(create_sleep(delay));
        }
    }

    None
}

/// Schedule a wakeup at the next word/grapheme boundary (richsync).
fn schedule_next_richsync_boundary(upd: &Update) -> Option<Pin<Box<Sleep>>> {
    let current_idx = upd.index?;
    let mut best_delay: Option<f64> = None;

    // Scan from current line forward for the nearest future boundary
    for line in upd.lines.iter().skip(current_idx) {
        let Some(words) = &line.words else {
            continue;
        };

        for word in words {
            update_best_delay(&mut best_delay, word.start, upd.position);
            update_best_delay(&mut best_delay, word.end, upd.position);

            // Schedule grapheme boundaries for smooth per-character animation
            if word.grapheme_count() > 1 {
                for grapheme_boundary in compute_grapheme_boundaries(word) {
                    update_best_delay(&mut best_delay, grapheme_boundary, upd.position);
                }
            }
        }

        // Early exit if we found a very near boundary
        if let Some(d) = best_delay
            && d <= 0.01 {
                break;
            }
    }

    best_delay.map(create_sleep)
}

/// Update best_delay if boundary is in the future and closer than current best.
fn update_best_delay(best: &mut Option<f64>, boundary: f64, position: f64) {
    if boundary <= position {
        return;
    }

    let delay = boundary - position;
    *best = Some(match *best {
        Some(current) => current.min(delay),
        None => delay,
    });
}

/// Compute grapheme boundaries for a word with per-word timing.
fn compute_grapheme_boundaries(word: &crate::lyrics::types::WordTiming) -> Vec<f64> {
    let total = word.grapheme_count();
    let duration = (word.end - word.start).max(f64::EPSILON);
    
    (1..total)
        .map(|k| word.start + (k as f64 / total as f64) * duration)
        .collect()
}

/// Create a tokio sleep with the given delay in seconds.
fn create_sleep(delay_secs: f64) -> Pin<Box<Sleep>> {
    let delay = delay_secs.max(0.0);
    let when = tokio::time::Instant::now() + Duration::from_secs_f64(delay);
    Box::pin(tokio::time::sleep_until(when))
}

/// Estimate current position and line index based on elapsed time.
///
/// This function:
/// 1. Advances position based on time elapsed since last MPRIS update
/// 2. Recomputes the current line index via binary search
/// 3. Schedules the next timer wakeup for smooth rendering
///
/// The `_karaoke_enabled` parameter is unused here (affects rendering only).
pub fn estimate_update_and_next_sleep(
    last_update: &Option<Update>,
    last_update_instant: Option<Instant>,
    _karaoke_enabled: bool,
) -> (Option<Update>, Option<Pin<Box<Sleep>>>) {
    let Some(update) = last_update else {
        return (None, None);
    };

    let mut estimated = update.clone();

    // Advance position if playing
    if estimated.playing
        && let Some(since) = last_update_instant {
            estimated.position += since.elapsed().as_secs_f64();
        }

    // Recompute current line index from estimated position
    estimated.index = compute_line_index(&estimated);

    // Schedule next boundary for smooth rendering
    let next_sleep = compute_next_word_sleep_from_update(&estimated);

    (Some(estimated), next_sleep)
}

/// Compute the current line index from position using binary search.
///
/// Returns `None` if:
/// - Not enough lines
/// - Position is invalid (NaN)
/// - Any line time is invalid
/// - Position is before the first line
fn compute_line_index(update: &Update) -> Option<usize> {
    // Need at least 2 lines for meaningful index
    if update.lines.len() <= 1 {
        return None;
    }

    // Validate position and line times
    if update.position.is_nan() || update.lines.iter().any(|l| l.time.is_nan()) {
        return None;
    }

    // Before first line
    if let Some(first) = update.lines.first()
        && update.position < first.time {
            return None;
        }

    // Binary search for current line
    match update.lines.binary_search_by(|line| {
        line.time
            .partial_cmp(&update.position)
            .unwrap_or(std::cmp::Ordering::Less)
    }) {
        Ok(idx) => Some(idx),      // Exact match
        Err(0) => None,             // Before first line
        Err(idx) => Some(idx - 1),  // Between lines
    }
}
