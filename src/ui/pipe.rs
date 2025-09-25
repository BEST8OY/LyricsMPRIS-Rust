use crate::pool;
use std::time::Duration;
use tokio::sync::mpsc;

/// Display lyrics in pipe mode (stdout only, for scripting)
pub async fn display_lyrics_pipe(
    _meta: crate::mpris::TrackMetadata,
    _pos: f64,
    poll_interval: Duration,
    mpris_config: crate::Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::channel(32);
    let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
    tokio::spawn(pool::listen(tx, poll_interval, shutdown_rx, mpris_config.clone()));

    // State for track transitions and lyric printing
    let mut last_track_id: Option<(String, String, String)> = None;
    let mut last_track_had_lyric = false;
    let mut last_line_idx: Option<usize> = None;

    while let Some(upd) = rx.recv().await {
        let track_id = crate::ui::track_id(&upd);
        let has_lyrics = !upd.lines.is_empty();
        let track_changed = last_track_id.as_ref() != Some(&track_id);

        if track_changed {
            // Only print a newline if previous track had lyrics and new track has no lyrics
            if last_track_id.is_some() && last_track_had_lyric && !has_lyrics {
                println!();
            }
            last_track_id = Some(track_id);
            last_line_idx = None;
            last_track_had_lyric = false;
            if has_lyrics {
                if let Some(idx) = upd.index {
                    if let Some(line) = upd.lines.get(idx) {
                        println!("{}", line.text);
                        last_track_had_lyric = true;
                    }
                    last_line_idx = Some(idx);
                }
            }
            continue;
        }

        if has_lyrics && upd.index != last_line_idx {
            if let Some(idx) = upd.index {
                if let Some(line) = upd.lines.get(idx) {
                    println!("{}", line.text);
                    last_track_had_lyric = true;
                }
            }
            last_line_idx = upd.index;
        }
    }
    Ok(())
}
