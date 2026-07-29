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

/// Track identifiers extracted from or provided for a lyrics lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMatchInfo {
    pub track_isrc: Option<String>,
    pub track_spotify_id: Option<String>,
    pub track_itunes_id: Option<String>,
}

/// Provider result: parsed lines plus optional raw lyrics string (LRC format or JSON)
/// and optional track identifiers extracted from the provider response.
pub type ProviderResult = Result<(Vec<LyricLine>, Option<String>, TrackMatchInfo), LyricsError>;

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
    /// Byte indices of grapheme cluster boundaries in `text`.
    /// To extract grapheme at index i: &text[boundaries[i]..boundaries[i+1]]
    /// The last boundary equals text.len() for convenience.
    pub grapheme_boundaries: Vec<usize>,
    /// Pre-computed time boundaries for each grapheme cluster transition.
    /// Length is grapheme_count() - 1. Empty if single grapheme.
    pub grapheme_times: Vec<f64>,
}

impl WordTiming {
    /// Returns the number of grapheme clusters in this word.
    pub fn grapheme_count(&self) -> usize {
        self.grapheme_boundaries.len().saturating_sub(1)
    }
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
