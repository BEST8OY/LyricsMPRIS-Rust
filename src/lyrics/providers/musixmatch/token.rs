//! Token management and dynamic acquisition for Musixmatch API.

use crate::lyrics::types::LyricsError;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) const API_BASE: &str = "https://apic.musixmatch.com/ws/1.1/";
pub(crate) const APP_ID: &str = "android-player-v1.0";
pub(crate) const DEFAULT_USER_AGENT: &str = "Dalvik/2.1.0 (Linux; U; Android 17)";
pub(crate) const DEFAULT_COOKIE: &str = "AWSELB=0; AWSELBCORS=0";

pub(crate) fn request_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub(crate) fn open_api_get(client: &Client, url: &str) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header("User-Agent", DEFAULT_USER_AGENT)
        .header("Cookie", DEFAULT_COOKIE)
        .header("Accept", "application/json")
}

pub(crate) const REQUEST_THROTTLE: Duration = Duration::from_millis(500);

#[derive(serde::Serialize, serde::Deserialize)]
struct HeldToken {
    user_token: String,
}

fn get_cache_token_path() -> std::path::PathBuf {
    if let Ok(xdg) = env::var("XDG_CACHE_HOME")
        && !xdg.trim().is_empty()
    {
        std::path::PathBuf::from(xdg)
            .join("lyricsmpris")
            .join("musixmatch_token.json")
    } else if let Ok(home) = env::var("HOME")
        && !home.trim().is_empty()
    {
        std::path::PathBuf::from(home)
            .join(".cache")
            .join("lyricsmpris")
            .join("musixmatch_token.json")
    } else {
        env::temp_dir()
            .join("lyricsmpris")
            .join("musixmatch_token.json")
    }
}

pub(crate) fn is_valid_token(token: &str) -> bool {
    let t = token.trim();
    if t.is_empty() || t.contains("UpgradeOnly") || t.chars().all(|c| c == '0' || c == '-') {
        return false;
    }
    true
}

fn load_token_from_disk() -> Option<String> {
    load_token_from_path(&get_cache_token_path())
}

fn load_token_from_path(path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let held: HeldToken = serde_json::from_str(&content).ok()?;
    let token = held.user_token.trim().to_string();
    if is_valid_token(&token) {
        Some(token)
    } else {
        None
    }
}

fn save_token_to_disk(token: &str) {
    save_token_to_path(token, &get_cache_token_path());
}

fn save_token_to_path(token: &str, path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let held = HeldToken {
        user_token: token.to_string(),
    };
    if let Ok(json) = serde_json::to_string(&held) {
        let _ = std::fs::write(path, json);
    }
}

fn clear_disk_token() {
    clear_token_at_path(&get_cache_token_path());
}

fn clear_token_at_path(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// Requests a fresh anonymous Musixmatch user token directly from the API (`token.get`).
pub async fn fetch_fresh_musixmatch_token(client: &Client) -> Result<String, LyricsError> {
    let req_id = request_id();
    let url = format!("{API_BASE}token.get?user_language=en&app_id={APP_ID}&t={req_id}");
    let resp = open_api_get(client, &url).send().await?;

    if !resp.status().is_success() {
        return Err(LyricsError::Http(resp.status().as_u16()));
    }

    let json: Value = resp.json().await?;
    let code = super::extract::get_root_status_code(&json).unwrap_or(0);
    if code != 200 {
        if code == 429 || code >= 500 {
            return Err(LyricsError::Http(code as u16));
        }
        return Err(LyricsError::Api(format!(
            "Musixmatch token.get status {}",
            code
        )));
    }

    let token = json
        .pointer("/message/body/user_token")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| is_valid_token(s));

    match token {
        Some(t) => {
            tracing::debug!(token_prefix = %&t[..t.len().min(8)], "Obtained fresh Musixmatch user token");
            Ok(t)
        }
        None => Err(LyricsError::Api(
            "Missing or invalid (empty, UpgradeOnly, or dummy) user_token in Musixmatch token.get response"
                .to_string(),
        )),
    }
}

/// Round-robin token manager for multiple Musixmatch usertokens,
/// with automatic disk-backed dynamic caching and Morphe 500ms request pacing.
pub(crate) struct TokenManager {
    tokens: Vec<String>,
    invalid_tokens: HashSet<String>,
    current_index: usize,
    cached_dynamic_token: Option<String>,
    last_request: Option<Instant>,
}

impl TokenManager {
    /// Initialize the token manager from the MUSIXMATCH_USERTOKEN environment variable.
    /// Supports multiple tokens separated by commas.
    pub(crate) fn new() -> Self {
        let tokens = env::var("MUSIXMATCH_USERTOKEN")
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let cached_dynamic_token = load_token_from_disk();

        TokenManager {
            tokens,
            invalid_tokens: HashSet::new(),
            current_index: 0,
            cached_dynamic_token,
            last_request: None,
        }
    }

    /// Returns available configured env/TOML tokens count.
    pub(crate) fn env_token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Merges comma-separated tokens from TOML/CLI config once without duplicating env entries
    /// or re-adding tokens previously rejected as expired.
    pub(crate) fn seed_configured_tokens(&mut self, csv: &str) {
        for raw in csv.split(',') {
            let tok = raw.trim();
            if !tok.is_empty()
                && !self.invalid_tokens.contains(tok)
                && !self.tokens.iter().any(|existing| existing == tok)
            {
                self.tokens.push(tok.to_string());
            }
        }
    }

    /// Marks an explicit env/TOML token invalid for this session and removes it from rotation.
    pub(crate) fn invalidate_env_token(&mut self, token: &str) {
        let tok = token.trim();
        if tok.is_empty() {
            return;
        }
        self.invalid_tokens.insert(tok.to_string());
        if let Some(pos) = self.tokens.iter().position(|t| t == tok) {
            self.tokens.remove(pos);
            if self.tokens.is_empty() {
                self.current_index = 0;
            } else if pos < self.current_index {
                self.current_index -= 1;
            } else if self.current_index >= self.tokens.len() {
                self.current_index = 0;
            }
        }
    }

    /// Get the next env token in round-robin fashion if available.
    pub(crate) fn next_env_token(&mut self) -> Option<String> {
        if self.tokens.is_empty() {
            return None;
        }

        let token = self.tokens[self.current_index].clone();
        self.current_index = (self.current_index + 1) % self.tokens.len();
        Some(token)
    }

    /// Get cached dynamic token if present in memory or on disk.
    pub(crate) fn get_dynamic_token(&mut self) -> Option<String> {
        if let Some(ref t) = self.cached_dynamic_token {
            return Some(t.clone());
        }
        let loaded = load_token_from_disk();
        if let Some(ref t) = loaded {
            self.cached_dynamic_token = Some(t.clone());
        }
        loaded
    }

    /// Update cached dynamic token in memory and on disk.
    pub(crate) fn set_dynamic_token(&mut self, token: String) {
        save_token_to_disk(&token);
        self.cached_dynamic_token = Some(token);
    }

    /// Clear cached dynamic token from memory and disk.
    pub(crate) fn clear_dynamic_token(&mut self) {
        clear_disk_token();
        self.cached_dynamic_token = None;
    }
}

pub(crate) static TOKEN_MANAGER: LazyLock<Mutex<TokenManager>> =
    LazyLock::new(|| Mutex::new(TokenManager::new()));

/// Enforces Morphe's 500ms minimum spacing between Musixmatch API requests.
pub(crate) async fn throttle_request() {
    let delay = {
        let Ok(mut mgr) = TOKEN_MANAGER.lock() else {
            return;
        };
        let now = Instant::now();
        let delay = mgr.last_request.and_then(|last| {
            let elapsed = now.duration_since(last);
            (elapsed < REQUEST_THROTTLE).then(|| REQUEST_THROTTLE - elapsed)
        });
        mgr.last_request = Some(now + delay.unwrap_or_default());
        delay
    };

    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
}

/// Clears any cached guest token across the global token manager without imposing a multi-minute ban.
pub(crate) fn clear_cached_dynamic_token() {
    if let Ok(mut mgr) = TOKEN_MANAGER.lock() {
        mgr.clear_dynamic_token();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_manager_round_robin() {
        let mut tm = TokenManager {
            tokens: vec![
                "token1".to_string(),
                "token2".to_string(),
                "token3".to_string(),
            ],
            invalid_tokens: HashSet::new(),
            current_index: 0,
            cached_dynamic_token: None,
            last_request: None,
        };

        assert_eq!(tm.next_env_token(), Some("token1".to_string()));
        assert_eq!(tm.next_env_token(), Some("token2".to_string()));
        assert_eq!(tm.next_env_token(), Some("token3".to_string()));
        assert_eq!(tm.next_env_token(), Some("token1".to_string())); // Wraps around
    }

    #[test]
    fn test_invalidate_env_token_prunes_and_blocks_reseed() {
        let mut tm = TokenManager {
            tokens: vec!["tok_a".into(), "tok_b".into()],
            invalid_tokens: HashSet::new(),
            current_index: 0,
            cached_dynamic_token: None,
            last_request: None,
        };

        assert_eq!(tm.next_env_token().as_deref(), Some("tok_a"));
        tm.invalidate_env_token("tok_a");
        assert_eq!(tm.env_token_count(), 1);
        assert_eq!(tm.next_env_token().as_deref(), Some("tok_b"));

        // Re-seeding from TOML/env config does not restore the invalidated token.
        tm.seed_configured_tokens("tok_a, tok_b, tok_c");
        assert_eq!(tm.tokens, vec!["tok_b".to_string(), "tok_c".to_string()]);
    }

    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_token_manager_dynamic_caching() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let mut tm = TokenManager {
            tokens: Vec::new(),
            invalid_tokens: HashSet::new(),
            current_index: 0,
            cached_dynamic_token: None,
            last_request: None,
        };

        assert_eq!(tm.next_env_token(), None);

        // Memory cache returns populated token without disk access
        tm.cached_dynamic_token = Some("fresh_dynamic_token_123".to_string());
        assert_eq!(
            tm.get_dynamic_token(),
            Some("fresh_dynamic_token_123".to_string())
        );
    }

    #[test]
    fn test_token_disk_io_and_upgrade_only_filtering() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let temp_dir = env::temp_dir().join(format!("mxm_test_{}", std::process::id()));
        let token_path = temp_dir.join("test_token.json");

        // Saving valid token
        save_token_to_path("valid_token_xyz", &token_path);
        assert_eq!(
            load_token_from_path(&token_path),
            Some("valid_token_xyz".to_string())
        );

        // UpgradeOnly token rejection
        save_token_to_path("UpgradeOnly_token_abc", &token_path);
        assert_eq!(load_token_from_path(&token_path), None);

        // All-zeros dummy token rejection (apic-desktop poisoned token)
        save_token_to_path(
            "00000000000000000000000000000000000000000000000000000000",
            &token_path,
        );
        assert_eq!(load_token_from_path(&token_path), None);

        // Clear token
        clear_token_at_path(&token_path);
        assert_eq!(load_token_from_path(&token_path), None);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_clear_cached_dynamic_token() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let mut tm = TokenManager {
            tokens: Vec::new(),
            invalid_tokens: HashSet::new(),
            current_index: 0,
            cached_dynamic_token: Some("dummy".to_string()),
            last_request: None,
        };

        assert_eq!(tm.cached_dynamic_token.as_deref(), Some("dummy"));
        tm.clear_dynamic_token();
        assert!(tm.cached_dynamic_token.is_none());
    }

    #[test]
    fn test_token_parsing_missing_user_token_rejected() {
        // Response without user_token (body is null, as returned by Musixmatch when guid is missing)
        let json_without_guid: Value = serde_json::from_str(
            r#"{
            "message": {
                "header": {
                    "status_code": 200,
                    "execute_time": 0.014
                },
                "body": null
            }
        }"#,
        )
        .unwrap();

        let extracted = json_without_guid
            .pointer("/message/body/user_token")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| is_valid_token(s));

        assert_eq!(extracted, None);
    }

    #[tokio::test]
    async fn test_throttle_request_enforces_500ms_pacing() {
        let _lock = TEST_MUTEX.lock().unwrap();
        if let Ok(mut mgr) = TOKEN_MANAGER.lock() {
            mgr.last_request = Some(Instant::now());
        }
        let start = Instant::now();
        throttle_request().await;
        assert!(
            start.elapsed() >= Duration::from_millis(450),
            "Expected ~500ms pacing between requests, elapsed={:?}",
            start.elapsed()
        );
    }
}
