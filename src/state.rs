// State module: compact, safe, and well-documented structures that represent
// the current lyrics and player state used by the UI and other components.

use crate::lyrics::LyricLine;
use crate::mpris::TrackMetadata;
use crate::timer::{sanitize_position, PlaybackTimer};
use std::cmp::Ordering;
use std::sync::Arc;

/// Provider that supplied the currently loaded lyrics.
/// Kept small and stable so other modules can match on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    Lrclib,
    MusixmatchRichsync,
    MusixmatchSubtitles,
}

/// Snapshot passed to the UI layer. This is an immutable view of the
/// important pieces of state: the lines, the currently highlighted index,
/// the estimated playback position and some metadata.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Update {
    pub lines: Arc<Vec<LyricLine>>,
    pub index: Option<usize>,
    pub position: f64,
    pub err: Option<String>,
    pub version: u64,
    pub playing: bool,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub provider: Option<Provider>,
}

/// Current state of the audio player. The struct keeps a high-precision
/// internal `PlaybackTimer` to estimate the current position when playing.
#[derive(Debug, PartialEq, Default)]
pub struct PlayerState {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    /// Last stored anchor position (seconds). Some call sites still read this
    /// directly so we keep it for compatibility.
    pub position: f64,
    pub err: Option<String>,
    /// Internal monotonic timer used to estimate position while playing.
    timer: PlaybackTimer,
    /// Known track length in seconds, used to clamp estimated positions.
    pub length: Option<f64>,
}

impl PlayerState {
    /// Apply new track metadata and reset position/timer to start.
    pub fn update_from_metadata(&mut self, meta: &TrackMetadata) {
        self.title = meta.title.clone();
        self.artist = meta.artist.clone();
        self.album = meta.album.clone();
        self.length = meta.length;
        self.position = 0.0;
        self.err = None;
        self.timer.reset(0.0);
    }

    /// Update playback state coming from DBus (or another external source).
    /// This sets the anchor position and adjusts the playing flag/timer
    /// consistently.
    pub fn update_playback_dbus(&mut self, playing: bool, position: f64) {
        let pos = sanitize_position(position);
        self.timer.set_position(pos);
        self.position = pos;
        if playing {
            self.start_playing();
        } else {
            self.pause();
        }
    }

    /// Estimate the current playback position in seconds using the internal
    /// timer. The returned value is clamped to [0, length] when length is
    /// known and finite.
    pub fn estimate_position(&self) -> f64 {
        let mut estimated = self.timer.estimate(self.playing);
        if estimated.is_nan() {
            // Keep a deterministic, finite fallback.
            estimated = self.position;
        }
        if let Some(len) = self.length {
            if estimated.is_finite() {
                if estimated > len {
                    estimated = len;
                }
                if estimated < 0.0 {
                    estimated = 0.0;
                }
            }
        }
        estimated
    }

    /// Returns true when the provided `meta` differs from the stored
    /// title/artist/album. Used to detect track changes.
    pub fn has_changed(&self, meta: &TrackMetadata) -> bool {
        self.title != meta.title || self.artist != meta.artist || self.album != meta.album
    }

    /// Set a new anchor position without changing whether the player is
    /// considered playing. The timer anchor and the stored `position` are
    /// updated together.
    pub fn set_position(&mut self, position: f64) {
        let pos = sanitize_position(position);
        self.timer.set_position(pos);
        self.position = pos;
    }

    /// Mark the player as playing. This flips the internal timer into the
    /// running state.
    pub fn start_playing(&mut self) {
        self.playing = true;
        self.timer.mark_playing();
    }

    /// Mark the player as paused. Timer is paused but the anchor position
    /// remains unchanged.
    pub fn pause(&mut self) {
        self.playing = false;
        self.timer.mark_paused();
    }
}

/// Holds the list of lyric lines and the currently highlighted index.
#[derive(Debug, Default)]
pub struct LyricState {
    pub lines: Arc<Vec<LyricLine>>,
    pub index: Option<usize>,
}

impl LyricState {
    /// Compute the index for `position`. Returns `None` when no line should
    /// be active (no lines, position before first timestamp, or NaNs).
    pub fn get_index(&self, position: f64) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        if position.is_nan() {
            return None;
        }
        // If any line has NaN time treat the whole set as invalid.
        if self.lines.iter().any(|l| l.time.is_nan()) {
            return None;
        }

        // If position is before the first timestamp, don't highlight anything.
        if let Some(first) = self.lines.first() {
            if position < first.time {
                return None;
            }
        }

        // binary_search_by returns Ok(idx) when exact match, or Err(insert).
        // When Err(insert) we want the previous line index (insert - 1).
        match self.lines.binary_search_by(|line| {
            line.time
                .partial_cmp(&position)
                .unwrap_or(Ordering::Less)
        }) {
            Ok(idx) => Some(idx),
            Err(0) => None,
            Err(insert) => Some(insert - 1),
        }
    }

    /// Replace the current lines with a sanitized, sorted list. Removes lines
    /// with NaN times and clamps negative times to 0.0. The current index is
    /// cleared because the position may no longer correspond to the same
    /// line set.
    pub fn update_lines(&mut self, lines: Vec<LyricLine>) {
        let mut sanitized: Vec<LyricLine> = lines
            .into_iter()
            .filter_map(|mut l| {
                if l.time.is_nan() {
                    return None;
                }
                if l.time < 0.0 {
                    l.time = 0.0;
                }
                Some(l)
            })
            .collect();

        sanitized.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(Ordering::Equal));

        self.lines = Arc::new(sanitized);
        self.index = None;
    }

    /// Update the stored index; returns true when it actually changed.
    pub fn update_index(&mut self, new_index: Option<usize>) -> bool {
        if self.index != new_index {
            self.index = new_index;
            true
        } else {
            false
        }
    }
}

/// Bundle holding both lyric and player state plus a monotonically
/// incrementing `version` used to indicate changes to observers.
#[derive(Debug, Default)]
pub struct StateBundle {
    pub lyric_state: LyricState,
    pub player_state: PlayerState,
    pub version: u64,
    pub provider: Option<Provider>,
}

impl StateBundle {
    /// Convenience constructor.
    pub fn new() -> Self {
        Default::default()
    }

    /// Clear loaded lyrics and bump `version`. Also clears the provider so
    /// callers don't observe stale provider info.
    pub fn clear_lyrics(&mut self) {
        self.lyric_state.update_lines(Vec::new());
        self.version = self.version.wrapping_add(1);
        self.provider = None;
    }

    /// Load new lyrics and associated metadata. This sets the player's
    /// metadata (resetting timer/position), stores an optional error, sets
    /// the provider, and advances the version.
    pub fn update_lyrics(
        &mut self,
        lines: Vec<LyricLine>,
        meta: &TrackMetadata,
        err: Option<String>,
        provider: Option<Provider>,
    ) {
        self.lyric_state.update_lines(lines);
        self.player_state.err = err;
        self.player_state.update_from_metadata(meta);
        self.provider = provider;
        self.version = self.version.wrapping_add(1);
    }

    /// Recompute the lyric index for `position`. If the index changes the
    /// bundle `version` is bumped and `true` is returned.
    pub fn update_index(&mut self, position: f64) -> bool {
        let new_index = self.lyric_state.get_index(position);
        let changed = self.lyric_state.update_index(new_index);
        if changed {
            self.version = self.version.wrapping_add(1);
        }
        changed
    }
}