// Minimal state data structures for lyrics and player

use crate::lyrics::LyricLine;
use crate::mpris::TrackMetadata;
use std::sync::Arc;
use std::cmp::Ordering;
use crate::timer::{PlaybackTimer, sanitize_position};

/// Which provider supplied the current lyrics.
#[derive(Debug, Clone, PartialEq)]
pub enum Provider {
    Lrclib,
    MusixmatchRichsync,
    MusixmatchSubtitles,
    
}

/// Update sent to the UI: a snapshot of lyrics, player position and metadata.
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// Provider that supplied the current lyrics.
    pub provider: Option<Provider>,
}

#[derive(Debug, PartialEq, Default)]
pub struct PlayerState {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    /// Last emitted/stored position in seconds. Consumers still read/write this
    /// field directly in a few places so we keep it for backward compatibility.
    pub position: f64,
    pub err: Option<String>,
    /// Internal high-precision timer to track position while playing. This
    /// uses `Instant` (monotonic clock) and isolates timing concerns so the
    /// rest of the codebase can use simple seconds (`f64`). The timer is
    /// intentionally lightweight and deterministic.
    timer: PlaybackTimer,
    /// Known track length in seconds (used to clamp estimates).
    pub length: Option<f64>,
}

impl PlayerState {
    pub fn update_from_metadata(&mut self, meta: &TrackMetadata) {
        self.title = meta.title.clone();
        self.artist = meta.artist.clone();
        self.album = meta.album.clone();
        self.length = meta.length;
        self.position = 0.0;
        self.err = None;
        // Reset the internal timer to the start of the track.
        self.timer.reset(0.0);
    }
    pub fn update_playback_dbus(&mut self, playing: bool, position: f64) {
        // Normalize incoming position and update timer anchor.
        let pos = sanitize_position(position);
        self.timer.set_position(pos);
        self.position = pos;
        // Use the convenience helpers so the playing flag and timer state
        // remain consistent and centralized.
        if playing {
            self.start_playing();
        } else {
            self.pause();
        }
    }
    pub fn estimate_position(&self) -> f64 {
        // Use the internal timer to produce an estimate. This keeps timing
        // logic in one place and ensures we always use the monotonic clock.
        let mut estimated = self.timer.estimate(self.playing);
        // Clamp to track length if available.
        if let Some(len) = self.length && estimated.is_finite() {
            if estimated > len { estimated = len; }
            if estimated < 0.0 { estimated = 0.0; }
        }
        estimated
    }
    pub fn has_changed(&self, meta: &TrackMetadata) -> bool {
        self.title != meta.title || self.artist != meta.artist || self.album != meta.album
    }
    /// Set a new anchor position without changing playback state.
    pub fn set_position(&mut self, position: f64) {
        let pos = sanitize_position(position);
        self.timer.set_position(pos);
        self.position = pos;
    }
    /// Start playback (does not modify anchor position).
    pub fn start_playing(&mut self) {
        self.playing = true;
        self.timer.mark_playing();
    }

    /// Pause playback (does not modify anchor position).
    pub fn pause(&mut self) {
        self.playing = false;
        self.timer.mark_paused();
    }
}

#[derive(Debug, Default)]
pub struct LyricState {
    pub lines: Arc<Vec<LyricLine>>,
    pub index: Option<usize>,
}

impl LyricState {
    /// Compute the current lyric index for a given playback `position`.
    /// Returns `None` when no line should be considered active yet (e.g.
    /// position is before the first timestamp or there are no valid lines).
    pub fn get_index(&self, position: f64) -> Option<usize> {
        // No lines -> no index
        if self.lines.is_empty() {
            return None;
        }

        if position.is_nan() || self.lines.iter().any(|line| line.time.is_nan()) {
            return None;
        }

        // If position is before the first timestamp, return None so the UI
        // doesn't pre-highlight the first line.
        if let Some(first) = self.lines.first() && position < first.time {
            return None;
        }

        // binary_search_by returns Ok(idx) when exact match found, or Err(insert)
        // where the correct index is insert - 1 (unless insert == 0, which we
        // already handled by the early-return above).
        match self.lines.binary_search_by(|line| {
            line.time
                .partial_cmp(&position)
                .unwrap_or(Ordering::Less)
        }) {
            Ok(idx) => Some(idx),
            Err(0) => None,
            Err(idx) => Some(idx - 1),
        }
    }
    pub fn update_lines(&mut self, lines: Vec<LyricLine>) {
        // Sanitize incoming lines to ensure they are safe for binary search
        // and UI rendering. This enforces a non-decreasing time order, removes
        // NaN times and clamps negative times to 0.0. Keeping this logic in
        // the central state ensures all providers don't need to duplicate it.
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

        // Sort by time to satisfy binary_search expectations. Use partial_cmp
        // and treat incomparable values as equal (they've already been filtered).
        sanitized.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));

        // Clear the current index: we don't assume the first line should be
        // active until playback position reaches its timestamp.
        self.index = None;
        self.lines = Arc::new(sanitized);
    }
    pub fn update_index(&mut self, new_index: Option<usize>) -> bool {
        // Update the stored index (Option semantics). Returns true when changed.
        let changed = new_index != self.index;
        if changed {
            self.index = new_index;
        }
        changed
    }
}

#[derive(Debug, Default)]
pub struct StateBundle {
    pub lyric_state: LyricState,
    pub player_state: PlayerState,
    pub version: u64,
    /// Provider that supplied current lyrics
    pub provider: Option<Provider>,
}

impl StateBundle {
    /// Create a new default `StateBundle`.
    pub fn new() -> Self {
        Default::default()
    }

    /// Remove current lyrics and bump the state version. Also clears the
    /// associated provider to avoid stale values being exposed.
    pub fn clear_lyrics(&mut self) {
        self.lyric_state.update_lines(Vec::new());
        // `update_lines` resets index to 0 already.
        self.version = self.version.wrapping_add(1);
        self.provider = None;
    }
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
        // Bump version to indicate a state change.
        self.version = self.version.wrapping_add(1);
        // Store provider so callers can know which provider supplied the current lyrics
        self.provider = provider;
    }
    pub fn update_index(&mut self, position: f64) -> bool {
        let new_index = self.lyric_state.get_index(position);
        let changed = self.lyric_state.update_index(new_index);
        if changed {
            self.version = self.version.wrapping_add(1);
        }
        changed
    }
}