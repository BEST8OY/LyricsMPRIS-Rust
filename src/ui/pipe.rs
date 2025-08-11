use crate::pool;
use crate::lyricsdb::LyricsDB;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use std::sync::Arc;

/// Display lyrics in pipe mode (stdout only, for scripting)
pub async fn display_lyrics_pipe(
    _meta: crate::mpris::TrackMetadata,
    _pos: f64,
    poll_interval: Duration,
    db: Option<Arc<Mutex<LyricsDB>>>,
    db_path: Option<String>,
    mpris_config: crate::Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::channel(32);
    let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
    tokio::spawn(pool::listen(tx, poll_interval, db.clone(), db_path.clone(), shutdown_rx, mpris_config.clone()));

    // State for track transitions and lyric printing
    let mut last_track_id: Option<(String, String, String)> = None;
    let mut last_track_had_lyric = false;
    let mut last_line_idx = None;

    while let Some(upd) = rx.recv().await {
        let track_id = (upd.artist.clone(), upd.title.clone(), upd.album.clone());
        let has_lyrics = !upd.lines.is_empty();
        let track_changed = last_track_id.as_ref() != Some(&track_id);

        if track_changed {
            // Only print a newline if previous track had lyrics and new track has no lyrics
            if last_track_id.is_some() && last_track_had_lyric && !has_lyrics {
                println!("");
            }
            last_track_id = Some(track_id);
            last_line_idx = None;
            last_track_had_lyric = false;
            if has_lyrics {
                if let Some(line) = upd.lines.get(upd.index) {
                    println!("{}", line.text);
                    last_track_had_lyric = true;
                }
                last_line_idx = Some(upd.index);
            }
            continue;
        }

        if has_lyrics {
            if Some(upd.index) != last_line_idx {
                if let Some(line) = upd.lines.get(upd.index) {
                    println!("{}", line.text);
                    last_track_had_lyric = true;
                }
                last_line_idx = Some(upd.index);
            }
        }
    }
    Ok(())
}