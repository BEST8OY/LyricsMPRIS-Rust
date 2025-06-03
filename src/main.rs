mod lyrics;
mod mpris;
mod pool;
mod ui;
mod lyricsdb;

use clap::Parser;
use std::time::Duration;
use std::error::Error;
use std::sync::{Arc, Mutex};

/// Application configuration from CLI
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Config {
    /// Pipe current lyric line to stdout (default is modern UI)
    #[arg(long)]
    pipe: bool,
    /// Lyric poll interval in milliseconds
    #[arg(long, default_value_t = 1000)]
    poll: u64,
    /// Path to local lyrics database (optional)
    #[arg(long)]
    database: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cfg = Config::parse();
    let poll_interval = Duration::from_millis(cfg.poll);
    let db_path = cfg.database.clone();

    // Load database if path provided
    let db = if let Some(ref path) = db_path {
        match lyricsdb::LyricsDB::load(path) {
            Ok(db) => Some(Arc::new(Mutex::new(db))),
            Err(e) => {
                eprintln!("Failed to load database: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Always start the UI, even if no song is playing yet
    // Try to get current metadata/position, but ignore errors and let UI handle waiting
    let meta = mpris::get_metadata().await.unwrap_or_default();
    let pos = mpris::get_position().await.unwrap_or(0.0);

    let result = if cfg.pipe {
        ui::display_lyrics_pipe(meta, pos, poll_interval, db.clone(), db_path.clone()).await
    } else {
        ui::display_lyrics_modern(meta, pos, poll_interval, db.clone(), db_path.clone()).await
    };

    // Print error if any, for better diagnostics
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        return Err(e);
    }
    Ok(())
}
