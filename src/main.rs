mod event;
mod lyrics;
mod mpris;
mod pool;
mod state;
mod timer;
mod text_utils;
mod ui;

use crate::mpris::metadata::get_metadata;
use crate::mpris::playback::get_position;
use clap::Parser;
use std::error::Error;
// polling removed; no Duration needed here

/// Application configuration from CLI
#[derive(Parser, Debug, Clone)]
#[command(author, version, about)]
pub struct Config {
    /// Pipe current lyric line to stdout (default is modern UI)
    #[arg(long)]
    pipe: bool,
    
    /// Blocklist for MPRIS player service names (comma-separated, case-insensitive)
    #[arg(
        long = "block",
        value_name = "SERVICE1,SERVICE2",
        value_delimiter = ','
    )]
    block: Vec<String>,
    /// Enable backend error logging to stderr
    #[arg(long)]
    pub debug_log: bool,
    /// Disable karaoke highlighting (per-word). Use --no-karaoke to disable karaoke (default: enabled).
    #[arg(long = "no-karaoke")]
    pub no_karaoke: bool,
    /// Comma-separated list of lyric providers in preferred order (e.g. "lrclib,musixmatch").
    /// If empty, the LYRIC_PROVIDERS env var will be used as a fallback.
    #[arg(long, value_delimiter = ',')]
    pub providers: Vec<String>,
    /// Path to local lyrics database JSON file for caching
    #[arg(long = "database")]
    pub database: Option<String>,
    /// Cached current player service for efficient D-Bus queries
    pub player_service: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pipe: false,
            block: vec![],
            debug_log: false,
            providers: vec!["lrclib".to_string(), "musixmatch".to_string()],
            database: None,
            player_service: None,
            no_karaoke: false,
        }
    }
}

fn providers_from_env_if_empty(cli: &mut Config) {
    if cli.providers.is_empty()
        && let Ok(s) = std::env::var("LYRIC_PROVIDERS")
    {
        let parts: Vec<String> = s
            .split(',')
            .map(|p| p.trim().to_lowercase())
            .filter(|p| !p.is_empty())
            .collect();
        if !parts.is_empty() {
            cli.providers = parts;
        }
    }
}

/// Initializes the database if a path is provided in the configuration.
async fn initialize_database(config: &Config) {
    if let Some(db_path) = &config.database {
        lyrics::database::initialize(std::path::PathBuf::from(db_path)).await;
    }
}

/// Fetches initial metadata from the player service.
///
/// Returns default metadata on error, logging if debug is enabled.
async fn fetch_initial_metadata(service: &str, debug_log: bool) -> crate::mpris::TrackMetadata {
    match get_metadata(service).await {
        Ok(meta) => meta,
        Err(e) => {
            if debug_log {
                eprintln!("[LyricsMPRIS] D-Bus error getting metadata: {}", e);
            }
            Default::default()
        }
    }
}

/// Fetches initial playback position from the player service.
///
/// Returns 0.0 on error, logging if debug is enabled.
async fn fetch_initial_position(service: &str, debug_log: bool) -> f64 {
    match get_position(service).await {
        Ok(pos) => pos,
        Err(e) => {
            if debug_log {
                eprintln!("[LyricsMPRIS] D-Bus error getting position: {}", e);
            }
            0.0
        }
    }
}

/// Starts the appropriate UI mode based on configuration.
async fn start_ui(
    meta: crate::mpris::TrackMetadata,
    position: f64,
    config: Config,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if config.pipe {
        crate::ui::pipe::display_lyrics_pipe(meta, position, config).await
    } else {
        let enable_karaoke = !config.no_karaoke;
        crate::ui::modern::display_lyrics_modern(meta, position, config, enable_karaoke).await
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut cfg = Config::parse();
    providers_from_env_if_empty(&mut cfg);

    initialize_database(&cfg).await;

    // Fetch initial state from player (fallback to defaults on error)
    let service = cfg.player_service.as_deref().unwrap_or("");
    let meta = fetch_initial_metadata(service, cfg.debug_log).await;
    let position = fetch_initial_position(service, cfg.debug_log).await;

    // Start UI and propagate any errors
    start_ui(meta, position, cfg).await.map_err(|e| {
        eprintln!("Error: {}", e);
        e
    })
}
