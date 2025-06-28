// state.rs: State data structures for lyrics and player

use crate::lyrics::LyricLine;
use crate::mpris::TrackMetadata;
use std::sync::Arc;

/// Represents a UI update for lyrics and player state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Update {
    pub lines: Arc<Vec<LyricLine>>,
    pub index: usize,
    pub err: Option<String>,
    pub unsynced: Option<String>,
    pub version: u64, // Incremented on any state change
}

/// Holds the current state of the player (track info, playback, errors).
#[derive(Debug, Default, PartialEq)]
pub struct PlayerState {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub position: f64,
    pub err: Option<String>,
}

impl PlayerState {
    pub fn update_from_metadata(&mut self, meta: &TrackMetadata) {
        self.title = meta.title.clone();
        self.artist = meta.artist.clone();
        self.album = meta.album.clone();
        self.position = 0.0;
        self.err = None;
    }
    pub fn update_playback(&mut self, playing: bool, position: f64) {
        self.playing = playing;
        self.position = position;
    }
    pub fn has_changed(&self, meta: &TrackMetadata) -> bool {
        self.title != meta.title || self.artist != meta.artist || self.album != meta.album
    }
}

/// Holds the current state of the lyrics (lines and current index).
#[derive(Debug, Default)]
pub struct LyricState {
    pub lines: Arc<Vec<LyricLine>>,
    pub index: usize,
}

impl LyricState {
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
    pub fn update_lines(&mut self, lines: Vec<LyricLine>) {
        self.index = 0;
        self.lines = Arc::new(lines);
    }
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
    pub last_unsynced: Option<String>,
    pub version: u64, // Incremented on any state change
}

impl StateBundle {
    pub fn new() -> Self {
        Self {
            lyric_state: LyricState::default(),
            player_state: PlayerState::default(),
            last_unsynced: None,
            version: 0,
        }
    }
    pub fn update_playback(&mut self, playing: bool, position: f64) {
        if self.player_state.playing != playing || (self.player_state.position - position).abs() > f64::EPSILON {
            self.version += 1;
        }
        self.player_state.update_playback(playing, position);
    }
    pub fn has_player_changed(&self, meta: &TrackMetadata) -> bool {
        self.player_state.has_changed(meta)
    }
    pub fn clear_lyrics(&mut self) {
        self.lyric_state.update_lines(Vec::new());
        self.lyric_state.index = 0;
        self.version += 1;
    }
    pub fn update_lyrics(&mut self, lines: Vec<LyricLine>, unsynced: Option<String>, meta: &TrackMetadata, err: Option<String>) {
        self.lyric_state.update_lines(lines);
        self.last_unsynced = unsynced;
        self.player_state.err = err;
        self.player_state.update_from_metadata(meta);
        self.version += 1;
    }
    pub fn update_index(&mut self, position: f64) -> bool {
        let new_index = self.lyric_state.get_index(position);
        let changed = self.lyric_state.update_index(new_index);
        if changed {
            self.version += 1;
        }
        changed
    }
}
