use crate::pool;
// polling removed; no Duration needed here
use tokio::sync::mpsc;
use std::pin::Pin;
use tokio::time::Sleep;
use std::time::Instant;
use crate::ui::estimate_update_and_next_sleep;

/// Display lyrics in pipe mode (stdout only, for scripting)
pub async fn display_lyrics_pipe(
    _meta: crate::mpris::TrackMetadata,
    _pos: f64,
    mpris_config: crate::Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::channel(32);
    let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
    tokio::spawn(pool::listen(tx, shutdown_rx, mpris_config.clone()));

    // State for track transitions and lyric printing
    let mut last_track_id: Option<(String, String, String)> = None;
    let mut last_track_had_lyric = false;
    let mut last_line_idx: Option<usize> = None;

    // Optional per-word/line sleep for progressive printing
    let mut next_sleep: Option<Pin<Box<Sleep>>> = None;
    let mut last_update: Option<crate::state::Update> = None;
    let mut last_update_instant: Option<Instant> = None;

    loop {
        tokio::select! {
            maybe_upd = rx.recv() => {
                match maybe_upd {
                    Some(upd) => {
                        // store the last_update for local estimation
                        last_update = Some(upd.clone());

                        let track_id = crate::ui::track_id(&upd);
                        let has_lyrics = !upd.lines.is_empty();
                        let track_changed = last_track_id.as_ref() != Some(&track_id);

                        if track_changed {
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
                        } else {
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

                        // record when we received this update so local estimates can advance
                        last_update_instant = Some(Instant::now());
                        // (Re)compute next sleep using helper
                        let (_maybe_tmp, next) = estimate_update_and_next_sleep(&last_update, last_update_instant, true);
                        next_sleep = next;
                    }
                    None => break, // channel closed
                }
            }

            // wake on scheduled per-word/line timing
            _ = async {
                if let Some(s) = &mut next_sleep {
                    s.as_mut().await;
                } else {
                    futures_util::future::pending::<()>().await;
                }
            } => {
                // on wake, estimate update and print progressed lines if changed
                let (maybe_tmp, next) = estimate_update_and_next_sleep(&last_update, last_update_instant, true);
                if let Some(tmp) = maybe_tmp {
                    // If index advanced compared to last_line_idx, print the new line(s)
                    if tmp.index != last_line_idx {
                        if let Some(idx) = tmp.index {
                            if let Some(line) = tmp.lines.get(idx) {
                                println!("{}", line.text);
                                last_track_had_lyric = true;
                            }
                        }
                        last_line_idx = tmp.index;
                        // update stored last_update so future estimates are based on last wake time
                        last_update = Some(tmp);
                        // reset the instant to now so next estimate advances from here
                        last_update_instant = Some(Instant::now());
                    }
                }
                next_sleep = next;
            }
        }
    }
    Ok(())
}
