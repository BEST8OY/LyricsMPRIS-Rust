// Minimal state data structures for lyrics and player

use crate::lyrics::LyricLine;
use crate::mpris::TrackMetadata;
use std::sync::Arc;
use std::time::Instant;

/// Which provider supplied the current lyrics.
#[derive(Debug, Clone, PartialEq)]
pub enum Provider {
    Lrclib,
    MusixmatchRichsync,
    MusixmatchSubtitles,
    // Db removed when local DB support was disabled
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Update {
    pub lines: Arc<Vec<LyricLine>>,
    pub index: usize,
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
    pub position: f64,
    pub err: Option<String>,
    pub last_position: f64,
    pub last_update: Option<Instant>,
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
        self.last_position = 0.0;
        self.last_update = Some(Instant::now());
    }
    pub fn update_playback_dbus(&mut self, playing: bool, position: f64) {
        self.playing = playing;
        self.last_position = position;
        self.last_update = Some(Instant::now());
        self.position = position;
    }
    pub fn estimate_position(&self) -> f64 {
        if self.playing
            && let Some(instant) = self.last_update
        {
            let elapsed = instant.elapsed().as_secs_f64();
            return self.last_position + elapsed;
        }
        self.last_position
    }
    pub fn has_changed(&self, meta: &TrackMetadata) -> bool {
        self.title != meta.title || self.artist != meta.artist || self.album != meta.album
    }
    pub fn reset_position_cache(&mut self, position: f64) {
        self.last_position = position;
        self.last_update = Some(Instant::now());
        self.position = position;
    }
}

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
        match self
            .lines
            .binary_search_by(|line| match line.time.partial_cmp(&position) {
                Some(ord) => ord,
                _ => std::cmp::Ordering::Less,
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

pub struct StateBundle {
    pub lyric_state: LyricState,
    pub player_state: PlayerState,
    pub version: u64,
    /// Provider that supplied current lyrics
    pub provider: Option<Provider>,
}

impl StateBundle {
    pub fn new() -> Self {
        Self {
            lyric_state: LyricState::default(),
            player_state: PlayerState::default(),
            version: 0,
            provider: None,
        }
    }
    pub fn clear_lyrics(&mut self) {
        self.lyric_state.update_lines(Vec::new());
        self.lyric_state.index = 0;
        self.version += 1;
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
        self.version += 1;
        // Store provider so callers can know which provider supplied the current lyrics
        self.provider = provider;
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
