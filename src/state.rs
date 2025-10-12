// State module: immutable snapshots and mutable state management for
// synchronized lyrics display.
//
// Architecture:
// - `Update`: immutable snapshot sent to observers (UI, external consumers)
// - `PlayerState`: mutable playback state with position estimation
// - `LyricState`: mutable lyrics with active line tracking
// - `StateBundle`: combines player and lyric state with versioning

use crate::lyrics::LyricLine;
use crate::mpris::TrackMetadata;
use crate::timer::{sanitize_position, PlaybackTimer};
use std::cmp::Ordering;
use std::sync::Arc;

// ============================================================================
// Provider Enumeration
// ============================================================================

/// Identifies the lyrics provider for the current track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Lrclib,
    MusixmatchRichsync,
    MusixmatchSubtitles,
}

impl Provider {
    /// Returns the canonical name of the provider.
    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        match self {
            Provider::Lrclib => "LRCLib",
            Provider::MusixmatchRichsync => "Musixmatch (Richsync)",
            Provider::MusixmatchSubtitles => "Musixmatch (Subtitles)",
        }
    }
}

// ============================================================================
// Update Snapshot
// ============================================================================

/// Immutable state snapshot sent to observers. This struct provides a
/// complete, consistent view of the current lyrics and playback state.
///
/// All observers receive clones of this lightweight structure rather than
/// direct access to mutable state.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// Lyrics lines (shared via Arc for efficient cloning)
    pub lines: Arc<Vec<LyricLine>>,
    
    /// Index of the currently highlighted line (if any)
    pub index: Option<usize>,
    
    /// Current playback position in seconds
    pub position: f64,
    
    /// Error message from the most recent operation (if any)
    pub err: Option<String>,
    
    /// Monotonically increasing version counter for change detection
    pub version: u64,
    
    /// Whether the player is currently playing (true) or paused (false)
    pub playing: bool,
    
    /// Current track artist
    pub artist: String,
    
    /// Current track title
    pub title: String,
    
    /// Current track album
    pub album: String,
    
    /// Provider that supplied the current lyrics
    pub provider: Option<Provider>,
}

impl Default for Update {
    fn default() -> Self {
        Self {
            lines: Arc::new(Vec::new()),
            index: None,
            position: 0.0,
            err: None,
            version: 0,
            playing: false,
            artist: String::new(),
            title: String::new(),
            album: String::new(),
            provider: None,
        }
    }
}

// ============================================================================
// Player State
// ============================================================================

/// Mutable playback state with high-precision position tracking.
///
/// This struct maintains both an anchor position and a monotonic timer to
/// accurately estimate the current playback position between updates.
#[derive(Debug, PartialEq)]
pub struct PlayerState {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    
    /// Most recent anchor position in seconds
    pub position: f64,
    
    /// Error from the most recent operation
    pub err: Option<String>,
    
    /// Track length in seconds (if known)
    pub length: Option<f64>,
    
    /// Internal timer for position estimation during playback
    timer: PlaybackTimer,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            playing: false,
            position: 0.0,
            err: None,
            length: None,
            timer: PlaybackTimer::default(),
        }
    }
}

impl PlayerState {
    /// Updates player state from track metadata, resetting position to zero.
    ///
    /// This should be called when a new track begins playing.
    pub fn update_from_metadata(&mut self, meta: &TrackMetadata) {
        self.title = meta.title.clone();
        self.artist = meta.artist.clone();
        self.album = meta.album.clone();
        self.length = meta.length;
        // Reset anchor and clear the monotonic anchor instant so position
        // estimation starts fresh for the new track (no paused/resumed state
        // carried over). Keep `position` consistent with the anchor.
        self.timer.reset(0.0);
        self.position = 0.0;
        self.err = None;
    }

    /// Updates playback state from an external source (e.g., D-Bus).
    ///
    /// Synchronizes both the anchor position and the playing/paused state.
    pub fn update_playback_dbus(&mut self, playing: bool, position: f64) {
        self.set_position(position);
        
        if playing {
            self.start_playing();
        } else {
            self.pause();
        }
    }

    /// Estimates current playback position using the internal timer.
    ///
    /// Returns a clamped value within [0, length] when length is known.
    /// Falls back to the anchor position if estimation produces NaN.
    pub fn estimate_position(&self) -> f64 {
        let mut estimated = self.timer.estimate(self.playing);
        
        if !estimated.is_finite() {
            estimated = self.position;
        }
        
        if let Some(len) = self.length && estimated.is_finite() {
            estimated = estimated.clamp(0.0, len);
        }
        
        estimated
    }

    /// Checks if the provided metadata represents a different track.
    pub fn has_changed(&self, meta: &TrackMetadata) -> bool {
        self.title != meta.title 
            || self.artist != meta.artist 
            || self.album != meta.album
    }

    /// Sets a new anchor position without changing playback state.
    pub fn set_position(&mut self, position: f64) {
        let pos = sanitize_position(position);
        self.timer.set_position(pos);
        self.position = pos;
    }

    /// Marks the player as playing and starts the internal timer.
    pub fn start_playing(&mut self) {
        if !self.playing {
            self.playing = true;
            self.timer.mark_playing();
        }
    }

    /// Marks the player as paused and stops the internal timer.
    pub fn pause(&mut self) {
        if self.playing {
            self.playing = false;
            self.timer.mark_paused();
        }
    }
}

// ============================================================================
// Lyric State
// ============================================================================

/// Mutable lyrics state with active line tracking.
///
/// Manages a sorted list of timestamped lyric lines and determines which
/// line should be highlighted at any given playback position.
#[derive(Debug)]
pub struct LyricState {
    /// Sorted lyrics lines (shared via Arc)
    pub lines: Arc<Vec<LyricLine>>,
    
    /// Index of the currently highlighted line
    pub index: Option<usize>,
}

impl Default for LyricState {
    fn default() -> Self {
        Self {
            lines: Arc::new(Vec::new()),
            index: None,
        }
    }
}

impl LyricState {
    /// Computes the appropriate line index for the given playback position.
    ///
    /// Returns `None` if:
    /// - No lyrics are loaded
    /// - Position is NaN
    /// - Any line has a NaN timestamp
    /// - Position is before the first line's timestamp
    ///
    /// Uses binary search for efficient lookup in sorted lyrics.
    pub fn get_index(&self, position: f64) -> Option<usize> {
        if self.lines.is_empty() || !position.is_finite() {
            return None;
        }

        // Validate all timestamps are finite
        if self.lines.iter().any(|line| !line.time.is_finite()) {
            return None;
        }

        // Check if position is before first line
        if let Some(first) = self.lines.first() && position < first.time {
            return None;
        }

        // Binary search for the appropriate line
        match self.lines.binary_search_by(|line| {
            line.time
                .partial_cmp(&position)
                .unwrap_or(Ordering::Less)
        }) {
            Ok(exact_match) => Some(exact_match),
            Err(0) => None,
            Err(insert_point) => Some(insert_point - 1),
        }
    }

    /// Replaces lyrics with a new set of lines.
    ///
    /// Performs sanitization:
    /// - Removes lines with NaN timestamps
    /// - Clamps negative timestamps to 0.0
    /// - Sorts lines by timestamp
    ///
    /// Resets the current index since line positions may have changed.
    pub fn update_lines(&mut self, lines: Vec<LyricLine>) {
        let sanitized = Self::sanitize_and_sort(lines);
        self.lines = Arc::new(sanitized);
        self.index = None;
    }

    /// Sanitizes and sorts a collection of lyric lines.
    fn sanitize_and_sort(lines: Vec<LyricLine>) -> Vec<LyricLine> {
        let mut sanitized: Vec<LyricLine> = lines
            .into_iter()
            .filter_map(Self::sanitize_line)
            .collect();

        sanitized.sort_by(|a, b| {
            a.time.partial_cmp(&b.time).unwrap_or(Ordering::Equal)
        });

        sanitized
    }

    /// Sanitizes a single lyric line, returning None for invalid lines.
    fn sanitize_line(mut line: LyricLine) -> Option<LyricLine> {
        if !line.time.is_finite() {
            return None;
        }

        if line.time < 0.0 {
            line.time = 0.0;
        }

        Some(line)
    }

    /// Updates the current index, returning true if it changed.
    pub fn update_index(&mut self, new_index: Option<usize>) -> bool {
        if self.index != new_index {
            self.index = new_index;
            true
        } else {
            false
        }
    }
}

// ============================================================================
// State Bundle
// ============================================================================

/// Combined state container with versioning for change notification.
///
/// Bundles player state, lyric state, and metadata with a monotonically
/// increasing version counter to enable efficient change detection.
#[derive(Debug)]
pub struct StateBundle {
    pub lyric_state: LyricState,
    pub player_state: PlayerState,
    
    /// Monotonically increasing version counter
    pub version: u64,
    
    /// Current lyrics provider
    pub provider: Option<Provider>,
}

impl Default for StateBundle {
    fn default() -> Self {
        Self::new()
    }
}

impl StateBundle {
    /// Creates a new state bundle with default values.
    pub fn new() -> Self {
        Self {
            lyric_state: LyricState::default(),
            player_state: PlayerState::default(),
            version: 0,
            provider: None,
        }
    }

    /// Clears all lyrics and increments the version.
    ///
    /// Also clears the provider to prevent stale information.
    pub fn clear_lyrics(&mut self) {
        self.lyric_state.update_lines(Vec::new());
        self.provider = None;
        self.increment_version();
    }

    /// Updates lyrics, metadata, and error state atomically.
    ///
    /// This is the primary method for loading new lyrics. It:
    /// - Replaces lyric lines
    /// - Updates player metadata
    /// - Sets error state
    /// - Records the provider
    /// - Increments version
    pub fn update_lyrics(
        &mut self,
        lines: Vec<LyricLine>,
        meta: &TrackMetadata,
        err: Option<String>,
        provider: Option<Provider>,
    ) {
        self.lyric_state.update_lines(lines);
        self.player_state.update_from_metadata(meta);
        self.player_state.err = err;
        self.provider = provider;
        self.increment_version();
    }

    /// Updates the active lyric line index based on playback position.
    ///
    /// Increments version and returns true if the index changed.
    pub fn update_index(&mut self, position: f64) -> bool {
        let new_index = self.lyric_state.get_index(position);
        let changed = self.lyric_state.update_index(new_index);
        
        if changed {
            self.increment_version();
        }
        
        changed
    }

    /// Increments the version counter, wrapping on overflow.
    fn increment_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Creates an immutable snapshot of the current state.
    pub fn create_update(&self) -> Update {
        let position = if self.player_state.playing {
            self.player_state.estimate_position()
        } else {
            self.player_state.position
        };
        
        Update {
            lines: Arc::clone(&self.lyric_state.lines),
            index: self.lyric_state.index,
            position,
            err: self.player_state.err.clone(),
            version: self.version,
            playing: self.player_state.playing,
            artist: self.player_state.artist.clone(),
            title: self.player_state.title.clone(),
            album: self.player_state.album.clone(),
            provider: self.provider,
        }
    }
}


// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lyric_index_empty() {
        let state = LyricState::default();
        assert_eq!(state.get_index(5.0), None);
    }

    #[test]
    fn test_lyric_index_before_first() {
        let mut state = LyricState::default();
        state.update_lines(vec![
            LyricLine { time: 10.0, text: "First".into(), words: None },
        ]);
        assert_eq!(state.get_index(5.0), None);
    }

    #[test]
    fn test_lyric_index_basic() {
        let mut state = LyricState::default();
        state.update_lines(vec![
            LyricLine { time: 10.0, text: "First".into(), words: None },
            LyricLine { time: 20.0, text: "Second".into(), words: None },
        ]);
        
        assert_eq!(state.get_index(15.0), Some(0));
        assert_eq!(state.get_index(25.0), Some(1));
    }
}