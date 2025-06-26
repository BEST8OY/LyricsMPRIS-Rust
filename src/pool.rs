// pool.rs: Central event loop for polling and event-based updates

use crate::lyrics::LyricLine;
use crate::lyricsdb::LyricsDB;
use crate::mpris::{TrackMetadata};
use tokio::sync::{mpsc, Mutex};
use std::sync::Arc;
use tokio::time::Duration;

/// Represents a UI update for lyrics and player state.
#[derive(Debug, Clone, Default)]
pub struct Update {
    pub lines: Vec<LyricLine>,
    pub index: usize,
    pub err: Option<String>,
    pub unsynced: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct PlayerState {
    title: String,
    artist: String,
    album: String,
    playing: bool,
    position: f64,
    err: Option<String>,
}

impl PlayerState {
    fn update_from_metadata(&mut self, meta: &TrackMetadata) {
        self.title = meta.title.clone();
        self.artist = meta.artist.clone();
        self.album = meta.album.clone();
        self.position = 0.0;
        self.err = None;
    }
    fn update_playback(&mut self, playing: bool, position: f64) {
        self.playing = playing;
        self.position = position;
    }
    fn has_changed(&self, meta: &TrackMetadata) -> bool {
        self.title != meta.title || self.artist != meta.artist || self.album != meta.album
    }
}

#[derive(Debug, Clone, Default)]
struct LyricState {
    lines: Vec<LyricLine>,
    index: usize,
}

impl LyricState {
    /// Returns the index of the lyric line for the given playback position.
    fn get_index(&self, position: f64) -> usize {
        if self.lines.len() <= 1 {
            return 0;
        }
        // Use binary search for efficiency
        match self.lines.binary_search_by(|line| line.time.partial_cmp(&position).unwrap_or(std::cmp::Ordering::Less)) {
            Ok(idx) => idx,
            Err(0) => 0,
            Err(idx) => idx - 1,
        }
    }
    fn update_lines(&mut self, lines: Vec<LyricLine>) {
        self.index = 0;
        self.lines = lines;
    }
    fn update_index(&mut self, new_index: usize) -> bool {
        if new_index != self.index {
            self.index = new_index;
            true
        } else {
            false
        }
    }
}

// Combined helpers and inlined small functions
struct StateBundle<'a> {
    lyric_state: &'a mut LyricState,
    player_state: &'a mut PlayerState,
    last_unsynced: &'a mut Option<String>,
}

async fn set_lyric_state_and_update(
    bundle: &mut StateBundle<'_>,
    lines: Vec<LyricLine>,
    unsynced: Option<String>,
    meta: &TrackMetadata,
    err: Option<String>,
    update_tx: &mpsc::Sender<Update>,
) {
    bundle.lyric_state.update_lines(lines);
    *bundle.last_unsynced = unsynced;
    bundle.player_state.err = err;
    bundle.player_state.update_from_metadata(meta);
    let _ = update_tx.send(Update {
        lines: bundle.lyric_state.lines.clone(),
        index: bundle.lyric_state.index,
        err: bundle.player_state.err.clone(),
        unsynced: bundle.last_unsynced.clone(),
    }).await;
}

async fn try_load_from_db_and_update(
    meta: &TrackMetadata,
    bundle: &mut StateBundle<'_>,
    db: &Arc<Mutex<LyricsDB>>,
    update_tx: &mpsc::Sender<Update>,
) -> bool {
    let guard = db.lock().await;
    if let Some(synced) = guard.get(&meta.artist, &meta.title) {
        set_lyric_state_and_update(
            bundle,
            crate::lyrics::parse_synced_lyrics(&synced),
            None,
            meta,
            None,
            update_tx,
        ).await;
        true
    } else {
        set_lyric_state_and_update(bundle, Vec::new(), None, meta, None, update_tx).await;
        false
    }
}

async fn try_fetch_from_api_and_save_and_update(
    meta: &TrackMetadata,
    bundle: &mut StateBundle<'_>,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    debug_log: bool,
    update_tx: &mpsc::Sender<Update>,
) {
    match crate::lyrics::fetch_lyrics_from_lrclib(&meta.artist, &meta.title).await {
        Ok((_plain, synced)) if !synced.is_empty() => {
            set_lyric_state_and_update(
                bundle,
                crate::lyrics::parse_synced_lyrics(&synced),
                None,
                meta,
                None,
                update_tx,
            ).await;
            if let (Some(db), Some(path)) = (db, db_path) {
                let mut guard = db.lock().await;
                guard.insert(&meta.artist, &meta.title, &synced);
                let _ = guard.save(path);
            }
        }
        Ok((plain, _)) => {
            set_lyric_state_and_update(
                bundle,
                Vec::new(),
                if plain.is_empty() { None } else { Some(plain) },
                meta,
                None,
                update_tx,
            ).await;
        }
        Err(e) => {
            if debug_log {
                eprintln!("[LyricsMPRIS] API error: {}", e);
            }
            set_lyric_state_and_update(bundle, Vec::new(), None, meta, Some(e.to_string()), update_tx).await;
        }
    }
}

async fn fetch_and_update_lyrics(
    meta: &TrackMetadata,
    bundle: &mut StateBundle<'_>,
    db: Option<&Arc<Mutex<LyricsDB>>>,
    db_path: Option<&str>,
    position: f64,
    debug_log: bool,
    update_tx: &mpsc::Sender<Update>,
) {
    if let Some(db) = db {
        if try_load_from_db_and_update(meta, bundle, db, update_tx).await {
            bundle.lyric_state.index = bundle.lyric_state.get_index(position);
            return;
        }
    }
    try_fetch_from_api_and_save_and_update(meta, bundle, db, db_path, debug_log, update_tx).await;
    bundle.lyric_state.index = bundle.lyric_state.get_index(position);
}

/// Listens for player and lyric updates, sending them to the update channel.
pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    poll_interval: Duration,
    db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
    mut shutdown_rx: mpsc::Receiver<()>,
    mpris_config: crate::Config,
) {
    let mut player_state = PlayerState::default();
    let mut lyric_state = LyricState::default();
    let mut last_unsynced: Option<String> = None;
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let mut latest_meta: Option<(TrackMetadata, f64)> = None;
    let event_tx_clone = event_tx.clone();
    let mpris_config_clone = mpris_config.clone();
    tokio::spawn(async move {
        let _ = crate::mpris::watch_and_handle_events(
            move |meta, pos| {
                let _ = event_tx_clone.try_send((meta, pos, true));
            },
            move |meta, pos| {
                let _ = event_tx.try_send((meta, pos, false));
            },
            Some(&mpris_config_clone),
        ).await;
    });
    let mut bundle = StateBundle {
        lyric_state: &mut lyric_state,
        player_state: &mut player_state,
        last_unsynced: &mut last_unsynced,
    };
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            maybe_event = event_rx.recv() => {
                if let Some((meta, position, is_track_change)) = maybe_event {
                    let changed = bundle.player_state.has_changed(&meta);
                    if is_track_change && changed {
                        latest_meta = Some((meta.clone(), position));
                    }
                    bundle.player_state.update_playback(true, position);
                    let new_index = bundle.lyric_state.get_index(bundle.player_state.position);
                    if changed {
                        bundle.lyric_state.index = new_index;
                        let _ = update_tx.send(Update {
                            lines: bundle.lyric_state.lines.clone(),
                            index: bundle.lyric_state.index,
                            err: bundle.player_state.err.clone(),
                            unsynced: bundle.last_unsynced.clone(),
                        }).await;
                    } else if bundle.lyric_state.update_index(new_index) {
                        let _ = update_tx.send(Update {
                            lines: bundle.lyric_state.lines.clone(),
                            index: bundle.lyric_state.index,
                            err: bundle.player_state.err.clone(),
                            unsynced: bundle.last_unsynced.clone(),
                        }).await;
                    }
                }
            }
            _ = tokio::time::sleep(poll_interval) => {
                if let Some((meta, position)) = latest_meta.take() {
                    fetch_and_update_lyrics(&meta, &mut bundle, db.as_ref(), db_path.as_deref(), position, mpris_config.debug_log, &update_tx).await;
                }
                let meta = crate::mpris::get_metadata(Some(&mpris_config)).await.unwrap_or_default();
                let playing = matches!(crate::mpris::get_playback_status(Some(&mpris_config)).await.unwrap_or_default().as_str(), "Playing");
                let position = crate::mpris::get_position(Some(&mpris_config)).await.unwrap_or(0.0);
                let changed = bundle.player_state.has_changed(&meta);
                bundle.player_state.update_playback(playing, position);
                let new_index = bundle.lyric_state.get_index(bundle.player_state.position);
                if changed {
                    bundle.lyric_state.index = new_index;
                    let _ = update_tx.send(Update {
                        lines: bundle.lyric_state.lines.clone(),
                        index: bundle.lyric_state.index,
                        err: bundle.player_state.err.clone(),
                        unsynced: bundle.last_unsynced.clone(),
                    }).await;
                } else if bundle.lyric_state.update_index(new_index) {
                    let _ = update_tx.send(Update {
                        lines: bundle.lyric_state.lines.clone(),
                        index: bundle.lyric_state.index,
                        err: bundle.player_state.err.clone(),
                        unsynced: bundle.last_unsynced.clone(),
                    }).await;
                }
                if bundle.player_state.err.is_some() || bundle.last_unsynced.is_some() {
                    let _ = update_tx.send(Update {
                        lines: bundle.lyric_state.lines.clone(),
                        index: bundle.lyric_state.index,
                        err: bundle.player_state.err.clone(),
                        unsynced: bundle.last_unsynced.clone(),
                    }).await;
                }
            }
        }
    }
}