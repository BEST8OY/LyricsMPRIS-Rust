use once_cell::sync::Lazy;
use reqwest::Client;
use thiserror::Error;

// Shared HTTP client with reasonable defaults for timeouts
static HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .user_agent("LyricsMPRIS/1.0")
        .timeout(std::time::Duration::from_secs(4))
        .build()
        .expect("failed to build HTTP client")
});

/// Track identifiers extracted from or provided for a lyrics lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMatchInfo {
    pub track_isrcs: Vec<String>,
    pub track_spotify_ids: Vec<String>,
    pub track_itunes_ids: Vec<String>,
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
    /// Optional phonetic transcription/transliteration for foreign writing systems (e.g. Romaji/Pinyin).
    pub romanized: Option<String>,
}

impl LyricLine {
    pub fn new(time: f64, text: impl Into<String>) -> Self {
        Self {
            time,
            text: text.into(),
            words: None,
            romanized: None,
        }
    }

    pub fn with_words(mut self, words: Option<Vec<WordTiming>>) -> Self {
        self.words = words;
        self
    }

    #[allow(dead_code)]
    pub fn with_romanized(mut self, romanized: Option<String>) -> Self {
        self.romanized = romanized;
        self
    }
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
    #[error("HTTP error: {0}")]
    Http(u16),
    #[error("API error: {0}")]
    Api(String),
    #[error("Serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Returns the index of the lyric line active at `position` via binary search.
///
/// Returns `None` if:
/// - No lyrics
/// - Position is before the first line's timestamp
///
/// Assumes `position` is finite and line timestamps are sanitized (no NaN).
/// Shared by [`crate::state::LyricState::get_index`] (authoritative state) and
/// `crate::ui::progression` (UI-side position estimation).
pub fn active_line_index(lines: &[LyricLine], position: f64) -> Option<usize> {
    let first = lines.first()?;
    if position < first.time {
        return None;
    }

    match lines.binary_search_by(|line| line.time.total_cmp(&position)) {
        Ok(exact_match) => Some(exact_match),
        Err(0) => None,
        Err(insert_point) => Some(insert_point - 1),
    }
}

/// Returns `true` for HTTP statuses that are worth retrying later
/// (rate limiting and server-side failures).
pub fn is_retryable_status(code: u16) -> bool {
    code == 429 || code >= 500
}

// Re-export HTTP client for providers within the lyrics module
pub(crate) fn http_client() -> &'static Client {
    &HTTP_CLIENT
}
