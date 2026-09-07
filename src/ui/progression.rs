//! Position estimation and timer scheduling for smooth lyrics progression.
//!
//! This module provides:
//! - Local position estimation based on elapsed time since last MPRIS update
//! - Per-word and per-grapheme boundary scheduling for richsync karaoke
//! - Line-level scheduling for standard synchronized lyrics

use crate::state::Update;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::time::Sleep;

/// Compute the next tokio Sleep based on lyrics timing.
///
/// For richsync lyrics, schedules wakeups at word/grapheme boundaries.
/// For standard lyrics, schedules wakeups at line transitions.
/// Returns `None` when playback is paused or no future boundary exists.
pub fn compute_next_word_sleep_from_update(upd: &Update) -> Option<Pin<Box<Sleep>>> {
    if !upd.playing || upd.lines.is_empty() || !upd.position.is_finite() {
        return None;
    }

    let is_richsync = matches!(
        upd.provider,
        Some(crate::state::Provider::MusixmatchRichsync)
    );

    if is_richsync {
        schedule_next_richsync_boundary(upd)
    } else {
        schedule_next_line_start(upd)
    }
}

/// Schedule a wakeup at the next line start (non-richsync).
fn schedule_next_line_start(upd: &Update) -> Option<Pin<Box<Sleep>>> {
    let start_idx = upd.index.unwrap_or(0);

    // Search from current line onward for next line after current position
    for line in upd.lines.iter().skip(start_idx) {
        if line.time.is_finite() && line.time > upd.position {
            let delay = (line.time - upd.position).max(0.0);
            return Some(create_sleep(delay));
        }
    }

    None
}

/// Schedule a wakeup at the next word/grapheme boundary (richsync).
fn schedule_next_richsync_boundary(upd: &Update) -> Option<Pin<Box<Sleep>>> {
    let start_idx = upd.index.unwrap_or(0);
    let mut best_delay: Option<f64> = None;

    // Scan from current line forward for the nearest future boundary
    for line in upd.lines.iter().skip(start_idx) {
        // Because lines are chronologically sorted, if this line's start time is already
        // further in the future than our best found boundary, no word in this line or any
        // subsequent line can possibly start sooner.
        if let Some(d) = best_delay
            && (d <= 0.01 || (line.time.is_finite() && line.time > upd.position + d))
        {
            break;
        }

        if line.time.is_finite() && line.time > upd.position {
            update_best_delay(&mut best_delay, line.time, upd.position);
        }

        if let Some(end) = line.end
            && end.is_finite()
            && end > upd.position
        {
            update_best_delay(&mut best_delay, end, upd.position);
        }

        let Some(words) = &line.words else {
            continue;
        };

        for word in words {
            update_best_delay(&mut best_delay, word.start, upd.position);
            update_best_delay(&mut best_delay, word.end, upd.position);

            // Use pre-computed grapheme boundaries (no allocation)
            for &grapheme_boundary in &word.grapheme_times {
                update_best_delay(&mut best_delay, grapheme_boundary, upd.position);
            }
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

/// Create a tokio sleep with the given delay in seconds.
fn create_sleep(delay_secs: f64) -> Pin<Box<Sleep>> {
    // Clamp minimum sleep delay to 12ms to match terminal frame timing & avoid OS timer thrashing
    let delay = delay_secs.max(0.012);
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

    // Advance position if playing: prioritize authoritative TimeAnchor projection, fallback to elapsed instant
    if estimated.playing {
        if estimated.time_anchor.instant.is_some() {
            estimated.position = estimated.current_position();
        } else if let Some(since) = last_update_instant {
            estimated.position += since.elapsed().as_secs_f64();
        }
    }

    // Recompute current line index from estimated position
    estimated.index = compute_line_index(&estimated);

    // Schedule next boundary for smooth rendering
    let next_sleep = compute_next_word_sleep_from_update(&estimated);

    (Some(estimated), next_sleep)
}

/// Compute the current line index from position.
///
/// Returns `None` if:
/// - No lyrics
/// - Position is invalid (NaN)
/// - Any line time is invalid (NaN)
/// - Position is before the first line
///
/// Delegates to the shared [`active_line_index`] binary search also used by
/// `LyricState::get_index`, keeping authoritative state and UI estimation in sync.
fn compute_line_index(update: &Update) -> Option<usize> {
    // Validate position and line times before searching
    if update.lines.is_empty()
        || update.position.is_nan()
        || update.lines.iter().any(|l| l.time.is_nan())
    {
        return None;
    }

    crate::lyrics::types::active_line_index(&update.lines, update.position)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_richsync_schedules_line_time_when_before_first_word() {
        // Line 0 is active from 0.0 to 10.0, but its words already passed by 8.0s
        // Line 1 starts at 10.0s, but its first word starts at 12.0s
        let line0 = crate::lyrics::LyricLine::new(0.0, "Line 0").with_words(Some(vec![
            crate::lyrics::types::WordTiming {
                start: 1.0,
                end: 2.0,
                text: "Hello".into(),
                grapheme_boundaries: vec![0, 5],
                grapheme_times: vec![],
            },
        ]));
        let line1 = crate::lyrics::LyricLine::new(10.0, "Line 1").with_words(Some(vec![
            crate::lyrics::types::WordTiming {
                start: 12.0,
                end: 13.0,
                text: "World".into(),
                grapheme_boundaries: vec![0, 5],
                grapheme_times: vec![],
            },
        ]));

        let update = Update {
            lines: Arc::new(vec![line0, line1]),
            index: Some(0),
            position: 8.5,
            playing: true,
            provider: Some(crate::state::Provider::MusixmatchRichsync),
            ..Default::default()
        };

        // At position 8.5, next boundary should be line 1 start at 10.0s (delay = 1.5s), NOT 12.0s!
        let sleep = compute_next_word_sleep_from_update(&update);
        assert!(sleep.is_some(), "Should schedule wakeup for line 1 start");
    }

    #[tokio::test]
    async fn test_progression_schedules_first_line_when_index_is_none() {
        let line0 = crate::lyrics::LyricLine::new(5.0, "Intro Line");
        let line1 = crate::lyrics::LyricLine::new(15.0, "Next Line");

        let update = Update {
            lines: Arc::new(vec![line0, line1]),
            index: None,
            position: 1.0,
            playing: true,
            ..Default::default()
        };

        let sleep = compute_next_word_sleep_from_update(&update);
        assert!(
            sleep.is_some(),
            "Should schedule wakeup for first line start when index is None"
        );
    }

    #[tokio::test]
    async fn test_progression_schedules_next_line_for_standard_lrc() {
        let line0 = crate::lyrics::LyricLine::new(5.0, "Line 0");
        let line1 = crate::lyrics::LyricLine::new(15.0, "Line 1");

        let update = Update {
            lines: Arc::new(vec![line0, line1]),
            index: Some(0),
            position: 8.0,
            playing: true,
            provider: Some(crate::state::Provider::Lrclib),
            ..Default::default()
        };

        let sleep = compute_next_word_sleep_from_update(&update);
        assert!(
            sleep.is_some(),
            "Should schedule wakeup for next line start"
        );
    }

    #[tokio::test]
    async fn test_progression_returns_none_when_paused() {
        let line0 = crate::lyrics::LyricLine::new(5.0, "Line 0");
        let update = Update {
            lines: Arc::new(vec![line0]),
            index: Some(0),
            position: 2.0,
            playing: false,
            ..Default::default()
        };

        let sleep = compute_next_word_sleep_from_update(&update);
        assert!(sleep.is_none(), "Should not schedule when paused");
    }
}
