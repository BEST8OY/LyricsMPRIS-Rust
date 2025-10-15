//! High-precision playback position tracking.
//!
//! This module provides [`PlaybackTimer`], a utility for estimating playback position
//! between D-Bus updates using monotonic time tracking.
//!
//! # Design
//!
//! The timer maintains two pieces of state:
//! - **Anchor position**: Last known position from D-Bus (in seconds)
//! - **Anchor instant**: Monotonic timestamp when the anchor was set
//!
//! When playing, position is estimated as: `anchor_position + elapsed_time`.
//! When paused, the anchor position is returned directly.
//!
//! # Invariants
//!
//! - Anchor position is always sanitized (finite, non-negative)
//! - Anchor instant is `None` when paused or uninitialized
//! - Position estimates are always finite (fallback to anchor if NaN)

use std::time::Instant;

/// High-precision playback position tracker.
///
/// This struct combines a position anchor (from D-Bus) with a monotonic timer
/// to provide smooth position estimation during playback without constant queries.
///
/// # Thread Safety
///
/// This struct is `!Send` and `!Sync` due to `Instant`. Use one per thread.
///
/// # Example
///
/// ```
/// # use lyricsmpris::timer::PlaybackTimer;
/// let mut timer = PlaybackTimer::default();
/// timer.set_position(10.0);
/// timer.mark_playing();
/// 
/// // ... time passes ...
/// let estimated = timer.estimate(true); // > 10.0
/// ```
#[derive(Debug, PartialEq, Default)]
pub struct PlaybackTimer {
    /// Anchor position in seconds (sanitized: finite, >= 0).
    anchor_position: f64,
    /// Monotonic instant corresponding to `anchor_position`.
    /// `None` when paused or before first playback start.
    anchor_instant: Option<Instant>,
}

impl PlaybackTimer {
    /// Resets the timer to a specific position without starting playback.
    ///
    /// This clears the monotonic anchor, so subsequent estimates will return
    /// the anchor position until [`mark_playing`](Self::mark_playing) is called.
    ///
    /// # Use Cases
    ///
    /// - New track starts (position = 0)
    /// - Track metadata changes
    /// - Resetting state to a known position
    ///
    /// # Arguments
    ///
    /// * `position` - New anchor position (will be sanitized)
    ///
    /// # Examples
    ///
    /// ```
    /// # use lyricsmpris::timer::PlaybackTimer;
    /// let mut timer = PlaybackTimer::default();
    /// timer.reset(5.0);
    /// assert_eq!(timer.estimate(false), 5.0);
    /// assert_eq!(timer.estimate(true), 5.0); // No instant set yet
    /// ```
    pub fn reset(&mut self, position: f64) {
        self.anchor_position = sanitize_position(position);
        // Clear the monotonic anchor. Callers should call `mark_playing()`
        // or `mark_paused()` to set the playback state explicitly.
        self.anchor_instant = None;
    }

    /// Sets a new anchor position and refreshes the monotonic instant.
    ///
    /// This is typically called when receiving a position update from D-Bus
    /// while playback is active. The monotonic instant is refreshed to prevent
    /// double-counting elapsed time.
    ///
    /// # Behavior
    ///
    /// - Position is sanitized (no NaN, no negatives)
    /// - Monotonic instant is set to current time
    /// - Subsequent estimates are relative to this moment
    ///
    /// # Arguments
    ///
    /// * `position` - New anchor position (will be sanitized)
    ///
    /// # Examples
    ///
    /// ```
    /// # use lyricsmpris::timer::PlaybackTimer;
    /// let mut timer = PlaybackTimer::default();
    /// timer.set_position(10.0);
    /// // Instant is now set, so estimates will grow from 10.0
    /// ```
    pub fn set_position(&mut self, position: f64) {
        self.anchor_position = sanitize_position(position);
        // Refresh the monotonic anchor so subsequent estimates are relative
        // to this observed position. This prevents double-counting when
        // callers sample the estimated position and write it back.
        self.anchor_instant = Some(Instant::now());
    }

    /// Marks the start or resumption of playback.
    ///
    /// Refreshes the monotonic instant to measure elapsed time from this moment.
    /// This prevents paused duration from being included in position estimates.
    ///
    /// # Idempotence
    ///
    /// Safe to call multiple times while playing (updates instant each time).
    ///
    /// # Examples
    ///
    /// ```
    /// # use lyricsmpris::timer::PlaybackTimer;
    /// let mut timer = PlaybackTimer::default();
    /// timer.set_position(5.0);
    /// timer.mark_playing();
    /// // Position estimates now grow from 5.0
    /// ```
    pub fn mark_playing(&mut self) {
        // Always refresh the anchor instant when playback starts or resumes
        // so elapsed time is measured from the resume moment. This prevents
        // paused duration from being included in estimates.
        self.anchor_instant = Some(Instant::now());
    }

    /// Marks playback as paused.
    ///
    /// Clears the monotonic anchor so that [`estimate`](Self::estimate) returns
    /// only the anchor position (no time progression).
    ///
    /// # Behavior
    ///
    /// After calling this, `estimate()` will return `anchor_position` regardless
    /// of the `playing` parameter, until [`mark_playing`](Self::mark_playing) is called.
    ///
    /// # Idempotence
    ///
    /// Safe to call multiple times while paused (no-op after first call).
    ///
    /// # Examples
    ///
    /// ```
    /// # use lyricsmpris::timer::PlaybackTimer;
    /// let mut timer = PlaybackTimer::default();
    /// timer.set_position(10.0);
    /// timer.mark_playing();
    /// // ... time passes ...
    /// timer.mark_paused();
    /// // Now estimate() returns the anchor position, frozen in time
    /// ```
    pub fn mark_paused(&mut self) {
        // When paused, clear the monotonic anchor so calls to `estimate()`
        // will return only the anchor position. Clearing prevents paused
        // wall-clock time from being incorrectly added after a resume.
        self.anchor_instant = None;
    }

    /// Estimates the current playback position.
    ///
    /// # Behavior
    ///
    /// - **If paused**: Returns anchor position
    /// - **If playing with instant**: Returns `anchor + elapsed_time`
    /// - **If playing without instant**: Returns anchor position
    /// - **If result is NaN/infinite**: Returns anchor position (fallback)
    ///
    /// # Arguments
    ///
    /// * `playing` - Whether playback is active
    ///
    /// # Returns
    ///
    /// Estimated position in seconds (always finite).
    ///
    /// # Examples
    ///
    /// ```
    /// # use lyricsmpris::timer::PlaybackTimer;
    /// # use std::thread::sleep;
    /// # use std::time::Duration;
    /// let mut timer = PlaybackTimer::default();
    /// timer.set_position(5.0);
    /// timer.mark_playing();
    /// 
    /// // Paused: returns anchor
    /// assert_eq!(timer.estimate(false), 5.0);
    /// 
    /// // Playing: returns anchor + elapsed (increases over time)
    /// sleep(Duration::from_millis(10));
    /// assert!(timer.estimate(true) > 5.0);
    /// ```
    #[must_use]
    pub fn estimate(&self, playing: bool) -> f64 {
        let base = self.anchor_position;
        
        if !playing {
            return base;
        }
        
        let Some(instant) = self.anchor_instant else {
            return base;
        };

        let elapsed = instant.elapsed().as_secs_f64();
        let estimated = base + elapsed;
        
        // Fallback to base if arithmetic produces invalid result
        if estimated.is_finite() {
            estimated
        } else {
            base
        }
    }

    /// Returns the current anchor position (without time progression).
    ///
    /// This is the last position set via [`set_position`](Self::set_position)
    /// or [`reset`](Self::reset).
    #[must_use]
    #[allow(dead_code)]
    pub const fn anchor_position(&self) -> f64 {
        self.anchor_position
    }
}

/// Sanitizes a position value to ensure it's valid for playback tracking.
///
/// # Sanitization Rules
///
/// - `NaN` → `0.0`
/// - `Infinity` / `-Infinity` → `0.0`
/// - Negative values → `0.0`
/// - Valid positive values → unchanged
///
/// # Arguments
///
/// * `position` - Raw position value (may be invalid)
///
/// # Returns
///
/// Sanitized position (finite, non-negative).
///
/// # Examples
///
/// ```
/// # use lyricsmpris::timer::sanitize_position;
/// assert_eq!(sanitize_position(5.0), 5.0);
/// assert_eq!(sanitize_position(-1.0), 0.0);
/// assert_eq!(sanitize_position(f64::NAN), 0.0);
/// assert_eq!(sanitize_position(f64::INFINITY), 0.0);
/// ```
#[must_use]
#[inline]
pub fn sanitize_position(position: f64) -> f64 {
    if !position.is_finite() || position < 0.0 {
        0.0
    } else {
        position
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn test_sanitize_position() {
        assert_eq!(sanitize_position(5.0), 5.0);
        assert_eq!(sanitize_position(0.0), 0.0);
        assert_eq!(sanitize_position(-1.0), 0.0);
        assert_eq!(sanitize_position(-100.0), 0.0);
        assert_eq!(sanitize_position(f64::NAN), 0.0);
        assert_eq!(sanitize_position(f64::INFINITY), 0.0);
        assert_eq!(sanitize_position(f64::NEG_INFINITY), 0.0);
    }

    #[test]
    fn test_timer_reset() {
        let mut timer = PlaybackTimer::default();
        timer.reset(10.0);
        
        // Should return anchor position when not playing
        assert_eq!(timer.estimate(false), 10.0);
        
        // Should return anchor position when playing without instant
        assert_eq!(timer.estimate(true), 10.0);
    }

    #[test]
    fn test_timer_set_position() {
        let mut timer = PlaybackTimer::default();
        timer.set_position(5.0);
        
        // Instant is set, so estimate should be >= anchor
        let estimate = timer.estimate(true);
        assert!(estimate >= 5.0);
    }

    #[test]
    fn test_timer_playing_paused() {
        let mut timer = PlaybackTimer::default();
        timer.set_position(10.0);
        timer.mark_playing();
        
        sleep(Duration::from_millis(10));
        let playing_estimate = timer.estimate(true);
        assert!(playing_estimate > 10.0, "Should advance when playing");
        
        timer.mark_paused();
        let paused_estimate = timer.estimate(true);
        assert_eq!(paused_estimate, 10.0, "Should freeze when paused");
    }

    #[test]
    fn test_timer_invalid_position() {
        let mut timer = PlaybackTimer::default();
        
        // NaN should be sanitized to 0.0
        timer.set_position(f64::NAN);
        assert_eq!(timer.estimate(false), 0.0);
        
        // Negative should be sanitized to 0.0
        timer.set_position(-5.0);
        assert_eq!(timer.estimate(false), 0.0);
        
        // Infinity should be sanitized to 0.0
        timer.set_position(f64::INFINITY);
        assert_eq!(timer.estimate(false), 0.0);
    }

    #[test]
    fn test_timer_anchor_position() {
        let mut timer = PlaybackTimer::default();
        timer.set_position(42.0);
        assert_eq!(timer.anchor_position(), 42.0);
    }
}