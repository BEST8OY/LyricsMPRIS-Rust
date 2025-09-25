use std::time::Instant;

/// Small helper to keep timing logic isolated. Public so other modules can use
/// the same behavior when needed.
#[derive(Debug, PartialEq, Default)]
pub struct PlaybackTimer {
    /// Anchor position in seconds (finite, >= 0 if possible).
    anchor_position: f64,
    /// Monotonic instant corresponding to `anchor_position`.
    anchor_instant: Option<Instant>,
}

impl PlaybackTimer {
    pub fn reset(&mut self, position: f64) {
        self.anchor_position = sanitize_position(position);
        // Reset timer anchor. Do not assume playback state here; callers
        // should call `mark_playing`/`mark_paused` to control whether the
        // monotonic anchor is active. Setting the instant here is harmless
        // but can lead to incorrect estimates if callers rely on pause
        // semantics, so keep it deterministic and clear by setting to None.
        self.anchor_instant = None;
    }
    pub fn set_position(&mut self, position: f64) {
        self.anchor_position = sanitize_position(position);
        // Refresh the monotonic anchor so subsequent estimates are relative
        // to this observed position. This prevents double-counting when
        // callers sample the estimated position and write it back (see
        // `handle_position_sync` in `event.rs`). Caller may still call
        // `mark_paused` to clear the running anchor when pausing.
        self.anchor_instant = Some(Instant::now());
    }
    pub fn mark_playing(&mut self) {
        // Always refresh the anchor instant when playback starts or resumes
        // so elapsed time is measured from the resume moment. This prevents
        // paused-duration from being included in estimates.
        self.anchor_instant = Some(Instant::now());
    }
    pub fn mark_paused(&mut self) {
        // When paused we clear the monotonic anchor so calls to `estimate`
        // (which should check the playing flag) will return the anchor
        // position only. Clearing prevents paused wall-clock time from
        // being added after a resume if callers forget to update the
        // instant.
        self.anchor_instant = None;
    }
    pub fn estimate(&self, playing: bool) -> f64 {
        let base = self.anchor_position;
        if !playing {
            return base;
        }
        if let Some(inst) = self.anchor_instant {
            let elapsed = inst.elapsed().as_secs_f64();
            let mut val = base + elapsed;
            if !val.is_finite() {
                val = base;
            }
            val
        } else {
            base
        }
    }
}

pub fn sanitize_position(p: f64) -> f64 {
    if p.is_nan() || !p.is_finite() {
        0.0
    } else if p < 0.0 {
        // Negative positions are not meaningful; clamp to zero.
        0.0
    } else {
        p
    }
}