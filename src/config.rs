//! Application configuration loaded from CLI, environment variables, and optional TOML file.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Curated default provider priority order matching Morphe's `LyricsProviders.PROVIDER_ORDER`.
pub const SUPPORTED_PROVIDERS: &[&str] = &[
    "apple",
    "lyricsplus",
    "lrcred",
    "blyrics",
    "lyricify",
    "unison",
    "petitlyrics",
    "amll",
    "qq",
    "simpmusic",
    "kugou",
    "musixmatch",
    "ytmusic",
    "lrclib",
    "netease",
    "luna",
    "kuwo",
    "lunabeat",
    "captions",
    "spotify",
    "deezer",
    "rmm",
    "genius",
];

/// Han script conversion mode applied by `LyricsTextFormatter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HanMode {
    #[default]
    None,
    Simplified,
    Traditional,
}

impl HanMode {
    pub fn from_str_loose(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "simplified" | "hans" | "zh-hans" | "cn" | "sc" => Self::Simplified,
            "traditional" | "hant" | "zh-hant" | "tw" | "hk" | "tc" => Self::Traditional,
            _ => Self::None,
        }
    }
}

/// Optional user tokens/cookies for authenticated providers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenConfig {
    pub musixmatch_usertoken: Option<String>,
    pub spotify_sp_dc: Option<String>,
    pub apple_media_user_token: Option<String>,
    pub deezer_arl: Option<String>,
    pub youtube_cookies: Option<String>,
}

/// Optional custom HTTP base endpoint overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointConfig {
    pub lrclib: Option<String>,
    pub amll: Option<String>,
    pub blyrics: Option<String>,
    pub lyricsplus: Option<String>,
    pub lunabeat: Option<String>,
    pub simpmusic: Option<String>,
    pub unison: Option<String>,
}

/// Text filtering, cleaning, and formatting preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    pub custom_metadata_regex: Option<String>,
    pub lyrics_text_filter_regex: Option<String>,
    pub credit_line_markers: Option<String>,
    pub force_capitalize: bool,
    pub strip_trailing_punctuation: bool,
    pub han_mode: HanMode,
    pub split_background: bool,
    pub retime_overlaps: bool,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            custom_metadata_regex: None,
            lyrics_text_filter_regex: None,
            credit_line_markers: None,
            force_capitalize: false,
            strip_trailing_punctuation: false,
            han_mode: HanMode::None,
            split_background: true,
            retime_overlaps: true,
        }
    }
}

/// Raw TOML file schema (`~/.config/lyricsmpris/config.toml`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TomlConfig {
    pub providers: Option<Vec<String>>,
    pub allow_plain: Option<bool>,
    pub karaoke: Option<bool>,
    pub visible_lines: Option<usize>,
    pub isrc: Option<bool>,
    pub block: Option<Vec<String>>,
    pub target: Option<Vec<String>>,
    pub tokens: TokenConfig,
    pub endpoints: EndpointConfig,
    pub filters: FilterConfig,
}

/// Unified runtime settings resolved from CLI + Env + TOML.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeSettings {
    pub allow_plain: bool,
    pub tokens: TokenConfig,
    pub endpoints: EndpointConfig,
    pub filters: FilterConfig,
}

/// Validates and normalizes a provider entry (`provider` or `-provider`).
pub fn parse_provider_entry(s: &str) -> Result<String, String> {
    let trimmed = s.trim().to_ascii_lowercase();
    let name = trimmed.strip_prefix('-').unwrap_or(trimmed.as_str());
    if SUPPORTED_PROVIDERS.contains(&name) {
        Ok(trimmed)
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

/// Resolves an ordered provider list supporting `-provider` exclusions (Morphe semantics).
pub fn resolve_provider_order(entries: &[String]) -> Vec<String> {
    if entries.is_empty() {
        return SUPPORTED_PROVIDERS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
    }

    let mut excluded = HashSet::new();
    let mut positive = Vec::new();
    let mut seen = HashSet::new();

    for entry in entries {
        let lower = entry.trim().to_ascii_lowercase();
        if let Some(name) = lower.strip_prefix('-') {
            excluded.insert(name.to_string());
        } else if !lower.is_empty() && seen.insert(lower.clone()) {
            positive.push(lower);
        }
    }

    if positive.is_empty() {
        SUPPORTED_PROVIDERS
            .iter()
            .filter(|p| !excluded.contains(**p))
            .map(|p| (*p).to_string())
            .collect()
    } else {
        positive
            .into_iter()
            .filter(|p| !excluded.contains(p))
            .collect()
    }
}

/// Default config path (`$XDG_CONFIG_HOME/lyricsmpris/config.toml` or `~/.config/lyricsmpris/config.toml`).
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("lyricsmpris").join("config.toml"))
}

/// Loads TOML configuration from an explicit path (erroring if missing) or the default path if present.
pub fn load_toml_config(explicit_path: Option<&Path>) -> Result<TomlConfig, String> {
    if let Some(path) = explicit_path {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read config '{}': {e}", path.display()))?;
        return toml::from_str(&content)
            .map_err(|e| format!("failed to parse config '{}': {e}", path.display()));
    }

    let Some(default_path) = default_config_path() else {
        return Ok(TomlConfig::default());
    };
    if !default_path.exists() {
        return Ok(TomlConfig::default());
    }
    let content = std::fs::read_to_string(&default_path)
        .map_err(|e| format!("failed to read config '{}': {e}", default_path.display()))?;
    toml::from_str(&content)
        .map_err(|e| format!("failed to parse config '{}': {e}", default_path.display()))
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Merges environment variables on top of TOML `RuntimeSettings`.
pub fn resolve_runtime_settings(
    toml: &TomlConfig,
    cli_allow_plain: bool,
    no_split_bg: bool,
) -> RuntimeSettings {
    let mut tokens = toml.tokens.clone();
    if let Some(v) = non_empty_env("MUSIXMATCH_USERTOKEN") {
        tokens.musixmatch_usertoken = Some(v);
    }
    if let Some(v) = non_empty_env("SPOTIFY_SP_DC") {
        tokens.spotify_sp_dc = Some(v);
    }
    if let Some(v) = non_empty_env("APPLE_MEDIA_USER_TOKEN") {
        tokens.apple_media_user_token = Some(v);
    }
    if let Some(v) = non_empty_env("DEEZER_ARL") {
        tokens.deezer_arl = Some(v);
    }
    if let Some(v) = non_empty_env("YOUTUBE_COOKIES") {
        tokens.youtube_cookies = Some(v);
    }

    let mut endpoints = toml.endpoints.clone();
    if let Some(v) = non_empty_env("LRCLIB_ENDPOINT") {
        endpoints.lrclib = Some(v);
    }
    if let Some(v) = non_empty_env("AMLL_ENDPOINT") {
        endpoints.amll = Some(v);
    }
    if let Some(v) = non_empty_env("BLYRICS_ENDPOINT") {
        endpoints.blyrics = Some(v);
    }
    if let Some(v) = non_empty_env("LYRICSPLUS_ENDPOINT") {
        endpoints.lyricsplus = Some(v);
    }
    if let Some(v) = non_empty_env("LUNABEAT_ENDPOINT") {
        endpoints.lunabeat = Some(v);
    }
    if let Some(v) = non_empty_env("SIMPMUSIC_ENDPOINT") {
        endpoints.simpmusic = Some(v);
    }
    if let Some(v) = non_empty_env("UNISON_ENDPOINT") {
        endpoints.unison = Some(v);
    }

    let mut filters = toml.filters.clone();
    if no_split_bg {
        filters.split_background = false;
    }
    if let Some(v) = non_empty_env("LYRICS_HAN_MODE") {
        filters.han_mode = HanMode::from_str_loose(&v);
    }

    let allow_plain = cli_allow_plain
        || non_empty_env("LYRICS_ALLOW_PLAIN")
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        || toml.allow_plain.unwrap_or(false);

    RuntimeSettings {
        allow_plain,
        tokens,
        endpoints,
        filters,
    }
}

/// Validates a custom base URL override (must be `http://` or `https://`).
pub fn validate_http_base_url(override_url: Option<&str>) -> Option<String> {
    let raw = override_url?.trim();
    if raw.is_empty() {
        return None;
    }
    let parsed = reqwest::Url::parse(raw).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(raw.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_provider_order_exclusions_and_explicit() {
        let default_list = resolve_provider_order(&[]);
        assert_eq!(default_list.len(), SUPPORTED_PROVIDERS.len());
        assert_eq!(default_list[0], "apple");
        assert_eq!(default_list.last().map(String::as_str), Some("genius"));

        let excluded = resolve_provider_order(&["-genius".into(), "-spotify".into()]);
        assert!(!excluded.contains(&"genius".to_string()));
        assert!(!excluded.contains(&"spotify".to_string()));
        assert_eq!(excluded.len(), SUPPORTED_PROVIDERS.len() - 2);

        let explicit = resolve_provider_order(&["lrclib".into(), "amll".into(), "-amll".into()]);
        assert_eq!(explicit, vec!["lrclib"]);
    }

    #[test]
    fn test_parse_provider_entry_supports_exclusion() {
        assert_eq!(parse_provider_entry("AMLL").unwrap(), "amll");
        assert_eq!(parse_provider_entry("-Genius").unwrap(), "-genius");
        assert!(parse_provider_entry("unknown_xyz").is_err());
    }

    #[test]
    fn test_validate_http_base_url() {
        assert_eq!(
            validate_http_base_url(Some("https://api.amll.dev/")),
            Some("https://api.amll.dev".to_string())
        );
        assert_eq!(validate_http_base_url(Some("ftp://example.com")), None);
        assert_eq!(validate_http_base_url(Some("not-a-url")), None);
    }
}
