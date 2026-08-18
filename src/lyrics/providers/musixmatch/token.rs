//! Token management and dynamic acquisition for Musixmatch API.

use crate::lyrics::types::LyricsError;
use once_cell::sync::Lazy;
use reqwest::Client;
use serde_json::Value;
use std::env;
use std::sync::Mutex;

const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Electron/1.7.6 Safari/537.36";

/// Requests a fresh anonymous Musixmatch user token directly from the API (`token.get`).
pub async fn fetch_fresh_musixmatch_token(client: &Client) -> Result<String, LyricsError> {
    let url = "https://apic-desktop.musixmatch.com/ws/1.1/token.get?app_id=web-desktop-app-v1.0";
    let resp = client
        .get(url)
        .header("User-Agent", DEFAULT_USER_AGENT)
        .header("Cookie", "x-mxm-token-guid=")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(LyricsError::Api(format!(
            "Failed to get Musixmatch token: HTTP {}",
            resp.status()
        )));
    }

    let json: Value = resp.json().await?;
    let code = super::extract::get_root_status_code(&json).unwrap_or(0);
    if code != 200 {
        return Err(LyricsError::Api(format!(
            "Musixmatch token.get status {}",
            code
        )));
    }

    let token = json
        .pointer("/message/body/user_token")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    match token {
        Some(t) => {
            tracing::debug!(token_prefix = %&t[..t.len().min(8)], "Obtained fresh Musixmatch user token");
            Ok(t)
        }
        None => Err(LyricsError::Api(
            "Missing user_token in Musixmatch token.get response".to_string(),
        )),
    }
}

/// Round-robin token manager for multiple Musixmatch usertokens,
/// with automatic dynamic fallback via token.get API.
pub(crate) struct TokenManager {
    tokens: Vec<String>,
    current_index: usize,
    cached_dynamic_token: Option<String>,
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

        TokenManager {
            tokens,
            current_index: 0,
            cached_dynamic_token: None,
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

    /// Get cached dynamic token if present.
    pub(crate) fn get_dynamic_token(&self) -> Option<String> {
        self.cached_dynamic_token.clone()
    }

    /// Update cached dynamic token.
    pub(crate) fn set_dynamic_token(&mut self, token: String) {
        self.cached_dynamic_token = Some(token);
    }
}

pub(crate) static TOKEN_MANAGER: Lazy<Mutex<TokenManager>> =
    Lazy::new(|| Mutex::new(TokenManager::new()));

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
        };

        assert_eq!(tm.next_env_token(), Some("token1".to_string()));
        assert_eq!(tm.next_env_token(), Some("token2".to_string()));
        assert_eq!(tm.next_env_token(), Some("token3".to_string()));
        assert_eq!(tm.next_env_token(), Some("token1".to_string())); // Wraps around
    }

    #[test]
    fn test_token_manager_dynamic_caching() {
        let mut tm = TokenManager {
            tokens: Vec::new(),
            current_index: 0,
            cached_dynamic_token: None,
        };

        assert_eq!(tm.next_env_token(), None);
        assert_eq!(tm.get_dynamic_token(), None);

        tm.set_dynamic_token("fresh_dynamic_token_123".to_string());
        assert_eq!(
            tm.get_dynamic_token(),
            Some("fresh_dynamic_token_123".to_string())
        );
    }
}
