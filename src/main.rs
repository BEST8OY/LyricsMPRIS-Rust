mod lyrics;
mod mpris;
mod pool;
mod ui;
mod lyricsdb;
mod text_utils;

use clap::Parser;
use std::time::Duration;
use std::error::Error;
use tokio::sync::Mutex;
use std::sync::Arc;
use crate::mpris::{get_metadata, get_position};


/// Application configuration from CLI
#[derive(Parser, Debug, Clone, Default)]
#[command(author, version, about)]
pub struct Config {
    /// Pipe current lyric line to stdout (default is modern UI)
    #[arg(long)]
    pipe: bool,
    /// Lyric poll interval in milliseconds
    #[arg(long, default_value_t = 1000)]
    poll: u64,
    /// Path to local lyrics database (optional)
    #[arg(long)]
    database: Option<String>,
    /// Blocklist for MPRIS player service names (comma-separated, case-insensitive)
    #[arg(long = "block", value_name = "SERVICE1,SERVICE2", value_delimiter = ',')]
    block: Vec<String>,
    /// Enable backend error logging to stderr
    #[arg(long)]
    pub debug_log: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
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
    let meta = get_metadata(Some(&cfg)).await.unwrap_or_default();
    let pos = get_position(Some(&cfg)).await.unwrap_or(0.0);

    let result = if cfg.pipe {
        crate::ui::display_lyrics_pipe(meta, pos, poll_interval, db.clone(), db_path.clone(), cfg.clone()).await
    } else {
        crate::ui::display_lyrics_modern(meta, pos, poll_interval, db.clone(), db_path.clone(), cfg.clone()).await
    };

    // Print error if any, for better diagnostics
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        return Err(e);
    }
    Ok(())
}
