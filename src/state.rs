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
use crate::timer::{PlaybackTimer, TimeAnchor, sanitize_position};
use std::sync::Arc;

// ============================================================================
// Provider Enumeration
// ============================================================================

/// Identifies the lyrics provider for the current track.
///
/// Each variant represents a distinct lyrics source with different capabilities:
/// - [`Provider::Lrclib`]: LRCLIB database (returns LRC timestamp format)
/// - [`Provider::MusixmatchRichsync`]: Word-level synchronized lyrics (JSON)
/// - [`Provider::MusixmatchSubtitles`]: Line-level synchronized lyrics (JSON)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Provider {
    /// LRCLIB provider - returns LRC format: `[MM:SS.CC]lyrics`
    Lrclib,
    /// Musixmatch provider - richsync format with word-level timing (JSON)
    MusixmatchRichsync,
    /// Musixmatch provider - subtitle format with line-level timing (JSON)
    MusixmatchSubtitles,
    /// Any dynamically resolved Morphe provider (Apple, LyricsPlus, QQ, KuGou, etc.)
    Custom {
        /// Human-readable provider display name
        name: String,
        /// Whether these lyrics include word-level timing
        word_synced: bool,
    },
}

impl Provider {
    /// Builds a `Provider` badge from a parsed `Lyrics` container.
    pub fn from_lyrics(lyrics: &crate::lyrics::types::Lyrics) -> Self {
        let word_synced = lyrics.has_word_sync();
        if lyrics.provider_name == "LRCLIB" {
            Self::Lrclib
        } else if lyrics.provider_name == "Musixmatch" {
            if word_synced {
                Self::MusixmatchRichsync
            } else {
                Self::MusixmatchSubtitles
            }
        } else {
            Self::Custom {
                name: lyrics.provider_name.clone(),
                word_synced,
            }
        }
    }

    /// Returns true if this provider supplied word-synced timing.
    pub fn is_word_synced(&self) -> bool {
        matches!(
            self,
            Self::MusixmatchRichsync
                | Self::Custom {
                    word_synced: true,
                    ..
                }
        )
    }

    /// Returns the display name of this provider.
    #[cfg(test)]
    pub fn display_name(&self) -> &str {
        match self {
            Self::Lrclib => "LRCLIB",
            Self::MusixmatchRichsync => "Musixmatch (Richsync)",
            Self::MusixmatchSubtitles => "Musixmatch (Subtitles)",
            Self::Custom { name, .. } => name.as_str(),
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
/// - `time_anchor`: Monotonic projection anchor for deterministic position calculation
#[derive(Debug, Clone, PartialEq, Default)]
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

    /// Monotonic projection anchor for unified position tracking
    pub time_anchor: TimeAnchor,
}

impl Update {
    /// Returns the projected playback position at the given monotonic timestamp.
    #[must_use]
    pub fn current_position_at(&self, now: std::time::Instant) -> f64 {
        if self.time_anchor.instant.is_some() {
            self.time_anchor.current_position_at(now)
        } else {
            self.position
        }
    }

    /// Returns the projected playback position at the current instant.
    #[must_use]
    pub fn current_position(&self) -> f64 {
        self.current_position_at(std::time::Instant::now())
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
#[derive(Debug, PartialEq, Default)]
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
        self.timer.set_rate(1.0);
        self.timer.reset(0.0);
        self.position = 0.0;
        self.playing = false;
        self.err = None;
    }

    /// Updates only the metadata fields without affecting position or playback state.
    ///
    /// This is used when metadata changes but the track is conceptually the same
    /// (e.g., after fetching lyrics, we want to update the track info in state
    /// but preserve the current playback position).
    ///
    /// # Behavior
    ///
    /// - Updates title, artist, album, and length
    /// - **Preserves** current position and playback state
    /// - **Preserves** timer state
    /// - Clears any previous error state
    pub fn update_metadata_only(&mut self, meta: &TrackMetadata) {
        self.title.clone_from(&meta.title);
        self.artist.clone_from(&meta.artist);
        self.album.clone_from(&meta.album);
        self.length = meta.length;
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

        // Always ensure non-negative (defensive against timer bugs)
        estimated = estimated.max(0.0);

        // Additionally clamp to track length if known
        if let Some(len) = self.length {
            estimated = estimated.min(len);
        }

        estimated
    }

    /// Checks if the provided metadata represents a different track.
    ///
    /// Compares title, artist, and album to detect track changes.
    #[must_use]
    pub fn has_changed(&self, meta: &TrackMetadata) -> bool {
        self.title != meta.title || self.artist != meta.artist || self.album != meta.album
    }

    /// Sets a new anchor position without changing playback state.
    ///
    /// The position is automatically sanitized (no NaN, no negatives).
    pub fn set_position(&mut self, position: f64) {
        let pos = sanitize_position(position);
        if self.playing {
            self.timer.set_position(pos);
        } else {
            self.timer.reset(pos);
        }
        self.position = pos;
    }

    /// Marks the player as playing and starts the internal timer.
    ///
    /// This is idempotent: calling multiple times has no additional effect.
    pub fn start_playing(&mut self) {
        if !self.playing {
            self.playing = true;
            self.timer.mark_playing();
            tracing::debug!(pos = self.position, "Playback resumed");
        } else if self.timer.time_anchor(true).instant.is_none() {
            self.timer.mark_playing();
            tracing::debug!(pos = self.position, "Playback timer initialized");
        }
    }

    /// Marks the player as paused and stops the internal timer.
    ///
    /// This is idempotent: calling multiple times has no additional effect.
    pub fn pause(&mut self) {
        if self.playing {
            self.position = self.estimate_position();
            self.playing = false;
            self.timer.reset(self.position);
            tracing::debug!(pos = self.position, "Playback paused");
        }
    }

    /// Updates playback rate and anchor position without altering playing/paused state.
    pub fn set_rate(&mut self, position: f64, rate: f64) {
        self.set_position(position);
        self.timer.set_rate(rate);
    }

    /// Returns an immutable [`TimeAnchor`] for deterministic external position projection.
    #[must_use]
    pub fn time_anchor(&self) -> TimeAnchor {
        self.timer.time_anchor(self.playing)
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
#[derive(Debug, Default)]
pub struct LyricState {
    /// Sorted lyrics lines (shared via Arc for cheap cloning)
    pub lines: Arc<Vec<LyricLine>>,

    /// Index of the currently highlighted line (if any)
    pub index: Option<usize>,

    /// O(1) sequential line progress tracker
    pub tracker: crate::lyrics::types::SyncTracker,
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
    #[cfg(test)]
    pub fn get_index(&self, position: f64) -> Option<usize> {
        // Early returns for invalid input
        if self.lines.is_empty() || !position.is_finite() {
            return None;
        }

        crate::lyrics::types::active_line_index(&self.lines, position)
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
        self.tracker.reset();
    }

    /// Sanitizes and sorts a collection of lyric lines.
    ///
    /// Keeps each parent `!is_bg` line grouped with its immediately following `is_bg`
    /// sibling lines so early/leading background vocals cannot detach ahead of their parent.
    fn sanitize_and_sort(lines: Vec<LyricLine>) -> Vec<LyricLine> {
        let sanitized: Vec<LyricLine> = lines.into_iter().filter_map(Self::sanitize_line).collect();
        if sanitized.is_empty() {
            return sanitized;
        }

        // Group each primary line with its immediately following `is_bg` lines.
        let mut clusters: Vec<Vec<LyricLine>> = Vec::with_capacity(sanitized.len());
        for line in sanitized {
            if line.is_bg
                && let Some(last_cluster) = clusters.last_mut()
                && !last_cluster[0].is_bg
            {
                last_cluster.push(line);
            } else {
                clusters.push(vec![line]);
            }
        }

        // Pull parent start time back to any earlier attached background vocal and sort
        // sibling bg lines chronologically while keeping the parent first in each cluster.
        for cluster in &mut clusters {
            if cluster.len() > 1 && !cluster[0].is_bg {
                cluster[1..].sort_by(|a, b| a.time.total_cmp(&b.time));
                let earliest_bg = cluster[1].time;
                if earliest_bg < cluster[0].time {
                    cluster[0].time = earliest_bg;
                }
            }
        }

        clusters.sort_by(|a, b| a[0].time.total_cmp(&b[0].time));

        // Maintain monotonic timestamps across cluster boundaries so binary_search remains valid.
        let mut flat = Vec::new();
        for i in 0..clusters.len() {
            let next_start = clusters.get(i + 1).map(|c| c[0].time);
            let mut cluster = std::mem::take(&mut clusters[i]);
            if let Some(limit) = next_start
                && cluster.len() > 1
            {
                let floor = cluster[0].time;
                for bg in &mut cluster[1..] {
                    if bg.time > limit {
                        bg.time = limit.max(floor);
                    }
                }
            }
            flat.extend(cluster);
        }

        flat
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
#[derive(Debug, Default)]
pub struct StateBundle {
    /// Lyrics state with active line tracking
    pub lyric_state: LyricState,

    /// Player state with position estimation
    pub player_state: PlayerState,

    /// Monotonically increasing version counter
    pub version: u64,

    /// Current lyrics provider (if lyrics are loaded)
    pub provider: Option<Provider>,

    /// Timestamp when lyrics were last loaded (for filtering stale Seeked events)
    pub lyrics_loaded_at: Option<std::time::Instant>,

    /// Version when the last update was sent to UI
    last_sent_version: u64,

    /// Playback state when the last update was sent to UI
    last_sent_playing: bool,
}

impl StateBundle {
    /// Creates a new state bundle with default values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks if the state has changed since the last sent update.
    pub fn should_send_update(&self, force: bool) -> bool {
        if force {
            return true;
        }

        if self.version == self.last_sent_version
            && self.player_state.playing == self.last_sent_playing
        {
            return false;
        }

        // Only send updates when there's something worth showing to the UI
        self.has_lyrics()
            || self.player_state.err.is_some()
            || self.player_state.playing != self.last_sent_playing
    }

    /// Marks the current state as sent to prevent redundant updates.
    pub fn mark_state_sent(&mut self) {
        self.last_sent_version = self.version;
        self.last_sent_playing = self.player_state.playing;
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
        self.lyrics_loaded_at = None;
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
    /// 2. Updates player metadata (preserving position)
    /// 3. Sets error state
    /// 4. Records the provider
    /// 5. Increments version once
    ///
    /// # Position Preservation
    ///
    /// Unlike new track detection, this method **preserves the current position**
    /// because it's called after lyrics are fetched for an already-playing track.
    /// The position should have been set correctly before calling this method.
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
        let has_lyrics = !lines.is_empty();
        self.lyric_state.update_lines(lines);
        self.player_state.update_metadata_only(meta);
        self.player_state.err = err;
        self.provider = provider;

        // Record when lyrics were loaded for filtering stale Seeked events
        self.lyrics_loaded_at = has_lyrics.then(std::time::Instant::now);

        self.increment_version();
    }

    /// Updates active lyric line index based on playback position.
    ///
    /// Increments version and returns `true` if the index changed.
    ///
    /// # Performance
    ///
    /// Uses [`SyncTracker`] for O(1) forward tracking with binary search fallback.
    pub fn update_index(&mut self, position: f64) -> bool {
        let (new_index, changed) = self
            .lyric_state
            .tracker
            .update(&self.lyric_state.lines, position);
        self.lyric_state.index = new_index;

        if changed {
            self.increment_version();
        }

        changed
    }

    /// Maximum allowable discrepancy between estimated playback position and authoritative D-Bus position
    /// before forcing a state synchronization update to observers (in seconds).
    ///
    /// Discrepancies larger than 30ms (such as post-buffering audio alignment or seeks)
    /// produce perceptible desynchronization in richsync word-level karaoke.
    pub const POSITION_DRIFT_THRESHOLD_SECS: f64 = 0.03;

    /// Updates playback state (playing/paused) and position anchor with seek and drift detection.
    ///
    /// Detects position jumps (|observed_pos - estimated_pos| > [`POSITION_DRIFT_THRESHOLD_SECS`]) and increments `version`
    /// so that change detection automatically notifies subscribers of calibration/seek events (even if line index does not change).
    ///
    /// # Returns
    /// Returns `(playing_changed, index_changed, position_jumped)`.
    pub fn update_playback_and_position(
        &mut self,
        playing: bool,
        position: f64,
    ) -> (bool, bool, bool) {
        let prev_playing = self.player_state.playing;
        let prev_position = self.player_state.estimate_position();

        self.player_state.update_playback_dbus(playing, position);

        let current_position = self.player_state.estimate_position();
        let changed_index = self.update_index(current_position);
        let playing_changed = prev_playing != self.player_state.playing;
        let position_jumped =
            (current_position - prev_position).abs() > Self::POSITION_DRIFT_THRESHOLD_SECS;

        if !changed_index && (playing_changed || position_jumped) {
            self.increment_version();
        }

        (playing_changed, changed_index, position_jumped)
    }

    /// Updates playback rate and position, incrementing `version` when anything observable changed.
    pub fn update_rate(&mut self, position: f64, rate: f64) -> (bool, bool) {
        let prev_position = self.player_state.estimate_position();
        let prev_rate = self.player_state.time_anchor().rate;

        self.player_state.set_rate(position, rate);

        let current_position = self.player_state.estimate_position();
        let new_rate = self.player_state.time_anchor().rate;
        let changed_index = self.update_index(current_position);
        let position_jumped =
            (current_position - prev_position).abs() > Self::POSITION_DRIFT_THRESHOLD_SECS;
        let rate_changed = (new_rate - prev_rate).abs() > f64::EPSILON;

        if !changed_index && (position_jumped || rate_changed) {
            self.increment_version();
        }

        (changed_index, position_jumped || rate_changed)
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
        let anchor = self.player_state.time_anchor();
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
            provider: self.provider.clone(),
            time_anchor: anchor,
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
        state.update_lines(vec![LyricLine::new(10.0, "First")]);
        assert_eq!(state.get_index(5.0), None);
    }

    #[test]
    fn test_lyric_index_basic() {
        let mut state = LyricState::default();
        state.update_lines(vec![
            LyricLine::new(10.0, "First"),
            LyricLine::new(20.0, "Second"),
        ]);

        assert_eq!(state.get_index(15.0), Some(0));
        assert_eq!(state.get_index(25.0), Some(1));
    }

    #[test]
    fn test_update_playback_and_position_version_increment() {
        let mut bundle = StateBundle::new();
        bundle.lyric_state.update_lines(vec![
            LyricLine::new(10.0, "Line 1"),
            LyricLine::new(20.0, "Line 2"),
        ]);
        let base_version = bundle.version;

        // Position jump + index change should increment version by exactly 1
        let (playing_changed, index_changed, position_jumped) =
            bundle.update_playback_and_position(true, 15.0);

        assert!(playing_changed);
        assert!(index_changed);
        assert!(position_jumped);
        assert_eq!(bundle.version, base_version + 1);
    }

    #[test]
    fn test_should_send_update_on_playback_change_without_lyrics() {
        let mut bundle = StateBundle::new();
        // Initially no lyrics, not playing, version 0
        bundle.mark_state_sent();

        // Not playing -> playing without lyrics should still trigger update
        bundle.player_state.playing = true;
        assert!(bundle.should_send_update(false));
    }

    #[test]
    fn test_player_state_set_position_paused_resets_instant() {
        let mut player = PlayerState::default();
        player.set_position(10.0);
        // Player is paused by default
        assert!(!player.playing);
        // While paused, estimation remains frozen at 10.0
        assert_eq!(player.estimate_position(), 10.0);
        assert_eq!(player.timer.estimate(true), 10.0);
    }

    #[test]
    fn test_track_transition_resets_playing_and_start_playing_activates_anchor() {
        let mut player = PlayerState::default();
        player.start_playing();
        assert!(player.playing);
        assert!(player.time_anchor().instant.is_some());

        let meta = TrackMetadata {
            title: "New Track".into(),
            artist: "New Artist".into(),
            album: "New Album".into(),
            length: Some(200.0),
            ..Default::default()
        };

        player.update_from_metadata(&meta);
        assert!(!player.playing);
        assert_eq!(player.position, 0.0);
        assert!(player.time_anchor().instant.is_none());

        player.start_playing();
        assert!(player.playing);
        assert!(player.time_anchor().instant.is_some());
    }

    #[test]
    fn test_player_state_pause_freezes_estimated_position() {
        let mut player = PlayerState::default();
        player.set_position(5.0);
        player.start_playing();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let estimated_before = player.estimate_position();
        assert!(estimated_before >= 5.01);

        player.pause();
        assert!(!player.playing);
        assert!(player.time_anchor().instant.is_none());
        assert!((player.position - estimated_before).abs() < 0.005);
        assert_eq!(player.estimate_position(), player.position);
    }

    #[test]
    fn test_sanitize_and_sort_keeps_early_bg_attached_to_parent() {
        let mut lyric_state = LyricState::default();
        let lines = vec![
            LyricLine::new(5.0, "Line 0"),
            LyricLine::new(11.2, "baby"),
            LyricLine::new(10.0, "Oh yeah").with_bg(true),
            LyricLine::new(15.0, "Line 2"),
        ];
        lyric_state.update_lines(lines);
        assert_eq!(lyric_state.lines.len(), 4);
        assert_eq!(lyric_state.lines[1].text, "baby");
        assert!(!lyric_state.lines[1].is_bg);
        assert!((lyric_state.lines[1].time - 10.0).abs() < 1e-6);
        assert_eq!(lyric_state.lines[2].text, "Oh yeah");
        assert!(lyric_state.lines[2].is_bg);
        assert_eq!(lyric_state.get_index(10.5), Some(1));
    }
}
