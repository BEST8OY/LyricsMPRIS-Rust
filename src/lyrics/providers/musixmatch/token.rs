//! Token management and dynamic acquisition for Musixmatch API.

use crate::lyrics::types::LyricsError;
use once_cell::sync::Lazy;
use reqwest::Client;
use serde_json::Value;
use std::env;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_USER_AGENT: &str =
    "Musixmatch/2025120901 CFNetwork/3860.300.31 Darwin/25.2.0";

pub(crate) const SPACING: Duration = Duration::from_millis(1500);
pub(crate) const COOLDOWN: Duration = Duration::from_secs(600);

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
    let guid = uuid::Uuid::new_v4().to_string();
    let url = format!(
        "https://apic-appmobile.musixmatch.com/ws/1.1/token.get?app_id=mac-ios-v2.0&guid={guid}"
    );
    let resp = client
        .get(&url)
        .header("User-Agent", DEFAULT_USER_AGENT)
        .header("X-User-Agent", DEFAULT_USER_AGENT)
        .header("Cookie", format!("x-mxm-token-guid={guid}"))
        .header("X-Cookie", format!("x-mxm-token-guid={guid}"))
        .header("x-mxm-app-version", "10.1.1")
        .header("Accept", "application/json")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await?;

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
/// with automatic disk-backed dynamic caching, request spacing, and throttling cooldown.
pub(crate) struct TokenManager {
    tokens: Vec<String>,
    current_index: usize,
    cached_dynamic_token: Option<String>,
    blocked_until: Option<Instant>,
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
            current_index: 0,
            cached_dynamic_token,
            blocked_until: None,
            last_request: None,
        }
    }

    /// Returns available configured env tokens count.
    pub(crate) fn env_token_count(&self) -> usize {
        self.tokens.len()
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

    /// Returns true if currently in throttling cooldown.
    pub(crate) fn is_cooling_down(&self) -> bool {
        self.blocked_until
            .is_some_and(|until| Instant::now() < until)
    }

    /// Triggers a 10-minute network cooldown and purges invalid dynamic tokens.
    pub(crate) fn trigger_cooldown(&mut self) {
        self.blocked_until = Some(Instant::now() + COOLDOWN);
        self.clear_dynamic_token();
    }
}

pub(crate) static TOKEN_MANAGER: Lazy<Mutex<TokenManager>> =
    Lazy::new(|| Mutex::new(TokenManager::new()));

/// Waits for rate-limiting spacing between requests.
/// Returns false if Musixmatch is blocked under network cooldown.
pub(crate) async fn wait_for_gate() -> bool {
    let delay = {
        let Ok(mut mgr) = TOKEN_MANAGER.lock() else {
            return true;
        };
        if let Some(until) = mgr.blocked_until {
            if Instant::now() < until {
                return false;
            }
            mgr.blocked_until = None;
        }
        let now = Instant::now();
        let delay = if let Some(last) = mgr.last_request {
            let elapsed = now.duration_since(last);
            if elapsed < SPACING {
                Some(SPACING - elapsed)
            } else {
                None
            }
        } else {
            None
        };
        mgr.last_request = Some(now + delay.unwrap_or_default());
        delay
    };

    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
    true
}

/// Triggers cooldown across the global token manager.
pub(crate) fn trigger_gate_cooldown() {
    if let Ok(mut mgr) = TOKEN_MANAGER.lock() {
        mgr.trigger_cooldown();
    }
}

/// Checks if cooldown is currently active.
#[allow(dead_code)]
pub(crate) fn is_gate_cooling_down() -> bool {
    if let Ok(mgr) = TOKEN_MANAGER.lock() {
        mgr.is_cooling_down()
    } else {
        false
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
            current_index: 0,
            cached_dynamic_token: None,
            blocked_until: None,
            last_request: None,
        };

        assert_eq!(tm.next_env_token(), Some("token1".to_string()));
        assert_eq!(tm.next_env_token(), Some("token2".to_string()));
        assert_eq!(tm.next_env_token(), Some("token3".to_string()));
        assert_eq!(tm.next_env_token(), Some("token1".to_string())); // Wraps around
    }

    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_token_manager_dynamic_caching() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let mut tm = TokenManager {
            tokens: Vec::new(),
            current_index: 0,
            cached_dynamic_token: None,
            blocked_until: None,
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
    fn test_cooldown_gating() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let mut tm = TokenManager {
            tokens: Vec::new(),
            current_index: 0,
            cached_dynamic_token: Some("dummy".to_string()),
            blocked_until: None,
            last_request: None,
        };

        assert!(!tm.is_cooling_down());
        tm.trigger_cooldown();
        assert!(tm.is_cooling_down());
        assert!(tm.cached_dynamic_token.is_none());
    }
}
