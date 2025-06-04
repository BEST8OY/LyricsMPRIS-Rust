// pool.rs: Central event loop for polling and event-based updates

use crate::lyrics::LyricLine;
use crate::mpris;
use crate::lyricsdb::LyricsDB;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration, Sleep};
use std::pin::Pin;

#[derive(Debug, Clone, Default)]
pub struct Update {
    #[allow(dead_code)]
    pub lines: Vec<LyricLine>,
    #[allow(dead_code)]
    pub index: usize,
    #[allow(dead_code)]
    pub playing: bool,
    #[allow(dead_code)]
    pub err: Option<String>,
    #[allow(dead_code)]
    pub unsynced: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct PlayerState {
    title: String,
    artist: String,
    album: String,
    playing: bool,
    position: f64,
    err: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct LyricState {
    lines: Vec<LyricLine>,
    index: usize,
}

impl LyricState {
    fn get_index(&self, position: f64) -> usize {
        if self.lines.len() <= 1 {
            return 0;
        }
        if position >= self.lines[self.index].time {
            for i in self.index + 1..self.lines.len() {
                if position < self.lines[i].time {
                    return i - 1;
                }
            }
            return self.lines.len() - 1;
        }
        for i in (0..=self.index).rev() {
            if position > self.lines[i].time {
                return i;
            }
        }
        0
    }
}

async fn update_lyric_state(meta: &crate::mpris::TrackMetadata, lyric_state: &mut LyricState, last_unsynced: &mut Option<String>, state: &mut PlayerState, db: Option<&Arc<Mutex<LyricsDB>>>, db_path: Option<&String>) {
    // Try local DB first
    if let Some(db) = db {
        if let Some(synced) = db.lock().unwrap().get(&meta.artist, &meta.title) {
            lyric_state.lines = crate::lyrics::parse_synced_lyrics(&synced);
            *last_unsynced = None;
            state.err = None;
            return;
        } else {
            // Not found in DB: clear lyric state immediately
            lyric_state.lines.clear();
            lyric_state.index = 0;
            *last_unsynced = None;
            state.err = None;
        }
    }
    // Fallback to API
    match crate::lyrics::fetch_lyrics_from_lrclib(&meta.artist, &meta.title).await {
        Ok((_, synced)) if !synced.is_empty() => {
            lyric_state.lines = crate::lyrics::parse_synced_lyrics(&synced);
            *last_unsynced = None;
            // Save to DB if available
            if let (Some(db), Some(path)) = (db, db_path) {
                db.lock().unwrap().insert(&meta.artist, &meta.title, &synced);
                let _ = db.lock().unwrap().save(path);
            }
            state.err = None;
        }
        Ok((plain, _)) => {
            lyric_state.lines.clear();
            *last_unsynced = Some(plain);
            state.err = None;
        }
        Err(e) => {
            lyric_state.lines.clear();
            *last_unsynced = None;
            state.err = Some(e.to_string());
        }
    }
    lyric_state.index = 0;
    state.position = 0.0;
    state.title = meta.title.clone();
    state.artist = meta.artist.clone();
    state.album = meta.album.clone();
}

/// Listens for player and lyric updates, sending them to the update channel.
pub async fn listen(
    update_tx: mpsc::Sender<Update>,
    poll_interval: Duration,
    db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
) {
    let mut state = PlayerState::default();
    let mut lyric_state = LyricState::default();
    let mut last_unsynced: Option<String> = None;
    let (event_tx, mut event_rx) = mpsc::channel(8);

    // Debounce state for track changes
    let mut pending_meta: Option<crate::mpris::TrackMetadata> = None;
    let mut debounce_sleep: Option<Pin<Box<Sleep>>> = None;
    let debounce_duration = Duration::from_millis(500); // adjust as needed

    // Spawn event-based listener for MPRIS property changes
    let event_tx_clone = event_tx.clone();
    tokio::spawn(async move {
        let _ = crate::mpris::watch_and_handle_events(
            move |meta, pos| {
                let _ = event_tx_clone.try_send((meta, pos, true));
            },
            move |meta, pos| {
                let _ = event_tx.try_send((meta, pos, false));
            },
        ).await;
    });

    loop {
        tokio::select! {
            Some((meta, position, is_track_change)) = event_rx.recv() => {
                let changed = meta.title != state.title || meta.artist != state.artist || meta.album != state.album;
                if is_track_change && changed {
                    // Debounce: store meta and reset timer
                    pending_meta = Some(meta.clone());
                    debounce_sleep = Some(Box::pin(sleep(debounce_duration)));
                }
                state.playing = true;
                state.position = position;
                let new_index = lyric_state.get_index(state.position);
                let index_changed = new_index != lyric_state.index;
                if changed {
                    lyric_state.index = new_index;
                    let _ = update_tx.send(Update {
                        lines: lyric_state.lines.clone(),
                        index: lyric_state.index,
                        playing: state.playing,
                        err: state.err.clone(),
                        unsynced: last_unsynced.clone(),
                    }).await;
                } else if index_changed {
                    lyric_state.index = new_index;
                    let _ = update_tx.send(Update {
                        lines: Vec::new(),
                        index: lyric_state.index,
                        playing: state.playing,
                        err: None,
                        unsynced: None,
                    }).await;
                }
            }
            // Debounce timer fires: fetch lyrics for last pending meta
            _ = async {
                if let Some(ref mut sleep_fut) = debounce_sleep {
                    sleep_fut.as_mut().await;
                    true
                } else {
                    false
                }
            }, if debounce_sleep.is_some() => {
                if let Some(meta) = pending_meta.take() {
                    update_lyric_state(&meta, &mut lyric_state, &mut last_unsynced, &mut state, db.as_ref(), db_path.as_ref()).await;
                    lyric_state.index = lyric_state.get_index(state.position);
                    // Always send update after track change, even if no lyrics
                    let _ = update_tx.send(Update {
                        lines: lyric_state.lines.clone(),
                        index: lyric_state.index,
                        playing: state.playing,
                        err: state.err.clone(),
                        unsynced: last_unsynced.clone(),
                    }).await;
                }
                debounce_sleep = None;
            }
            _ = tokio::time::sleep(poll_interval) => {
                // Fallback polling (for robustness)
                let meta = mpris::get_metadata().await.unwrap_or_default();
                let playing = matches!(mpris::get_playback_status().await.as_deref(), Ok("Playing"));
                let position = mpris::get_position().await.unwrap_or(0.0);
                let changed = meta.title != state.title || meta.artist != state.artist || meta.album != state.album;
                if changed && playing {
                    // Debounce polling as well
                    pending_meta = Some(meta.clone());
                    debounce_sleep = Some(Box::pin(sleep(debounce_duration)));
                }
                state.playing = playing;
                state.position = position;
                let new_index = lyric_state.get_index(state.position);
                let index_changed = new_index != lyric_state.index;
                if changed {
                    lyric_state.index = new_index;
                    let _ = update_tx.send(Update {
                        lines: lyric_state.lines.clone(),
                        index: lyric_state.index,
                        playing: state.playing,
                        err: state.err.clone(),
                        unsynced: last_unsynced.clone(),
                    }).await;
                } else if index_changed {
                    lyric_state.index = new_index;
                    let _ = update_tx.send(Update {
                        lines: Vec::new(),
                        index: lyric_state.index,
                        playing: state.playing,
                        err: None,
                        unsynced: None,
                    }).await;
                } else if state.err.is_some() || last_unsynced.is_some() {
                    let _ = update_tx.send(Update {
                        lines: lyric_state.lines.clone(),
                        index: lyric_state.index,
                        playing: state.playing,
                        err: state.err.clone(),
                        unsynced: last_unsynced.clone(),
                    }).await;
                }
            }
        }
    }
}
