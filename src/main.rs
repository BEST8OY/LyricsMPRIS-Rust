mod config;
mod event;
mod lyrics;
mod mpris;
mod pool;
mod state;
mod text_utils;
mod timer;
mod ui;

use crate::config::{
    RuntimeSettings, SUPPORTED_PROVIDERS, load_toml_config, parse_provider_entry,
    resolve_provider_order, resolve_runtime_settings,
};
use crate::mpris::metadata::set_isrc_enabled;
use clap::Parser;
use std::error::Error;
use std::fs::OpenOptions;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// Application configuration from CLI, env, and optional TOML file.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about)]
pub struct Config {
    /// Pipe current lyric line to stdout (default is modern UI)
    #[arg(long)]
    pipe: bool,

    /// Path to optional TOML config file (default: ~/.config/lyricsmpris/config.toml)
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

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

    /// Comma-separated list of lyric providers in preferred order (supports -provider exclusions).
    /// If empty, LYRIC_PROVIDERS env var or config.toml is used, else Morphe's curated priority order.
    #[arg(long, value_delimiter = ',', value_parser = parse_provider_entry)]
    pub providers: Vec<String>,

    /// Enable ISRC lookup from local file metadata for lyrics search
    #[arg(long)]
    pub isrc: bool,

    /// Allow unsynced plain-text lyrics fallback (e.g. Genius / LRCLIB plainLyrics)
    #[arg(long = "allow-plain")]
    pub allow_plain: bool,

    /// Disable splitting parenthesized background vocals onto separate lines
    #[arg(long = "no-split-bg")]
    pub no_split_bg: bool,

    /// Resolved runtime settings (tokens, endpoints, filters)
    #[arg(skip)]
    pub settings: RuntimeSettings,
}

/// Validates and normalizes a lyrics provider name.
pub fn parse_provider(s: &str) -> Result<String, String> {
    parse_provider_entry(s)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pipe: false,
            config: None,
            block: vec![],
            target: vec![],
            providers: SUPPORTED_PROVIDERS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            no_karaoke: false,
            visible_lines: None,
            isrc: false,
            allow_plain: false,
            no_split_bg: false,
            settings: RuntimeSettings::default(),
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

fn apply_toml_and_env(cli: &mut Config) -> Result<(), String> {
    let toml_cfg = load_toml_config(cli.config.as_deref())?;

    providers_from_env_if_empty(cli)?;
    if cli.providers.is_empty()
        && let Some(ref toml_providers) = toml_cfg.providers
    {
        let mut parsed = Vec::new();
        for p in toml_providers {
            parsed.push(parse_provider(p)?);
        }
        cli.providers = parsed;
    }
    cli.providers = resolve_provider_order(&cli.providers);

    if cli.block.is_empty()
        && let Some(block) = &toml_cfg.block
    {
        cli.block = block.clone();
    }
    if cli.target.is_empty()
        && let Some(target) = &toml_cfg.target
    {
        cli.target = target.clone();
    }
    if !cli.no_karaoke && toml_cfg.karaoke == Some(false) {
        cli.no_karaoke = true;
    }
    if cli.visible_lines.is_none() {
        cli.visible_lines = toml_cfg.visible_lines;
    }
    if !cli.isrc && toml_cfg.isrc == Some(true) {
        cli.isrc = true;
    }

    cli.settings =
        resolve_runtime_settings(&toml_cfg, cli.allow_plain, cli.no_split_bg || cli.pipe);
    cli.allow_plain = cli.settings.allow_plain;
    Ok(())
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
    let pipe_mode = std::env::args().any(|a| a == "--pipe");
    if pipe_mode {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_target(true)
            .with_thread_ids(false)
            .with_writer(std::io::stderr)
            .init();
    } else {
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
    if let Err(err) = apply_toml_and_env(&mut cfg) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }

    set_isrc_enabled(cfg.isrc);

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
        assert_eq!(parse_provider("AMLL").unwrap(), "amll");
        assert_eq!(parse_provider(" -Genius ").unwrap(), "-genius");
    }

    #[test]
    fn test_parse_provider_invalid() {
        assert!(parse_provider("lyricmpris").is_err());
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
