mod event;
mod lyrics;
mod mpris;
mod pool;
mod state;
mod text_utils;
mod timer;
mod ui;

use crate::mpris::metadata::set_isrc_enabled;
use clap::Parser;
use std::error::Error;
use std::fs::OpenOptions;
use tracing_subscriber::EnvFilter;

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
    /// Comma-separated list of lyric providers in preferred order (e.g. "musixmatch,lrclib").
    /// Supported providers: "lrclib", "musixmatch".
    /// If empty, the LYRIC_PROVIDERS env var will be used as a fallback.
    #[arg(long, value_delimiter = ',', value_parser = parse_provider)]
    pub providers: Vec<String>,
    /// Path to SQLite lyrics cache database
    #[arg(long = "database")]
    pub database: Option<String>,
    /// Enable ISRC lookup from metadata for musixmatch lyrics search
    #[arg(long)]
    pub isrc: bool,
    /// Enable Romanization subtitles for foreign scripts (e.g. Romaji/Pinyin)
    #[arg(long)]
    pub romanize: bool,
}

/// Supported lyrics providers.
pub const SUPPORTED_PROVIDERS: &[&str] = &["lrclib", "musixmatch"];

/// Validates and normalizes a lyrics provider name.
pub fn parse_provider(s: &str) -> Result<String, String> {
    let lower = s.trim().to_lowercase();
    if SUPPORTED_PROVIDERS.contains(&lower.as_str()) {
        Ok(lower)
    } else {
        Err(format!(
            "unknown lyrics provider '{s}'. Supported providers are: {}",
            SUPPORTED_PROVIDERS
                .iter()
                .map(|p| format!("'{p}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pipe: false,
            block: vec![],
            target: vec![],
            providers: vec!["musixmatch".to_string(), "lrclib".to_string()],
            database: None,
            no_karaoke: false,
            visible_lines: None,
            isrc: false,
            romanize: false,
        }
    }
}

fn providers_from_env_if_empty(cli: &mut Config) -> Result<(), String> {
    if cli.providers.is_empty()
        && let Ok(s) = std::env::var("LYRIC_PROVIDERS")
    {
        let mut parts = Vec::new();
        for p in s.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
            parts.push(parse_provider(p)?);
        }
        if !parts.is_empty() {
            cli.providers = parts;
        }
    }
    Ok(())
}

/// Initializes the database if a path is provided in the configuration.
async fn initialize_database(config: &Config) {
    if let Some(db_path) = &config.database {
        lyrics::database::initialize(std::path::PathBuf::from(db_path)).await;
    }
}

/// Starts the appropriate UI mode based on configuration.
async fn start_ui(config: Config) -> Result<(), Box<dyn Error + Send + Sync>> {
    if config.pipe {
        crate::ui::pipe::display_lyrics_pipe(config).await
    } else {
        let enable_karaoke = !config.no_karaoke;
        crate::ui::modern::display_lyrics_modern(config, enable_karaoke).await
    }
}

#[tokio::main(flavor = "current_thread")]
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
        // Install panic hook to restore terminal cooked mode before panics print
        crate::ui::modern::install_panic_hook();

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
    if let Err(err) = providers_from_env_if_empty(&mut cfg) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }

    set_isrc_enabled(cfg.isrc);

    initialize_database(&cfg).await;

    // Start UI and propagate any errors
    start_ui(cfg).await.map_err(|e| {
        tracing::error!(error = %e, "Application error");
        e
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_provider_valid() {
        assert_eq!(parse_provider("lrclib").unwrap(), "lrclib");
        assert_eq!(parse_provider("musixmatch").unwrap(), "musixmatch");
        assert_eq!(parse_provider("LRCLIB").unwrap(), "lrclib");
        assert_eq!(parse_provider(" MusixMatch ").unwrap(), "musixmatch");
    }

    #[test]
    fn test_parse_provider_invalid() {
        assert!(parse_provider("lyricmpris").is_err());
        assert!(parse_provider("spotify").is_err());
        assert!(parse_provider("").is_err());
    }

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_providers_from_env_valid() {
        let _lock = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("LYRIC_PROVIDERS", "lrclib, musixmatch");
        }
        let mut cfg = Config {
            providers: vec![],
            ..Default::default()
        };
        let res = providers_from_env_if_empty(&mut cfg);
        assert!(res.is_ok());
        assert_eq!(cfg.providers, vec!["lrclib", "musixmatch"]);
        unsafe {
            std::env::remove_var("LYRIC_PROVIDERS");
        }
    }

    #[test]
    fn test_providers_from_env_invalid() {
        let _lock = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("LYRIC_PROVIDERS", "lyricmpris");
        }
        let mut cfg = Config {
            providers: vec![],
            ..Default::default()
        };
        let res = providers_from_env_if_empty(&mut cfg);
        assert!(res.is_err());
        unsafe {
            std::env::remove_var("LYRIC_PROVIDERS");
        }
    }
}
