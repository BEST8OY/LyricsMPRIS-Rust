mod event;
mod lyrics;
mod mpris;
mod pool;
mod state;
mod text_utils;
mod timer;
mod ui;

use crate::mpris::metadata::{get_metadata, set_isrc_enabled};
use crate::mpris::playback::get_position;
use clap::Parser;
use std::error::Error;
use std::fs::OpenOptions;
use tracing_subscriber::EnvFilter;
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

    /// Only listen for certain MPRIS player service names (comma-separated, case-insensitive)
    #[arg(
        long = "target",
        value_name = "SERVICE1,SERVICE2",
        value_delimiter = ','
    )]
    target: Vec<String>,

    /// Disable karaoke highlighting (per-word). Use --no-karaoke to disable karaoke (default: enabled).
    #[arg(long = "no-karaoke")]
    pub no_karaoke: bool,
    /// Maximum number of visible lyric lines (treating wrapped lines as one line). Default: unlimited
    #[arg(long = "visible-lines", value_name = "COUNT")]
    pub visible_lines: Option<usize>,
    /// Comma-separated list of lyric providers in preferred order (e.g. "lrclib,musixmatch").
    /// If empty, the LYRIC_PROVIDERS env var will be used as a fallback.
    #[arg(long, value_delimiter = ',')]
    pub providers: Vec<String>,
    /// Path to local lyrics database JSON file for caching
    #[arg(long = "database")]
    pub database: Option<String>,
    /// Cached current player service for efficient D-Bus queries
    pub player_service: Option<String>,
    /// Enable ISRC lookup from metadata for musixmatch lyrics search
    #[arg(long)]
    pub isrc: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pipe: false,
            block: vec![],
            target: vec![],
            providers: vec!["lrclib".to_string(), "musixmatch".to_string()],
            database: None,
            player_service: None,
            no_karaoke: false,
            visible_lines: None,
            isrc: false,
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
/// Returns default metadata on error with warning log.
async fn fetch_initial_metadata(service: &str) -> crate::mpris::TrackMetadata {
    match get_metadata(service).await {
        Ok(meta) => meta,
        Err(e) => {
            tracing::warn!(
                service = %service,
                error = %e,
                "D-Bus error getting initial metadata"
            );
            Default::default()
        }
    }
}

/// Fetches initial playback position from the player service.
///
/// Returns 0.0 on error with warning log.
async fn fetch_initial_position(service: &str) -> f64 {
    match get_position(service).await {
        Ok(pos) => pos,
        Err(e) => {
            tracing::warn!(
                service = %service,
                error = %e,
                "D-Bus error getting initial position"
            );
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
    // Initialize tracing with environment filter.
    // Logs are OFF by default; enable with RUST_LOG=debug (or any level).
    //
    // In --pipe mode: logs go to stderr (safe; no TUI active).
    // In TUI mode:    logs go to a file to avoid corrupting the Ratatui
    //                 alternate screen. Default path: /tmp/lyricsmpris.log.
    //                 Override with RUST_LOG_FILE=/path/to/file.
    //
    // To watch logs live while the TUI runs:
    //   RUST_LOG=debug lyricsmpris &
    //   tail -f /tmp/lyricsmpris.log
    let pipe_mode = std::env::args().any(|a| a == "--pipe");
    if pipe_mode {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_target(true)
            .with_thread_ids(false)
            .with_writer(std::io::stderr)
            .init();
    } else {
        let log_path =
            std::env::var("RUST_LOG_FILE").unwrap_or_else(|_| "/tmp/lyricsmpris.log".to_string());
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .expect("Failed to open log file");
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_target(true)
            .with_thread_ids(false)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(log_file))
            .init();
    }

    let mut cfg = Config::parse();
    providers_from_env_if_empty(&mut cfg);

    set_isrc_enabled(cfg.isrc);

    initialize_database(&cfg).await;

    // Fetch initial state from player (fallback to defaults on error)
    let service = cfg.player_service.as_deref().unwrap_or("");
    let meta = fetch_initial_metadata(service).await;
    let position = fetch_initial_position(service).await;

    // Start UI and propagate any errors
    start_ui(meta, position, cfg).await.map_err(|e| {
        tracing::error!(error = %e, "Application error");
        e
    })
}
