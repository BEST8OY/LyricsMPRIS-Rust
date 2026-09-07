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
    /// Explicit end time (enables instrumental gap detection). None for legacy LRC.
    pub end: Option<f64>,
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
            end: None,
            text: text.into(),
            words: None,
            romanized: None,
        }
    }

    pub fn with_end(mut self, end: Option<f64>) -> Self {
        self.end = end;
        self
    }

    pub fn with_words(mut self, words: Option<Vec<WordTiming>>) -> Self {
        self.words = words;
        self
    }

    #[cfg(test)]
    pub fn with_romanized(mut self, romanized: Option<String>) -> Self {
        self.romanized = romanized;
        self
    }

    /// Returns true if this line is active at the given playback position.
    #[inline]
    #[cfg(test)]
    pub fn is_active_at(&self, position: f64) -> bool {
        if position < self.time {
            return false;
        }
        if let Some(end) = self.end {
            position <= end
        } else {
            true
        }
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
/// A line remains active from its start timestamp until the next line begins
/// (or indefinitely for the last line).
/// Shared by [`crate::state::LyricState::get_index`] (authoritative state) and
/// `crate::ui::progression` (UI-side position estimation).
pub fn active_line_index(lines: &[LyricLine], position: f64) -> Option<usize> {
    let first = lines.first()?;
    if !position.is_finite() || position < first.time {
        return None;
    }

    match lines.binary_search_by(|line| line.time.total_cmp(&position)) {
        Ok(exact_match) => Some(exact_match),
        Err(0) => None,
        Err(insert_point) => Some(insert_point - 1),
    }
}

/// O(1) linear forward progression tracker for active lyric lines with binary search fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncTracker {
    current_index: Option<usize>,
}

impl SyncTracker {
    pub fn new() -> Self {
        Self {
            current_index: None,
        }
    }

    #[inline]
    pub fn reset(&mut self) {
        self.current_index = None;
    }

    /// Updates tracker state for the given playback position.
    /// Returns `(current_index, has_changed)`.
    pub fn update(&mut self, lines: &[LyricLine], position: f64) -> (Option<usize>, bool) {
        if lines.is_empty() || !position.is_finite() || position < lines[0].time {
            let changed = self.current_index.is_some();
            self.current_index = None;
            return (None, changed);
        }

        // Fast path: forward sequential progression from current active line
        if let Some(idx) = self.current_index
            && idx < lines.len()
        {
            let curr = &lines[idx];
            let next = lines.get(idx + 1);

            // Case 1: Still on the current line (curr.time <= position < next.time)
            if position >= curr.time && next.is_none_or(|n| position < n.time) {
                return (Some(idx), false);
            }

            // Case 2: Advanced directly to the next line
            if let Some(next_line) = next
                && position >= next_line.time
            {
                let next_next = lines.get(idx + 2);
                if next_next.is_none_or(|nn| position < nn.time) {
                    self.current_index = Some(idx + 1);
                    return (Some(idx + 1), true);
                }
            }
        }

        // Fallback: full binary search for seeks, skips, rewinds, or jumping multiple lines
        let new_idx = active_line_index(lines, position);
        let changed = self.current_index != new_idx;
        self.current_index = new_idx;
        (new_idx, changed)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_active_line_index_with_explicit_end_instrumental_gap() {
        let lines = vec![
            LyricLine::new(10.0, "Line 1").with_end(Some(15.0)),
            LyricLine::new(25.0, "Line 2").with_end(Some(30.0)),
        ];

        // Before first line
        assert_eq!(active_line_index(&lines, 5.0), None);
        // During Line 1
        assert_eq!(active_line_index(&lines, 10.0), Some(0));
        assert_eq!(active_line_index(&lines, 12.5), Some(0));
        assert_eq!(active_line_index(&lines, 15.0), Some(0));
        assert!(lines[0].is_active_at(12.5));

        // In instrumental gap between 15.0s and 25.0s:
        // The active line remains Line 1 until Line 2 begins, but vocal is inactive
        assert_eq!(active_line_index(&lines, 15.01), Some(0));
        assert_eq!(active_line_index(&lines, 20.0), Some(0));
        assert_eq!(active_line_index(&lines, 24.99), Some(0));
        assert!(!lines[0].is_active_at(20.0));

        // During Line 2
        assert_eq!(active_line_index(&lines, 25.0), Some(1));
        assert_eq!(active_line_index(&lines, 28.0), Some(1));
        assert_eq!(active_line_index(&lines, 30.0), Some(1));
        // After Line 2 ends (remains on last line until track end/change)
        assert_eq!(active_line_index(&lines, 30.05), Some(1));
    }

    #[test]
    fn test_active_line_index_legacy_without_end() {
        let lines = vec![
            LyricLine::new(10.0, "Line 1"),
            LyricLine::new(25.0, "Line 2"),
        ];

        // Legacy behavior: line remains active until next line starts
        assert_eq!(active_line_index(&lines, 12.0), Some(0));
        assert_eq!(active_line_index(&lines, 20.0), Some(0));
        assert_eq!(active_line_index(&lines, 25.0), Some(1));
        assert_eq!(active_line_index(&lines, 100.0), Some(1));
    }

    #[test]
    fn test_sync_tracker_o1_progression_and_seek_fallback() {
        let lines = vec![
            LyricLine::new(10.0, "Line 1").with_end(Some(15.0)),
            LyricLine::new(25.0, "Line 2").with_end(Some(30.0)),
        ];

        let mut tracker = SyncTracker::new();

        // Step 1: Initial position before first line
        let (idx, changed) = tracker.update(&lines, 5.0);
        assert_eq!(idx, None);
        assert!(!changed);

        // Step 2: Line 1 starts
        let (idx, changed) = tracker.update(&lines, 10.0);
        assert_eq!(idx, Some(0));
        assert!(changed);

        // Step 3: Line 1 continues (O(1) fast path)
        let (idx, changed) = tracker.update(&lines, 12.0);
        assert_eq!(idx, Some(0));
        assert!(!changed);

        // Step 4: In instrumental gap (O(1) fast path: stays on Line 1)
        let (idx, changed) = tracker.update(&lines, 16.0);
        assert_eq!(idx, Some(0));
        assert!(!changed);

        // Step 5: Gap continues
        let (idx, changed) = tracker.update(&lines, 20.0);
        assert_eq!(idx, Some(0));
        assert!(!changed);

        // Step 6: Line 2 starts
        let (idx, changed) = tracker.update(&lines, 25.0);
        assert_eq!(idx, Some(1));
        assert!(changed);

        // Step 7: Backward seek jump to 11.0s (fallback to binary search)
        let (idx, changed) = tracker.update(&lines, 11.0);
        assert_eq!(idx, Some(0));
        assert!(changed);
    }
}
