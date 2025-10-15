//! State management module for synchronized lyrics display.
//!
//! This module provides a clean separation between mutable state and immutable snapshots:
//!
//! ## Core Components
//!
//! - [`Update`]: Immutable snapshot sent to observers (UI, external consumers)
//! - [`PlayerState`]: Mutable playback state with high-precision position estimation
//! - [`LyricState`]: Mutable lyrics container with active line tracking
//! - [`StateBundle`]: Unified state container with atomic versioning
//!
//! ## Design Principles
//!
//! - **Immutability for observers**: All external consumers receive immutable [`Update`] snapshots
//! - **Efficient cloning**: Heavy data (lyrics) is wrapped in [`Arc`] for cheap clones
//! - **Version tracking**: Monotonic version counter enables efficient change detection
//! - **Type safety**: Strong typing prevents invalid state transitions

use crate::lyrics::LyricLine;
use crate::mpris::TrackMetadata;
use crate::timer::{sanitize_position, PlaybackTimer};
use std::cmp::Ordering;
use std::sync::Arc;

// ============================================================================
// Provider Enumeration
// ============================================================================

/// Identifies the lyrics provider for the current track.
///
/// Each variant represents a distinct lyrics source with different capabilities:
/// - [`Provider::Lrclib`]: Community-maintained LRC database
/// - [`Provider::MusixmatchRichsync`]: Word-level synchronized lyrics
/// - [`Provider::MusixmatchSubtitles`]: Line-level synchronized lyrics
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Provider {
    /// LRCLib community lyrics database
    Lrclib,
    /// Musixmatch with word-level timestamps (richsync format)
    MusixmatchRichsync,
    /// Musixmatch with line-level timestamps (subtitle format)
    MusixmatchSubtitles,
}

impl Provider {
    /// Returns the human-readable name of the provider.
    ///
    /// # Examples
    ///
    /// ```
    /// # use lyricsmpris::state::Provider;
    /// assert_eq!(Provider::Lrclib.name(), "LRCLib");
    /// assert_eq!(Provider::MusixmatchRichsync.name(), "Musixmatch (Richsync)");
    /// ```
    #[must_use]
    #[allow(dead_code)]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Lrclib => "LRCLib",
            Self::MusixmatchRichsync => "Musixmatch (Richsync)",
            Self::MusixmatchSubtitles => "Musixmatch (Subtitles)",
        }
    }

    /// Returns a short identifier suitable for logging.
    #[must_use]
    #[allow(dead_code)]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Lrclib => "lrclib",
            Self::MusixmatchRichsync => "musixmatch_richsync",
            Self::MusixmatchSubtitles => "musixmatch_subtitles",
        }
    }
}

// ============================================================================
// Update Snapshot
// ============================================================================

/// Immutable state snapshot sent to observers.
///
/// This struct provides a complete, consistent view of the current lyrics and playback state.
/// All observers receive clones of this lightweight structure rather than direct access to
/// mutable state.
///
/// # Performance
///
/// Cloning is cheap: lyrics are wrapped in [`Arc`], and metadata is typically small strings.
/// The entire structure is designed for efficient broadcast to multiple consumers.
///
/// # Fields
///
/// - `lines`: Sorted lyrics with timestamps (shared reference)
/// - `index`: Currently active line index (if any)
/// - `position`: Current playback position in seconds
/// - `playing`: Playback state (true = playing, false = paused)
/// - `version`: Monotonic counter for change detection
/// - `err`: Error message from the most recent operation
/// - `provider`: Source of the current lyrics
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// Lyrics lines (shared via Arc for efficient cloning)
    pub lines: Arc<Vec<LyricLine>>,
    
    /// Index of the currently highlighted line (if any)
    pub index: Option<usize>,
    
    /// Current playback position in seconds
    pub position: f64,
    
    /// Whether the player is currently playing (true) or paused (false)
    pub playing: bool,
    
    /// Monotonically increasing version counter for change detection
    pub version: u64,
    
    /// Error message from the most recent operation (if any)
    pub err: Option<String>,
    
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
            playing: false,
            version: 0,
            err: None,
            artist: String::new(),
            title: String::new(),
            album: String::new(),
            provider: None,
        }
    }
}

impl Update {
    /// Returns true if this update contains valid lyrics.
    #[must_use]
    #[allow(dead_code)]
    pub fn has_lyrics(&self) -> bool {
        !self.lines.is_empty()
    }

    /// Returns true if an error is present.
    #[must_use]
    #[allow(dead_code)]
    pub const fn has_error(&self) -> bool {
        self.err.is_some()
    }

    /// Returns true if lyrics are present and a line is currently active.
    #[must_use]
    #[allow(dead_code)]
    pub const fn has_active_line(&self) -> bool {
        self.index.is_some()
    }
}

// ============================================================================
// Player State
// ============================================================================

/// Mutable playback state with high-precision position tracking.
///
/// This struct maintains both an anchor position and a monotonic timer to
/// accurately estimate the current playback position between D-Bus updates.
///
/// # Position Estimation
///
/// The position is tracked using a two-tier system:
/// 1. **Anchor position**: Last known position from D-Bus
/// 2. **Monotonic timer**: Tracks elapsed time since anchor when playing
///
/// This approach provides smooth position estimation without constant D-Bus queries.
///
/// # Invariants
///
/// - `position` is always sanitized (no NaN, no negative values)
/// - `length` if present, is always positive and finite
/// - Timer state is synchronized with `playing` flag
#[derive(Debug, PartialEq)]
pub struct PlayerState {
    /// Current track title
    pub title: String,
    
    /// Current track artist
    pub artist: String,
    
    /// Current track album
    pub album: String,
    
    /// Playback state: true if playing, false if paused
    pub playing: bool,
    
    /// Most recent anchor position in seconds (sanitized)
    pub position: f64,
    
    /// Error from the most recent operation (if any)
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
    ///
    /// # Behavior
    ///
    /// - Clears any previous error state
    /// - Resets position to 0.0
    /// - Resets internal timer
    /// - Updates metadata from the provided track
    pub fn update_from_metadata(&mut self, meta: &TrackMetadata) {
        self.title.clone_from(&meta.title);
        self.artist.clone_from(&meta.artist);
        self.album.clone_from(&meta.album);
        self.length = meta.length;
        self.timer.reset(0.0);
        self.position = 0.0;
        self.err = None;
    }

    /// Updates playback state from an external source (e.g., D-Bus).
    ///
    /// Synchronizes both the anchor position and the playing/paused state.
    ///
    /// # Arguments
    ///
    /// * `playing` - True if the player is playing, false if paused
    /// * `position` - Current playback position in seconds
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
    /// # Returns
    ///
    /// The estimated position in seconds, clamped to `[0, length]` if length is known.
    /// Falls back to the anchor position if estimation produces `NaN`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut player = PlayerState::default();
    /// player.set_position(10.0);
    /// player.start_playing();
    /// // ... time passes ...
    /// let current_pos = player.estimate_position(); // > 10.0
    /// ```
    #[must_use]
    pub fn estimate_position(&self) -> f64 {
        let mut estimated = self.timer.estimate(self.playing);
        
        if !estimated.is_finite() {
            estimated = self.position;
        }
        
        if let Some(len) = self.length {
            if estimated.is_finite() {
                estimated = estimated.clamp(0.0, len);
            }
        }
        
        estimated
    }

    /// Checks if the provided metadata represents a different track.
    ///
    /// Compares title, artist, and album to detect track changes.
    #[must_use]
    pub fn has_changed(&self, meta: &TrackMetadata) -> bool {
        self.title != meta.title 
            || self.artist != meta.artist 
            || self.album != meta.album
    }

    /// Sets a new anchor position without changing playback state.
    ///
    /// The position is automatically sanitized (no NaN, no negatives).
    pub fn set_position(&mut self, position: f64) {
        let pos = sanitize_position(position);
        self.timer.set_position(pos);
        self.position = pos;
    }

    /// Marks the player as playing and starts the internal timer.
    ///
    /// This is idempotent: calling multiple times has no additional effect.
    pub fn start_playing(&mut self) {
        if !self.playing {
            self.playing = true;
            self.timer.mark_playing();
        }
    }

    /// Marks the player as paused and stops the internal timer.
    ///
    /// This is idempotent: calling multiple times has no additional effect.
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
///
/// # Invariants
///
/// - Lines are always sorted by timestamp
/// - All timestamps are finite (no NaN)
/// - Negative timestamps are clamped to 0.0
///
/// # Performance
///
/// Uses binary search for O(log n) line lookup by timestamp.
#[derive(Debug)]
pub struct LyricState {
    /// Sorted lyrics lines (shared via Arc for cheap cloning)
    pub lines: Arc<Vec<LyricLine>>,
    
    /// Index of the currently highlighted line (if any)
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
    /// - Any line has a NaN timestamp (defensive check)
    /// - Position is before the first line's timestamp
    ///
    /// # Performance
    ///
    /// Uses binary search for O(log n) lookup in sorted lyrics.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let state = LyricState { /* ... */ };
    /// let index = state.get_index(15.5); // Returns index of active line at 15.5s
    /// ```
    #[must_use]
    pub fn get_index(&self, position: f64) -> Option<usize> {
        // Early returns for invalid input
        if self.lines.is_empty() || !position.is_finite() {
            return None;
        }

        // Validate all timestamps are finite (defensive check)
        if self.lines.iter().any(|line| !line.time.is_finite()) {
            return None;
        }

        // Check if position is before first line
        let first = self.lines.first()?;
        if position < first.time {
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
    /// Performs automatic sanitization:
    /// - Removes lines with NaN or infinite timestamps
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
    ///
    /// This is a pure function that doesn't mutate state.
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

    /// Sanitizes a single lyric line, returning `None` for invalid lines.
    ///
    /// Invalid lines have NaN or infinite timestamps.
    /// Negative timestamps are clamped to 0.0.
    fn sanitize_line(mut line: LyricLine) -> Option<LyricLine> {
        if !line.time.is_finite() {
            return None;
        }

        if line.time < 0.0 {
            line.time = 0.0;
        }

        Some(line)
    }

    /// Updates the current index, returning `true` if it changed.
    ///
    /// This is used to track state changes for efficient UI updates.
    pub fn update_index(&mut self, new_index: Option<usize>) -> bool {
        let changed = self.index != new_index;
        if changed {
            self.index = new_index;
        }
        changed
    }

    /// Returns the number of lyrics lines.
    #[must_use]
    #[allow(dead_code)]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Returns `true` if no lyrics are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

// ============================================================================
// State Bundle
// ============================================================================

/// Combined state container with versioning for change notification.
///
/// Bundles player state, lyric state, and metadata with a monotonically
/// increasing version counter to enable efficient change detection.
///
/// # Architecture
///
/// This is the primary mutable state container used by the event loop.
/// It aggregates [`PlayerState`] and [`LyricState`] while maintaining
/// a version counter for change tracking.
///
/// # Version Management
///
/// The version counter is incremented atomically whenever state changes.
/// Consumers can use this to detect whether an update is necessary:
///
/// ```ignore
/// let old_version = bundle.version;
/// // ... modify state ...
/// assert!(bundle.version > old_version);
/// ```
#[derive(Debug)]
pub struct StateBundle {
    /// Lyrics state with active line tracking
    pub lyric_state: LyricState,
    
    /// Player state with position estimation
    pub player_state: PlayerState,
    
    /// Monotonically increasing version counter
    pub version: u64,
    
    /// Current lyrics provider (if lyrics are loaded)
    pub provider: Option<Provider>,
}

impl Default for StateBundle {
    fn default() -> Self {
        Self::new()
    }
}

impl StateBundle {
    /// Creates a new state bundle with default values.
    #[must_use]
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
    ///
    /// # Use Cases
    ///
    /// - Player stopped or disconnected
    /// - Switching to a track with no available lyrics
    /// - Reset on error conditions
    pub fn clear_lyrics(&mut self) {
        self.lyric_state.update_lines(Vec::new());
        self.provider = None;
        self.increment_version();
    }

    /// Updates lyrics, metadata, and error state atomically.
    ///
    /// This is the primary method for loading new lyrics. It performs
    /// multiple state updates atomically with a single version increment.
    ///
    /// # Operations
    ///
    /// 1. Replaces lyric lines (sanitizing and sorting)
    /// 2. Updates player metadata
    /// 3. Sets error state
    /// 4. Records the provider
    /// 5. Increments version once
    ///
    /// # Arguments
    ///
    /// * `lines` - New lyrics lines (will be sanitized and sorted)
    /// * `meta` - Track metadata
    /// * `err` - Optional error message
    /// * `provider` - Source of the lyrics
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
    /// Increments version and returns `true` if the index changed.
    ///
    /// # Performance
    ///
    /// Uses binary search for O(log n) lookup.
    pub fn update_index(&mut self, position: f64) -> bool {
        let new_index = self.lyric_state.get_index(position);
        let changed = self.lyric_state.update_index(new_index);
        
        if changed {
            self.increment_version();
        }
        
        changed
    }

    /// Increments the version counter, wrapping on overflow.
    ///
    /// This is called automatically by state-modifying methods.
    fn increment_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Creates an immutable snapshot of the current state.
    ///
    /// This is the primary way to export state to observers. The resulting
    /// [`Update`] struct is cheap to clone due to [`Arc`]-wrapped lyrics.
    ///
    /// # Position Handling
    ///
    /// If playing, uses estimated position (anchor + elapsed time).
    /// If paused, uses the anchor position directly.
    #[must_use]
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
            playing: self.player_state.playing,
            version: self.version,
            err: self.player_state.err.clone(),
            artist: self.player_state.artist.clone(),
            title: self.player_state.title.clone(),
            album: self.player_state.album.clone(),
            provider: self.provider,
        }
    }

    /// Returns `true` if lyrics are currently loaded.
    #[must_use]
    pub fn has_lyrics(&self) -> bool {
        !self.lyric_state.is_empty()
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