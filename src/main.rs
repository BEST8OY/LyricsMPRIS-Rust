mod lyrics;
mod mpris;
mod pool;
mod ui;
mod lyricsdb;
mod text_utils;
mod state;
mod event;

use clap::Parser;
use std::time::Duration;
use std::error::Error;
use tokio::sync::Mutex;
use std::sync::Arc;
use crate::mpris::metadata::get_metadata;
use crate::mpris::playback::get_position;


/// Application configuration from CLI
#[derive(Parser, Debug, Clone)]
#[command(author, version, about)]
pub struct Config {
    /// Pipe current lyric line to stdout (default is modern UI)
    #[arg(long)]
    pipe: bool,
    /// Path to local lyrics database (optional)
    #[arg(long)]
    database: Option<String>,
    /// Blocklist for MPRIS player service names (comma-separated, case-insensitive)
    #[arg(long = "block", value_name = "SERVICE1,SERVICE2", value_delimiter = ',')]
    block: Vec<String>,
    /// Enable backend error logging to stderr
    #[arg(long)]
    pub debug_log: bool,
    /// Cached current player service for efficient D-Bus queries
    pub player_service: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pipe: false,
            database: None,
            block: vec![],
            debug_log: false,
            player_service: None,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let cfg = Config::parse();
    let poll_interval = Duration::from_millis(1000);
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
    let service = cfg.player_service.clone().unwrap_or_default();
    let meta = match get_metadata(&service).await {
        Ok(meta) => meta,
        Err(e) => {
            if cfg.debug_log {
                eprintln!("[LyricsMPRIS] D-Bus error getting metadata: {}", e);
            }
            Default::default()
        }
    };
    let pos = match get_position(&service).await {
        Ok(pos) => pos,
        Err(e) => {
            if cfg.debug_log {
                eprintln!("[LyricsMPRIS] D-Bus error getting position: {}", e);
            }
            0.0
        }
    };

    let result = if cfg.pipe {
        crate::ui::pipe::display_lyrics_pipe(meta, pos, poll_interval, db.clone(), db_path.clone(), cfg.clone()).await
    } else {
        crate::ui::modern::display_lyrics_modern(meta, pos, poll_interval, db.clone(), db_path.clone(), cfg.clone()).await
    };

    // Print error if any, for better diagnostics
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        return Err(e);
    }
    Ok(())
}
