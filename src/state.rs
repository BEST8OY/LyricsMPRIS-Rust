// state.rs: State data structures for lyrics and player

use crate::lyrics::LyricLine;
use crate::mpris::TrackMetadata;
use std::sync::Arc;
use std::time::Instant;

/// Represents a UI update for lyrics and player state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Update {
    pub lines: Arc<Vec<LyricLine>>,
    pub index: usize,
    pub err: Option<String>,
    pub version: u64, // Incremented on any state change
    pub playing: bool, // Whether playback is active
    pub artist: String,
    pub title: String,
    pub album: String,
}

/// Holds the current state of the player (track info, playback, errors).
#[derive(Debug, PartialEq)]
pub struct PlayerState {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub position: f64, // last known position (seconds)
    pub err: Option<String>,
    pub player_service: Option<String>, // Cached D-Bus service name
    pub last_position: f64, // last position from DBus (seconds)
    pub last_update: Option<Instant>, // when last_position was updated
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
            player_service: None,
            last_position: 0.0,
            last_update: None,
        }
    }
}

impl PlayerState {
    /// Update player state from new metadata.
    pub fn update_from_metadata(&mut self, meta: &TrackMetadata) {
        self.title = meta.title.clone();
        self.artist = meta.artist.clone();
        self.album = meta.album.clone();
        self.position = 0.0;
        self.err = None;
        self.last_position = 0.0;
        self.last_update = Some(Instant::now());
    }

    /// Update playback status and position from DBus (reset cache).
    pub fn update_playback_dbus(&mut self, playing: bool, position: f64) {
        self.playing = playing;
        self.last_position = position;
        self.last_update = Some(Instant::now());
        self.position = position;
    }

    /// Estimate current position using local timer if playing.
    pub fn estimate_position(&self) -> f64 {
        if self.playing {
            if let Some(instant) = self.last_update {
                let elapsed = instant.elapsed().as_secs_f64();
                return self.last_position + elapsed;
            }
        }
        self.last_position
    }

    /// Returns true if the track metadata has changed.
    pub fn has_changed(&self, meta: &TrackMetadata) -> bool {
        self.title != meta.title || self.artist != meta.artist || self.album != meta.album
    }

    /// Reset position cache (e.g., on track change).
    pub fn reset_position_cache(&mut self, position: f64) {
        self.last_position = position;
        self.last_update = Some(Instant::now());
        self.position = position;
    }
}

/// Holds the current state of the lyrics (lines and current index).
#[derive(Debug, Default)]
pub struct LyricState {
    pub lines: Arc<Vec<LyricLine>>,
    pub index: usize,
}

impl LyricState {
    /// Get the index of the lyric line for the given playback position.
    pub fn get_index(&self, position: f64) -> usize {
        if self.lines.len() <= 1 {
            return 0;
        }
        if position.is_nan() || self.lines.iter().any(|line| line.time.is_nan()) {
            return 0;
        }
        match self.lines.binary_search_by(|line| {
            match line.time.partial_cmp(&position) {
                Some(ord) => ord,
                None => std::cmp::Ordering::Less,
            }
        }) {
            Ok(idx) => idx,
            Err(0) => 0,
            Err(idx) => idx - 1,
        }
    }

    /// Replace all lyric lines and reset index.
    pub fn update_lines(&mut self, lines: Vec<LyricLine>) {
        self.index = 0;
        self.lines = Arc::new(lines);
    }

    /// Update the current lyric index. Returns true if changed.
    pub fn update_index(&mut self, new_index: usize) -> bool {
        if new_index != self.index {
            self.index = new_index;
            true
        } else {
            false
        }
    }
}

/// Bundles all state for the player and lyrics, plus versioning.
pub struct StateBundle {
    pub lyric_state: LyricState,
    pub player_state: PlayerState,
    pub version: u64, // Incremented on any state change
}

impl StateBundle {
    /// Create a new, empty state bundle.
    pub fn new() -> Self {
        Self {
            lyric_state: LyricState::default(),
            player_state: PlayerState::default(),
            version: 0,
        }
    }

    /// Clear all lyrics and increment version.
    pub fn clear_lyrics(&mut self) {
        self.lyric_state.update_lines(Vec::new());
        self.lyric_state.index = 0;
        self.version += 1;
    }

    /// Update lyrics, player metadata, and error, incrementing version.
    pub fn update_lyrics(&mut self, lines: Vec<LyricLine>, meta: &TrackMetadata, err: Option<String>) {
        self.lyric_state.update_lines(lines);
        self.player_state.err = err;
        self.player_state.update_from_metadata(meta);
        self.version += 1;
    }

    /// Update lyric index for the given position. Returns true if changed and increments version.
    pub fn update_index(&mut self, position: f64) -> bool {
        let new_index = self.lyric_state.get_index(position);
        let changed = self.lyric_state.update_index(new_index);
        if changed {
            self.version += 1;
        }
        changed
    }
}
