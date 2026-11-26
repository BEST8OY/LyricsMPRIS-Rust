use reqwest::Client;
use std::collections::HashMap;
use std::sync::LazyLock;
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

// Shared HTTP client with reasonable defaults for timeouts and gzip decompression
static HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .user_agent("LyricsMPRIS/3.0")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client")
});

/// Canonical track query input for lyrics providers and scoring.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_secs: i64,
}

impl TrackInfo {
    pub fn new(
        title: impl Into<String>,
        artist: impl Into<String>,
        album: impl Into<String>,
        duration_secs: i64,
    ) -> Self {
        Self {
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            duration_secs,
        }
    }

    pub fn from_metadata(meta: &crate::mpris::TrackMetadata) -> Self {
        Self {
            title: meta.title.clone(),
            artist: meta.artist.clone(),
            album: meta.album.clone(),
            duration_secs: meta.length.map(|l| l.round() as i64).unwrap_or(0),
        }
    }
}

/// Track identifiers extracted from or provided for a lyrics lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMatchInfo {
    pub track_isrcs: Vec<String>,
    pub track_spotify_ids: Vec<String>,
    pub track_itunes_ids: Vec<String>,
    pub youtube_video_id: Option<String>,
}

/// Provider result: parsed lines plus optional raw lyrics string (LRC format or JSON)
/// and optional track identifiers extracted from the provider response.
pub type ProviderResult = Result<(Vec<LyricLine>, Option<String>, TrackMatchInfo), LyricsError>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LyricLine {
    pub time: f64,
    /// Explicit end time (enables instrumental gap detection & overlap retiming).
    pub end: Option<f64>,
    pub text: String,
    /// Optional per-word timings (start, end, text) for karaoke rendering.
    pub words: Option<Vec<WordTiming>>,
    /// Singer/agent identifier (e.g. "v1", "v2", "v1000").
    pub agent_id: Option<String>,
    /// True when rendered as a right-aligned duet line.
    pub is_duet: bool,
    /// True when this line is a background/harmony vocal.
    pub is_bg: bool,
    /// Optional section label (e.g. "Verse 1", "Chorus").
    pub song_part: Option<String>,
}

impl LyricLine {
    pub fn new(time: f64, text: impl Into<String>) -> Self {
        Self {
            time,
            end: None,
            text: text.into(),
            words: None,
            agent_id: None,
            is_duet: false,
            is_bg: false,
            song_part: None,
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

    pub fn with_agent(mut self, agent_id: Option<String>, is_duet: bool) -> Self {
        self.agent_id = agent_id;
        self.is_duet = is_duet;
        self
    }

    pub fn with_bg(mut self, is_bg: bool) -> Self {
        self.is_bg = is_bg;
        self
    }

    pub fn with_song_part(mut self, song_part: Option<String>) -> Self {
        self.song_part = song_part;
        self
    }

    /// True if `agent_id` is a centered group/other label (`v1000` / `v2000`).
    pub fn is_centered_agent_id(agent_id: Option<&str>) -> bool {
        agent_id.is_some_and(|id| {
            let trimmed = id.trim();
            trimmed.eq_ignore_ascii_case("v1000") || trimmed.eq_ignore_ascii_case("v2000")
        })
    }

    #[allow(dead_code)]
    pub fn is_centered(&self) -> bool {
        Self::is_centered_agent_id(self.agent_id.as_deref())
    }

    /// Returns the effective sung end time (`max(last_word.end, self.end)`).
    pub fn sung_end(&self) -> Option<f64> {
        let word_end = self
            .words
            .as_ref()
            .and_then(|ws| ws.last())
            .map(|w| w.end)
            .filter(|e| e.is_finite() && *e > self.time);
        match (
            word_end,
            self.end.filter(|e| e.is_finite() && *e >= self.time),
        ) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Mirrors Morphe's `LyricsLine.hasWordTimings()`: rejects fake single-word spans
    /// or word arrays whose timings all span the entire line unchanged.
    pub fn has_word_timings(&self) -> bool {
        let Some(words) = self.words.as_ref() else {
            return false;
        };
        if words.is_empty() {
            return false;
        }
        if words.len() == 1 {
            let w = &words[0];
            if w.end <= w.start {
                return false;
            }
            if let Some(end) = self.end {
                let compact_word: String = w.text.chars().filter(|c| !c.is_whitespace()).collect();
                let compact_line: String =
                    self.text.chars().filter(|c| !c.is_whitespace()).collect();
                if compact_word == compact_line
                    && (w.start - self.time).abs() < 0.002
                    && (w.end - end).abs() < 0.002
                {
                    return false;
                }
            }
            return true;
        }
        if let Some(end) = self.end {
            let all_match_line = words.iter().all(|w| {
                w.end <= w.start
                    || ((w.start - self.time).abs() < 0.002 && (w.end - end).abs() < 0.002)
            });
            if all_match_line {
                return false;
            }
        }
        words.iter().any(|w| w.end > w.start)
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

#[derive(Debug, Clone, Default, PartialEq)]
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
    /// Preserves trailing space separation across word-synced tokens.
    pub ends_with_space: bool,
    /// Optional word-level romanization reading (e.g. from TTML sidecar / KRC / QRC).
    pub romaji: Option<String>,
    /// Optional ruby/furigana annotation (e.g. from TTML `tts:ruby` / HTML `<ruby>`).
    pub ruby: Option<String>,
}

impl WordTiming {
    /// Returns the number of grapheme clusters in this word.
    pub fn grapheme_count(&self) -> usize {
        self.grapheme_boundaries.len().saturating_sub(1)
    }

    pub fn with_space(mut self, ends_with_space: bool) -> Self {
        self.ends_with_space = ends_with_space;
        self
    }

    pub fn with_annotations(mut self, romaji: Option<String>, ruby: Option<String>) -> Self {
        self.romaji = romaji;
        self.ruby = ruby;
        self
    }
}

/// Constructs a `WordTiming` with precomputed UTF-8 grapheme cluster boundaries & transition times.
pub fn create_word_timing(start: f64, end: f64, text: &str) -> WordTiming {
    let mut grapheme_boundaries = Vec::new();
    grapheme_boundaries.push(0);
    for (byte_offset, _) in text.grapheme_indices(true) {
        if byte_offset > 0 {
            grapheme_boundaries.push(byte_offset);
        }
    }
    grapheme_boundaries.push(text.len());

    let total = grapheme_boundaries.len().saturating_sub(1);
    let duration = (end - start).max(f64::EPSILON);
    let grapheme_times = (1..total)
        .map(|k| start + (k as f64 / total as f64) * duration)
        .collect();

    WordTiming {
        start,
        end,
        text: text.to_string(),
        grapheme_boundaries,
        grapheme_times,
        ends_with_space: false,
        romaji: None,
        ruby: None,
    }
}

/// Full structured lyrics payload mirroring `morphe-patches`'s `Lyrics` record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    pub provider_name: String,
    pub synced: bool,
    pub translations: HashMap<String, Vec<LyricLine>>,
    pub romanizations: HashMap<String, Vec<LyricLine>>,
    pub songwriters: Vec<String>,
    pub credits: Vec<String>,
    pub raw_format: Option<String>,
    pub format_type: Option<String>,
    pub source_url: Option<String>,
    pub about: Option<String>,
    pub agent_names: HashMap<String, String>,
}

impl Lyrics {
    pub fn new(lines: Vec<LyricLine>, provider_name: impl Into<String>, synced: bool) -> Self {
        Self {
            lines,
            provider_name: provider_name.into(),
            synced,
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn has_word_sync(&self) -> bool {
        self.lines.iter().any(LyricLine::has_word_timings)
    }

    /// 2 = word-synced, 1 = line-synced, 0 = plain text (`LyricsRequests.syncRank`).
    pub fn sync_rank(&self) -> i32 {
        if self.has_word_sync() {
            2
        } else if self.synced {
            1
        } else {
            0
        }
    }

    /// 64-bit FNV-1a fingerprint over provider name + raw format or normalized line texts.
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let prime: u64 = 0x0000_0100_0000_01b3;
        for b in self.provider_name.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(prime);
        }
        h ^= b'|' as u64;
        h = h.wrapping_mul(prime);

        if let Some(raw) = self.raw_format.as_deref().filter(|s| !s.is_empty()) {
            for b in raw.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(prime);
            }
        } else {
            for line in &self.lines {
                for b in line.text.as_bytes() {
                    h ^= *b as u64;
                    h = h.wrapping_mul(prime);
                }
                h ^= b'\n' as u64;
                h = h.wrapping_mul(prime);
            }
        }
        h
    }

    /// Repairs zero/negative/equal/out-of-order word timings (`Lyrics.fixAnomalousWordTimestamps`).
    pub fn fix_anomalous_word_timings(&mut self) {
        for line in &mut self.lines {
            let Some(words) = line.words.as_ref() else {
                continue;
            };
            if words.is_empty() {
                continue;
            }
            let line_start = line.time;
            let has_anomalous = words.iter().any(|w| {
                let untimed = w.start < 0.0 || w.end < 0.0;
                let bad_start = w.start < 0.0
                    || (w.start > 0.0 && line_start > 0.0 && w.start < line_start - 0.5);
                untimed
                    || bad_start
                    || (w.start == 0.0 && line_start > 0.0)
                    || (w.end == 0.0 && w.start > 0.0)
                    || (w.start > 0.0 && (w.start - w.end).abs() < 0.0005)
            });
            if !has_anomalous {
                continue;
            }
            let unsalvageable =
                words.iter().any(|w| w.start < 0.0 || w.end < 0.0) && line_start < 0.0;
            if unsalvageable {
                line.words = None;
                continue;
            }

            let mut fixed = Vec::with_capacity(words.len());
            let mut prev_end = line_start.max(0.0);
            for (i, w) in words.iter().enumerate() {
                let bad_start = w.start < 0.0
                    || (w.start > 0.0 && line_start > 0.0 && w.start < line_start - 0.5);
                let new_start = if bad_start {
                    prev_end.max(line_start.max(0.0))
                } else {
                    prev_end.max(w.start)
                };

                let needs_end = w.end <= new_start;
                let new_end = if needs_end {
                    let next_valid = words[i + 1..]
                        .iter()
                        .find(|nw| nw.start > new_start && nw.end > nw.start)
                        .map(|nw| nw.start);
                    next_valid
                        .or_else(|| line.end.filter(|e| *e > new_start))
                        .unwrap_or(new_start + 0.8)
                        .max(new_start + 0.05)
                } else {
                    w.end.max(new_start + 0.05)
                };

                let mut rebuilt = create_word_timing(new_start, new_end, &w.text);
                rebuilt.ends_with_space = w.ends_with_space;
                rebuilt.romaji = w.romaji.clone();
                rebuilt.ruby = w.ruby.clone();
                prev_end = new_end;
                fixed.push(rebuilt);
            }
            line.words = Some(fixed);
        }
    }

    /// Clamps the last word end time of each word-timed line (`Lyrics.clampLastWordEnds`).
    pub fn clamp_last_word_ends(&mut self) {
        let len = self.lines.len();
        for i in 0..len {
            let next_non_bg_start = self.lines[i + 1..]
                .iter()
                .find(|l| !l.is_bg && l.time.is_finite() && l.time >= 0.0)
                .map(|l| l.time);

            let line_end = self.lines[i].end;
            let Some(words) = self.lines[i].words.as_mut() else {
                continue;
            };
            let Some(last) = words.last_mut() else {
                continue;
            };

            let mut eff_end = if last.end > last.start && last.end.is_finite() {
                last.end
            } else if let Some(le) = line_end.filter(|e| *e > last.start) {
                le
            } else {
                last.start + 0.8
            };

            if let Some(next_start) = next_non_bg_start {
                eff_end = eff_end.min(next_start);
            }
            if let Some(le) = line_end.filter(|e| *e > last.start) {
                eff_end = eff_end.min(le);
            }

            let floor = match next_non_bg_start {
                Some(ns) => (last.start + 0.2).min(ns),
                None => last.start + 0.2,
            };
            eff_end = eff_end.max(floor).max(last.start + 0.04);

            if (eff_end - last.end).abs() > 0.001 {
                let mut rebuilt = create_word_timing(last.start, eff_end, &last.text);
                rebuilt.ends_with_space = last.ends_with_space;
                rebuilt.romaji = last.romaji.clone();
                rebuilt.ruby = last.ruby.clone();
                *last = rebuilt;
            }
        }
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
/// Rewinds past `is_bg` lines so background vocals do not steal primary focus,
/// matching `Lyrics.indexForPosition`.
pub fn active_line_index(lines: &[LyricLine], position: f64) -> Option<usize> {
    let first = lines.first()?;
    if !position.is_finite() || position < first.time {
        return None;
    }

    let mut idx = match lines.binary_search_by(|line| line.time.total_cmp(&position)) {
        Ok(exact_match) => exact_match,
        Err(0) => return None,
        Err(insert_point) => insert_point - 1,
    };

    while lines[idx].is_bg {
        if idx == 0 {
            return None;
        }
        idx -= 1;
    }

    Some(idx)
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

        // Fast path: forward sequential progression when no BG lines complicate indexing
        if let Some(idx) = self.current_index
            && idx < lines.len()
            && !lines[idx].is_bg
        {
            let curr = &lines[idx];
            let next = lines.get(idx + 1);

            if position >= curr.time && next.is_none_or(|n| position < n.time) {
                return (Some(idx), false);
            }

            if let Some(next_line) = next
                && !next_line.is_bg
                && position >= next_line.time
            {
                let next_next = lines.get(idx + 2);
                if next_next.is_none_or(|nn| position < nn.time) {
                    self.current_index = Some(idx + 1);
                    return (Some(idx + 1), true);
                }
            }
        }

        let new_idx = active_line_index(lines, position);
        let changed = self.current_index != new_idx;
        self.current_index = new_idx;
        (new_idx, changed)
    }
}

/// Shifts line and word timings (including precomputed `grapheme_times`) by `offset_secs`.
pub fn shift_lines_timings(lines: &mut [LyricLine], offset_secs: f64) {
    if offset_secs == 0.0 || !offset_secs.is_finite() {
        return;
    }
    for line in lines {
        if line.time >= 0.0 {
            line.time = (line.time + offset_secs).max(0.0);
        }
        if let Some(end) = line.end {
            line.end = Some((end + offset_secs).max(0.0));
        }
        if let Some(words) = line.words.as_mut() {
            for w in words {
                w.start = (w.start + offset_secs).max(0.0);
                w.end = (w.end + offset_secs).max(0.0);
                for gt in &mut w.grapheme_times {
                    *gt = (*gt + offset_secs).max(0.0);
                }
            }
        }
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

        assert_eq!(active_line_index(&lines, 5.0), None);
        assert_eq!(active_line_index(&lines, 10.0), Some(0));
        assert_eq!(active_line_index(&lines, 12.5), Some(0));
        assert_eq!(active_line_index(&lines, 15.0), Some(0));
        assert!(lines[0].is_active_at(12.5));
        assert_eq!(active_line_index(&lines, 20.0), Some(0));
        assert!(!lines[0].is_active_at(20.0));
        assert_eq!(active_line_index(&lines, 25.0), Some(1));
        assert_eq!(active_line_index(&lines, 30.05), Some(1));
    }

    #[test]
    fn test_active_line_index_rewinds_past_bg() {
        let lines = vec![
            LyricLine::new(10.0, "Main 1"),
            LyricLine::new(12.0, "Echo").with_bg(true),
            LyricLine::new(20.0, "Main 2"),
        ];
        assert_eq!(active_line_index(&lines, 13.0), Some(0));
    }

    #[test]
    fn test_sync_tracker_o1_progression_and_seek_fallback() {
        let lines = vec![
            LyricLine::new(10.0, "Line 1").with_end(Some(15.0)),
            LyricLine::new(25.0, "Line 2").with_end(Some(30.0)),
        ];

        let mut tracker = SyncTracker::new();
        assert_eq!(tracker.update(&lines, 5.0), (None, false));
        assert_eq!(tracker.update(&lines, 10.0), (Some(0), true));
        assert_eq!(tracker.update(&lines, 12.0), (Some(0), false));
        assert_eq!(tracker.update(&lines, 16.0), (Some(0), false));
        assert_eq!(tracker.update(&lines, 25.0), (Some(1), true));
        assert_eq!(tracker.update(&lines, 11.0), (Some(0), true));
    }

    #[test]
    fn test_has_word_timings_rejects_dummy_single_word() {
        let dummy = LyricLine::new(1.0, "Hello")
            .with_end(Some(5.0))
            .with_words(Some(vec![create_word_timing(1.0, 5.0, "Hello")]));
        assert!(!dummy.has_word_timings());

        let real = LyricLine::new(1.0, "Hello world")
            .with_end(Some(5.0))
            .with_words(Some(vec![
                create_word_timing(1.0, 2.5, "Hello"),
                create_word_timing(2.5, 5.0, "world"),
            ]));
        assert!(real.has_word_timings());
    }

    #[test]
    fn test_shift_lines_timings_updates_grapheme_times() {
        let mut lines = vec![
            LyricLine::new(1.0, "ab")
                .with_end(Some(3.0))
                .with_words(Some(vec![create_word_timing(1.0, 3.0, "ab")])),
        ];
        shift_lines_timings(&mut lines, 2.5);
        assert!((lines[0].time - 3.5).abs() < 1e-6);
        assert!((lines[0].end.unwrap() - 5.5).abs() < 1e-6);
        let w = &lines[0].words.as_ref().unwrap()[0];
        assert!((w.start - 3.5).abs() < 1e-6);
        assert!((w.end - 5.5).abs() < 1e-6);
        assert_eq!(w.grapheme_times.len(), 1);
        assert!((w.grapheme_times[0] - 4.5).abs() < 1e-6);
    }
}
