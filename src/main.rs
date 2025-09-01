mod event;
mod lyrics;
mod mpris;
mod pool;
mod state;
mod text_utils;
mod ui;

use crate::mpris::metadata::get_metadata;
use crate::mpris::playback::get_position;
use clap::Parser;
use std::error::Error;
use std::time::Duration;

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
            providers: vec!["lrclib".to_string(), "musixmatch".to_string()],
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let cfg = Config::parse();
    let mut cfg = cfg;
    providers_from_env_if_empty(&mut cfg);
    let poll_interval = Duration::from_millis(1000);

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
        crate::ui::pipe::display_lyrics_pipe(
            meta,
            pos,
            poll_interval,
            cfg.clone(),
        )
        .await
    } else {
        crate::ui::modern::display_lyrics_modern(
            meta,
            pos,
            poll_interval,
            cfg.clone(),
            !cfg.no_karaoke,
        )
        .await
    };

    // Print error if any, for better diagnostics
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        return Err(e);
    }
    Ok(())
}
