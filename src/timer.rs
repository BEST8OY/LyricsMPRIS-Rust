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

    #[tokio::test]
    #[ignore = "Live real-time comparison test against active media player"]
    async fn test_live_timer_vs_player() {
        let active = crate::mpris::connection::find_best_active_service(&[], &[], None, None).await;
        let Ok(Some(service)) = active else {
            eprintln!("No active MPRIS player service found; skipping live test.");
            return;
        };

        let status = crate::mpris::playback::get_playback_status(&service).await;
        if status != "Playing" {
            eprintln!(
                "Active player ({service}) is not currently Playing (status: {status}); skipping live test."
            );
            return;
        }

        let Ok(initial_pos) = crate::mpris::playback::get_position(&service).await else {
            eprintln!("Failed to fetch initial position from {service}; skipping live test.");
            return;
        };

        let mut timer = PlaybackTimer::default();
        timer.set_position(initial_pos);
        timer.mark_playing();

        eprintln!("\n=== Live Real-Time Timer vs Player Comparison ===");
        eprintln!("Player service: {service}");
        eprintln!("Anchor position: {initial_pos:.4}s at t0\n");
        eprintln!(
            "{:<6} | {:<16} | {:<16} | {:<12} | {:<10}",
            "Step", "Internal Timer", "Player (D-Bus)", "Diff (ms)", "D-Bus RTT"
        );
        eprintln!(
            "{:-<6}-+-{:-<16}-+-{:-<16}-+-{:-<12}-+-{:-<10}",
            "", "", "", "", ""
        );

        for step in 1..=10 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let query_start = std::time::Instant::now();
            let timer_pos_before = timer.estimate(true);

            let real_pos = match crate::mpris::playback::get_position(&service).await {
                Ok(pos) => pos,
                Err(e) => {
                    eprintln!("D-Bus query failed at step {step}: {e}");
                    break;
                }
            };

            let rtt = query_start.elapsed();
            let timer_pos_after = timer.estimate(true);
            let timer_pos_mid = (timer_pos_before + timer_pos_after) / 2.0;
            let diff_ms = (timer_pos_mid - real_pos) * 1000.0;

            eprintln!(
                "{:<6} | {:<16.4} | {:<16.4} | {:<+12.2} | {:<10.2?}",
                format!("#{step}"),
                timer_pos_mid,
                real_pos,
                diff_ms,
                rtt
            );
        }
        eprintln!("=================================================\n");
    }

    #[tokio::test]
    #[ignore = "Live real-time track transition test against active media player"]
    async fn test_live_track_transition() {
        let active = crate::mpris::connection::find_best_active_service(&[], &[], None, None).await;
        let Ok(Some(service)) = active else {
            eprintln!("No active MPRIS player service found; skipping live test.");
            return;
        };

        let conn = match crate::mpris::connection::get_dbus_conn().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to get D-Bus connection: {e}");
                return;
            }
        };

        let proxy = match crate::mpris::proxies::MediaPlayer2PlayerProxy::builder(&conn)
            .destination(service.as_str())
            .unwrap()
            .build()
            .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to build proxy for {service}: {e}");
                return;
            }
        };

        let initial_meta = match proxy.metadata().await {
            Ok(m) => crate::mpris::metadata::extract_metadata(&m),
            Err(e) => {
                eprintln!("Failed to get initial metadata: {e}");
                return;
            }
        };

        eprintln!("\n=== Live Real-Time Song Transition Test ===");
        eprintln!("Player: {service}");
        eprintln!(
            "Current track: {} - {}",
            initial_meta.artist, initial_meta.title
        );
        eprintln!("Issuing Next() call on player...\n");

        let t_next = std::time::Instant::now();
        if let Err(e) = conn
            .call_method(
                Some(service.as_str()),
                "/org/mpris/MediaPlayer2",
                Some("org.mpris.MediaPlayer2.Player"),
                "Next",
                &(),
            )
            .await
        {
            eprintln!("Next() D-Bus call failed: {e}");
            return;
        }

        // Wait for metadata to change
        let mut new_track = initial_meta.clone();
        let mut switch_latency = std::time::Duration::ZERO;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            if let Ok(meta_map) = proxy.metadata().await {
                let track = crate::mpris::metadata::extract_metadata(&meta_map);
                if track != initial_meta {
                    new_track = track;
                    switch_latency = t_next.elapsed();
                    break;
                }
            }
        }

        if new_track == initial_meta {
            eprintln!("Player did not transition to a new track within 1.5s.");
            return;
        }

        eprintln!(
            "New track detected in {:.1?}: {} - {}",
            switch_latency, new_track.artist, new_track.title
        );

        // Fetch initial position and start PlaybackTimer as handle_new_track does
        let raw_pos = proxy.position().await.unwrap_or(0) as f64 / 1_000_000.0;
        let init_pos = if raw_pos > 0.5 { raw_pos } else { 0.0 };

        let mut timer = PlaybackTimer::default();
        timer.set_position(init_pos);
        timer.mark_playing();

        eprintln!(
            "Initial position reported: {raw_pos:.4}s -> timer initialized at {init_pos:.4}s\n"
        );
        eprintln!(
            "{:<6} | {:<10} | {:<15} | {:<15} | {:<12} | {:<10}",
            "Step", "Elapsed", "Internal Timer", "Player (D-Bus)", "Diff (ms)", "State"
        );
        eprintln!(
            "{:-<6}-+-{:-<10}-+-{:-<15}-+-{:-<15}-+-{:-<12}-+-{:-<10}",
            "", "", "", "", "", ""
        );

        let t_track_start = std::time::Instant::now();
        for step in 1..=20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let elapsed = t_track_start.elapsed();
            let timer_pos = timer.estimate(true);
            let real_pos = match proxy.position().await {
                Ok(us) => us as f64 / 1_000_000.0,
                Err(e) => {
                    eprintln!("Failed to read position at step {step}: {e}");
                    break;
                }
            };
            let diff_ms = (timer_pos - real_pos) * 1000.0;
            let state = if real_pos <= 0.03 {
                "Buffering"
            } else {
                "Playing"
            };

            eprintln!(
                "{:<6} | {:<10.2?} | {:<15.4} | {:<15.4} | {:<+12.1} | {:<10}",
                format!("#{step}"),
                elapsed,
                timer_pos,
                real_pos,
                diff_ms,
                state
            );
        }
        eprintln!("===========================================\n");
    }

    #[tokio::test]
    #[ignore = "Live real-time natural song end transition test against active media player"]
    async fn test_live_natural_song_end_transition() {
        let active = crate::mpris::connection::find_best_active_service(&[], &[], None, None).await;
        let Ok(Some(service)) = active else {
            eprintln!("No active MPRIS player service found; skipping live test.");
            return;
        };

        let conn = match crate::mpris::connection::get_dbus_conn().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to get D-Bus connection: {e}");
                return;
            }
        };

        let proxy = match crate::mpris::proxies::MediaPlayer2PlayerProxy::builder(&conn)
            .destination(service.as_str())
            .unwrap()
            .build()
            .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to build proxy for {service}: {e}");
                return;
            }
        };

        let meta_map = match proxy.metadata().await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("Failed to read metadata: {e}");
                return;
            }
        };
        let track_a = crate::mpris::metadata::extract_metadata(&meta_map);
        let Some(length) = track_a.length else {
            eprintln!("Track does not report length; skipping natural end test.");
            return;
        };

        if length <= 5.0 {
            eprintln!("Track length too short; skipping.");
            return;
        }

        let seek_target = length - 2.5;
        eprintln!("\n=== Live Real-Time Natural Song-End Transition Test ===");
        eprintln!("Player: {service}");
        eprintln!(
            "Current song (Song A): {} - {} (length: {:.2}s)",
            track_a.artist, track_a.title, length
        );
        eprintln!(
            "Seeking via D-Bus SetPosition to {:.2}s (2.5s before end of song)...",
            seek_target
        );

        // Seek to 2.5s before track end using D-Bus SetPosition
        let track_id_str = meta_map
            .get("mpris:trackid")
            .and_then(crate::mpris::metadata::extract_string_or_first_item)
            .unwrap_or_default();
        let obj_path = zvariant::ObjectPath::try_from(track_id_str.as_str())
            .unwrap_or_else(|_| zvariant::ObjectPath::from_static_str("/").unwrap());
        let pos_us = (seek_target * 1_000_000.0) as i64;
        let _ = conn
            .call_method(
                Some(service.as_str()),
                "/org/mpris/MediaPlayer2",
                Some("org.mpris.MediaPlayer2.Player"),
                "SetPosition",
                &(obj_path, pos_us),
            )
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let mut timer_a = PlaybackTimer::default();
        let pos_after_seek = proxy.position().await.unwrap_or(0) as f64 / 1_000_000.0;
        timer_a.set_position(pos_after_seek);
        timer_a.mark_playing();

        eprintln!("\nPlaying out the end of Song A into Song B by itself...");
        eprintln!(
            "{:<6} | {:<10} | {:<28} | {:<12} | {:<12} | {:<8}",
            "Step", "Elapsed", "Track", "Timer (s)", "D-Bus (s)", "Phase"
        );
        eprintln!(
            "{:-<6}-+-{:-<10}-+-{:-<28}-+-{:-<12}-+-{:-<12}-+-{:-<8}",
            "", "", "", "", "", ""
        );

        let t_start = std::time::Instant::now();
        let mut track_changed = false;
        let mut timer_b: Option<PlaybackTimer> = None;
        let mut song_b_start: Option<std::time::Instant> = None;
        let mut track_b = track_a.clone();

        for step in 1..=45 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let elapsed = t_start.elapsed();

            let cur_meta = match proxy.metadata().await {
                Ok(m) => crate::mpris::metadata::extract_metadata(&m),
                Err(_) => track_a.clone(),
            };

            let real_pos = match proxy.position().await {
                Ok(us) => us as f64 / 1_000_000.0,
                Err(_) => 0.0,
            };

            if !track_changed && cur_meta != track_a {
                track_changed = true;
                track_b = cur_meta.clone();
                song_b_start = Some(std::time::Instant::now());

                let mut t_b = PlaybackTimer::default();
                let init_b = if real_pos > 0.5 { real_pos } else { 0.0 };
                t_b.set_position(init_b);
                t_b.mark_playing();
                timer_b = Some(t_b);

                eprintln!(
                    ">>> TRACK CHANGED NATURALLY: {} - {} (init pos: {:.4}s)",
                    track_b.artist, track_b.title, real_pos
                );
            }

            let (active_track_name, timer_val, phase) = if !track_changed {
                let clamped_timer = timer_a.estimate(true).min(length);
                let phase = if (length - real_pos).abs() < 0.8 {
                    "Outro/Pad"
                } else {
                    "Ending"
                };
                (
                    format!("{} - {}", track_a.artist, track_a.title),
                    clamped_timer,
                    phase,
                )
            } else {
                let timer_val = timer_b.as_ref().map(|t| t.estimate(true)).unwrap_or(0.0);
                (
                    format!("{} - {}", track_b.artist, track_b.title),
                    timer_val,
                    "Song B",
                )
            };

            let truncated_title = if active_track_name.len() > 28 {
                format!("{}...", &active_track_name[..25])
            } else {
                active_track_name
            };

            eprintln!(
                "{:<6} | {:<10.2?} | {:<28} | {:<12.4} | {:<12.4} | {:<8}",
                format!("#{step}"),
                elapsed,
                truncated_title,
                timer_val,
                real_pos,
                phase
            );

            // After 10 steps into Song B, finish the test
            if let Some(b_start) = song_b_start
                && b_start.elapsed() > std::time::Duration::from_millis(1200)
            {
                break;
            }
        }
        eprintln!("=======================================================\n");
    }
}
