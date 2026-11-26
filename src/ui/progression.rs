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
#[cfg(test)]
pub fn compute_next_word_sleep_from_update(upd: &Update) -> Option<Pin<Box<Sleep>>> {
    compute_next_sleep_with_mode(upd, true, false)
}

fn compute_next_sleep_with_mode(
    upd: &Update,
    karaoke_enabled: bool,
    skip_bg: bool,
) -> Option<Pin<Box<Sleep>>> {
    if !upd.playing || upd.lines.is_empty() || !upd.position.is_finite() {
        return None;
    }

    if !karaoke_enabled {
        return schedule_next_line_start(upd, skip_bg);
    }

    let is_richsync = upd
        .provider
        .as_ref()
        .is_some_and(crate::state::Provider::is_word_synced)
        || upd
            .lines
            .iter()
            .any(crate::lyrics::LyricLine::has_word_timings);

    if is_richsync {
        schedule_next_richsync_boundary(upd)
    } else {
        schedule_next_line_start(upd, skip_bg)
    }
}

/// Schedule a wakeup at the next line start (or still-active overlap end when in TUI mode).
fn schedule_next_line_start(upd: &Update, skip_bg: bool) -> Option<Pin<Box<Sleep>>> {
    let start_idx = upd.index.unwrap_or(0);
    let rate = effective_rate(upd);
    let mut best_boundary: Option<f64> = None;

    // In TUI mode, wake when an overlapping earlier line finishes its active vocal window
    // so it transitions to dimmed `before` styling cleanly on time.
    if !skip_bg {
        for line in upd.lines.iter().take(start_idx) {
            if let Some(end) = line.vocal_end()
                && line.time <= upd.position
                && end > upd.position
            {
                best_boundary = Some(best_boundary.map_or(end, |b| b.min(end)));
            }
        }
    }

    // Search from current line onward for next line after current position
    for line in upd.lines.iter().skip(start_idx) {
        if skip_bg && line.is_bg {
            continue;
        }
        if line.time.is_finite() && line.time > upd.position {
            best_boundary = Some(best_boundary.map_or(line.time, |b| b.min(line.time)));
            break;
        }
    }

    best_boundary.map(|t| create_sleep((t - upd.position).max(0.0) / rate))
}

fn collect_line_richsync_delays(
    line: &crate::lyrics::LyricLine,
    position: f64,
    best_delay: &mut Option<f64>,
) {
    if line.time.is_finite() && line.time > position {
        update_best_delay(best_delay, line.time, position);
    }

    if let Some(end) = line.end
        && end.is_finite()
        && end > position
    {
        update_best_delay(best_delay, end, position);
    }

    let Some(words) = &line.words else {
        return;
    };

    for word in words {
        update_best_delay(best_delay, word.start, position);
        update_best_delay(best_delay, word.end, position);

        // Use pre-computed grapheme boundaries (no allocation)
        for &grapheme_boundary in &word.grapheme_times {
            update_best_delay(best_delay, grapheme_boundary, position);
        }
    }
}

/// Schedule a wakeup at the next word/grapheme boundary (richsync).
fn schedule_next_richsync_boundary(upd: &Update) -> Option<Pin<Box<Sleep>>> {
    let start_idx = upd.index.unwrap_or(0);
    let rate = effective_rate(upd);
    let mut best_delay: Option<f64> = None;

    // Include overlapping earlier lines whose vocals are still actively finishing so their
    // trailing syllables keep animating smoothly and dim right at vocal end.
    for line in upd.lines.iter().take(start_idx) {
        if line
            .vocal_end()
            .is_some_and(|end| line.time <= upd.position && end > upd.position)
        {
            collect_line_richsync_delays(line, upd.position, &mut best_delay);
        }
    }

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

        collect_line_richsync_delays(line, upd.position, &mut best_delay);
    }

    best_delay.map(|d| create_sleep(d / rate))
}

fn effective_rate(upd: &Update) -> f64 {
    let rate = upd.time_anchor.rate;
    if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        1.0
    }
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
pub fn estimate_update_and_next_sleep(
    last_update: &Option<Update>,
    last_update_instant: Option<Instant>,
    karaoke_enabled: bool,
) -> (Option<Update>, Option<Pin<Box<Sleep>>>) {
    estimate_update_and_next_sleep_internal(
        last_update,
        last_update_instant,
        karaoke_enabled,
        false,
    )
}

/// Pipe-mode variant of [`estimate_update_and_next_sleep`]: skips word timings and `is_bg` wakeups.
pub fn estimate_update_and_next_sleep_for_pipe(
    last_update: &Option<Update>,
    last_update_instant: Option<Instant>,
) -> (Option<Update>, Option<Pin<Box<Sleep>>>) {
    estimate_update_and_next_sleep_internal(last_update, last_update_instant, false, true)
}

fn estimate_update_and_next_sleep_internal(
    last_update: &Option<Update>,
    last_update_instant: Option<Instant>,
    karaoke_enabled: bool,
    skip_bg: bool,
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

    // Schedule next boundary for smooth rendering (skip grapheme wakeups when karaoke is disabled)
    let next_sleep = compute_next_sleep_with_mode(&estimated, karaoke_enabled, skip_bg);

    (Some(estimated), next_sleep)
}

/// Compute the current line index from position.
///
/// Returns `None` if:
/// - No lyrics
/// - Position is invalid (`!is_finite()`)
/// - Position is before the first line
///
/// Delegates to the shared [`active_line_index`] binary search also used by
/// `LyricState::get_index`, keeping authoritative state and UI estimation in sync.
fn compute_line_index(update: &Update) -> Option<usize> {
    if update.lines.is_empty() || !update.position.is_finite() {
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
                ..Default::default()
            },
        ]));
        let line1 = crate::lyrics::LyricLine::new(10.0, "Line 1").with_words(Some(vec![
            crate::lyrics::types::WordTiming {
                start: 12.0,
                end: 13.0,
                text: "World".into(),
                grapheme_boundaries: vec![0, 5],
                grapheme_times: vec![],
                ..Default::default()
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

    #[tokio::test]
    async fn test_non_karaoke_schedules_line_start_instead_of_word_wakeup() {
        let line0 = crate::lyrics::LyricLine::new(0.0, "Hello world").with_words(Some(vec![
            crate::lyrics::types::create_word_timing(1.0, 2.0, "Hello"),
            crate::lyrics::types::create_word_timing(2.0, 3.0, "world"),
        ]));
        let line1 = crate::lyrics::LyricLine::new(10.0, "Next line");
        let update = Update {
            lines: Arc::new(vec![line0, line1]),
            index: Some(0),
            position: 0.5,
            playing: true,
            ..Default::default()
        };

        let (_, karaoke_sleep) = estimate_update_and_next_sleep(&Some(update.clone()), None, true);
        let (_, line_sleep) = estimate_update_and_next_sleep(&Some(update), None, false);
        // Word boundary is at 1.0s (~0.5s away); line transition is at 10.0s (~9.5s away)
        let now = tokio::time::Instant::now();
        let k_dur = karaoke_sleep
            .unwrap()
            .deadline()
            .saturating_duration_since(now);
        let l_dur = line_sleep
            .unwrap()
            .deadline()
            .saturating_duration_since(now);
        assert!(k_dur < Duration::from_secs(2));
        assert!(l_dur > Duration::from_secs(8));
    }

    #[tokio::test]
    async fn test_pipe_progression_skips_is_bg_wakeups() {
        let lines = vec![
            crate::lyrics::LyricLine::new(0.0, "Lead 0"),
            crate::lyrics::LyricLine::new(2.0, "Echo 0").with_bg(true),
            crate::lyrics::LyricLine::new(10.0, "Lead 1"),
        ];
        let update = Update {
            lines: Arc::new(lines),
            index: Some(0),
            position: 0.5,
            playing: true,
            ..Default::default()
        };
        let (_, sleep) = estimate_update_and_next_sleep_for_pipe(&Some(update), None);
        let dur = sleep
            .unwrap()
            .deadline()
            .saturating_duration_since(tokio::time::Instant::now());
        assert!(
            dur > Duration::from_secs(8),
            "pipe timer should target 10.0s Main line, got {:?}",
            dur
        );
    }

    #[tokio::test]
    async fn test_overlapping_before_richsync_boundary_is_scheduled() {
        let l38 = crate::lyrics::LyricLine::new(170.072, "We had all")
            .with_end(Some(177.443))
            .with_words(Some(vec![
                crate::lyrics::types::create_word_timing(173.177, 174.655, "We"),
                crate::lyrics::types::create_word_timing(174.655, 177.443, "all"),
            ]));
        let l39 = crate::lyrics::LyricLine::new(174.553, "We could")
            .with_end(Some(182.0))
            .with_words(Some(vec![crate::lyrics::types::create_word_timing(
                180.0, 182.0, "could",
            )]));

        let update = Update {
            lines: Arc::new(vec![l38, l39]),
            index: Some(1),
            position: 175.0,
            playing: true,
            ..Default::default()
        };

        // L38's "all" has grapheme boundaries / end (~175.5..177.443) well before L39's 180.0 word!
        let (_, sleep) = estimate_update_and_next_sleep(&Some(update), None, true);
        let dur = sleep
            .unwrap()
            .deadline()
            .saturating_duration_since(tokio::time::Instant::now());
        assert!(
            dur < Duration::from_secs(3),
            "expected wakeup for overlapping L38 grapheme/end boundary, got {:?}",
            dur
        );
    }
}
