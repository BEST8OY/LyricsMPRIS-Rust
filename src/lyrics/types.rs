use once_cell::sync::Lazy;
use reqwest::Client;
use thiserror::Error;

// Shared HTTP client with reasonable defaults for timeouts
static HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .user_agent("LyricsMPRIS/1.0")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client")
});

/// Provider result: parsed lines plus optional raw LRC string for DB storage
pub type ProviderResult = Result<(Vec<LyricLine>, Option<String>), LyricsError>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LyricLine {
    pub time: f64,
    pub text: String,
    /// Optional per-word timings (start, end, text) for karaoke rendering.
    pub words: Option<Vec<WordTiming>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WordTiming {
    pub start: f64,
    pub end: f64,
    pub text: String,
    /// Grapheme cluster slices of `text` (precomputed to avoid per-tick allocations).
    pub graphemes: Vec<String>,
    /// Byte offsets corresponding to the start of each grapheme in `text`.
    pub grapheme_byte_offsets: Vec<usize>,
}

#[derive(Error, Debug)]
pub enum LyricsError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("API error: {0}")]
    Api(String),
    #[error("Serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

// Re-export HTTP client for providers within the lyrics module
pub(crate) fn http_client() -> &'static Client {
    &HTTP_CLIENT
}
