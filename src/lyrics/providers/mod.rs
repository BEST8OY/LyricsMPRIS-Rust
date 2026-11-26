//! Multi-provider lyrics fetching trait, 0-11 scoring engine, and provider registry.

pub mod amll;
pub mod apple;
pub mod blyrics;
pub mod captions;
pub mod deezer;
pub mod genius;
pub mod kugou;
pub mod kuwo;
pub mod lrclib;
pub mod lrcred;
pub mod luna;
pub mod lunabeat;
pub mod lyricify;
pub mod lyricsplus;
pub mod musixmatch;
pub mod netease;
pub mod petitlyrics;
pub mod qq;
pub mod rmm;
pub mod simpmusic;
pub mod spotify;
pub mod unison;
pub mod ytmusic;

use crate::config::RuntimeSettings;
use crate::lyrics::types::{Lyrics, LyricsError, TrackInfo, TrackMatchInfo, http_client};
use crate::text_utils::normalize_for_match;
use regex::Regex;
use reqwest::Client;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const CHROME_DESKTOP_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36";
pub const ITUNES_SEARCH_URL: &str = "https://itunes.apple.com/search";

pub const SOFT_MIN: i32 = 2;
pub const VIDEO_ID_TRUST: i32 = 10;
pub const NEUTRAL: i32 = 6;
pub const MAX_CANDIDATES: usize = 5;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runtime context passed into every provider invocation.
pub struct ProviderContext {
    pub settings: RuntimeSettings,
    pub custom_regex: Option<Regex>,
}

impl ProviderContext {
    pub fn from_settings(settings: RuntimeSettings) -> Self {
        let custom_regex = settings
            .filters
            .custom_metadata_regex
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .and_then(|s| Regex::new(s).ok());
        Self {
            settings,
            custom_regex,
        }
    }

    pub fn client(&self) -> &'static Client {
        http_client()
    }
}

/// Candidate returned by a provider with its metadata match score.
#[derive(Debug, Clone)]
pub struct FetchedCandidate {
    pub lyrics: Lyrics,
    pub match_score: i32,
    pub is_blind: bool,
}

impl FetchedCandidate {
    pub fn with_metadata(
        lyrics: Lyrics,
        cand_title: &str,
        cand_artist: &str,
        cand_dur_secs: i64,
        cand_album: Option<&str>,
        track: &TrackInfo,
        ctx: &ProviderContext,
    ) -> Option<Self> {
        if lyrics.is_empty() {
            return None;
        }
        let prepared = PreparedTrack::new(track, ctx.custom_regex.as_ref());
        let verdict = prepared.evaluate(cand_title, cand_artist, cand_dur_secs, cand_album);
        if !verdict.is_high_match() {
            return None;
        }
        Some(Self {
            lyrics,
            match_score: verdict.score,
            is_blind: false,
        })
    }

    pub fn blind(lyrics: Lyrics) -> Option<Self> {
        if lyrics.is_empty() {
            return None;
        }
        Some(Self {
            lyrics,
            match_score: NEUTRAL,
            is_blind: true,
        })
    }

    pub fn video_id(lyrics: Lyrics) -> Option<Self> {
        if lyrics.is_empty() {
            return None;
        }
        Some(Self {
            lyrics,
            match_score: VIDEO_ID_TRUST,
            is_blind: false,
        })
    }
}

pub trait LyricsProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn is_blind(&self) -> bool {
        is_blind_provider(self.id())
    }
    fn requires_video_id(&self) -> bool {
        matches!(self.id(), "ytmusic" | "captions")
    }
    fn is_configured(&self, _ctx: &ProviderContext) -> bool {
        true
    }
    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>>;
}

pub fn is_blind_provider(id: &str) -> bool {
    matches!(
        id,
        "spotify" | "amll" | "blyrics" | "lyricsplus" | "lyricify"
    )
}

pub fn build_provider(id: &str) -> Option<Box<dyn LyricsProvider>> {
    match id {
        "apple" => Some(Box::new(apple::AppleMusicProvider)),
        "lyricsplus" => Some(Box::new(lyricsplus::LyricsPlusProvider)),
        "lrcred" => Some(Box::new(lrcred::LRCRedProvider)),
        "blyrics" => Some(Box::new(blyrics::BlyricsProvider)),
        "lyricify" => Some(Box::new(lyricify::LyricifyProvider)),
        "unison" => Some(Box::new(unison::UnisonProvider)),
        "petitlyrics" => Some(Box::new(petitlyrics::PetitLyricsProvider)),
        "amll" => Some(Box::new(amll::AMLLProvider)),
        "qq" => Some(Box::new(qq::QQProvider)),
        "simpmusic" => Some(Box::new(simpmusic::SimpMusicProvider)),
        "kugou" => Some(Box::new(kugou::KuGouProvider)),
        "musixmatch" => Some(Box::new(musixmatch::MusixmatchProvider)),
        "ytmusic" => Some(Box::new(ytmusic::YouTubeMusicProvider)),
        "lrclib" => Some(Box::new(lrclib::LrcLibProvider)),
        "netease" => Some(Box::new(netease::NetEaseProvider)),
        "luna" => Some(Box::new(luna::LunaProvider)),
        "kuwo" => Some(Box::new(kuwo::KuwoProvider)),
        "lunabeat" => Some(Box::new(lunabeat::LunaBeatProvider)),
        "captions" => Some(Box::new(captions::CaptionsProvider)),
        "spotify" => Some(Box::new(spotify::SpotifyProvider)),
        "deezer" => Some(Box::new(deezer::DeezerProvider)),
        "rmm" => Some(Box::new(rmm::RMMProvider)),
        "genius" => Some(Box::new(genius::GeniusProvider)),
        _ => None,
    }
}

/// Async non-blocking throttle gate using atomic slot reservation so concurrent callers serialize cleanly.
pub async fn throttle_ms(last_ms: &AtomicU64, min_interval_ms: u64) {
    let now = now_ms();
    let mut prev_slot = 0_u64;
    let _ = last_ms.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |prev| {
        prev_slot = prev;
        let next = if prev == 0 || now >= prev.saturating_add(min_interval_ms) {
            now
        } else {
            prev.saturating_add(min_interval_ms)
        };
        Some(next)
    });
    if prev_slot > 0 {
        let target = prev_slot.saturating_add(min_interval_ms);
        if target > now {
            tokio::time::sleep(Duration::from_millis(target - now)).await;
        }
    }
}

#[inline]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Resolves an HTTP(S) endpoint override via `config::validate_http_base_url`, falling back to `default_url`.
pub fn endpoint_url(override_url: Option<&str>, default_url: &str) -> String {
    crate::config::validate_http_base_url(override_url)
        .unwrap_or_else(|| default_url.trim_end_matches('/').to_string())
}

// ============================================================================
// 0-11 Scoring Engine (`LyricsRequests.PreparedTrack`)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    Equal,
    Match,
    Partial,
    Missing,
    Mismatch,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchVerdict {
    pub score: i32,
    pub title: Evidence,
    pub artist: Evidence,
    pub duration: Evidence,
    pub album: Evidence,
}

impl MatchVerdict {
    pub fn is_high_match(&self) -> bool {
        if matches!(self.title, Evidence::None | Evidence::Missing) {
            return false;
        }
        if self.artist == Evidence::Mismatch {
            return false;
        }
        if self.score >= 7 {
            return self.duration != Evidence::Mismatch || self.artist == Evidence::Match;
        }
        self.title == Evidence::Equal
            && self.duration != Evidence::Mismatch
            && self.album != Evidence::Mismatch
    }
}

pub struct PreparedTrack {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_sec: i64,
    pub title_valid: bool,
    custom_regex: Option<Regex>,
}

impl PreparedTrack {
    pub fn new(track: &TrackInfo, custom_regex: Option<&Regex>) -> Self {
        let title = normalize_for_match(&track.title, custom_regex);
        let artist = normalize_for_match(&track.artist, custom_regex);
        let album = normalize_for_match(&track.album, custom_regex);
        Self {
            title_valid: !title.is_empty(),
            title,
            artist,
            album,
            duration_sec: track.duration_secs,
            custom_regex: custom_regex.cloned(),
        }
    }

    pub fn evaluate(
        &self,
        title: &str,
        artist: &str,
        duration_sec: i64,
        album: Option<&str>,
    ) -> MatchVerdict {
        let re = self.custom_regex.as_ref();
        let t = normalize_for_match(title, re);
        let a = normalize_for_match(artist, re);
        let al = album
            .map(|s| normalize_for_match(s, re))
            .unwrap_or_default();

        let (title_ev, title_score) = if !self.title_valid {
            (Evidence::Missing, 0)
        } else if t.is_empty() {
            (Evidence::None, 0)
        } else if t == self.title {
            (Evidence::Equal, 5)
        } else if contains_either(&t, &self.title) {
            (Evidence::Match, 3)
        } else {
            let j = jaccard(&t, &self.title);
            if j >= 0.6 {
                (Evidence::Partial, 2)
            } else if j >= 0.35 {
                (Evidence::Partial, 1)
            } else {
                (Evidence::None, 0)
            }
        };

        let (artist_ev, artist_score) = if a.is_empty() || self.artist.is_empty() {
            (Evidence::Missing, 0)
        } else if a == self.artist || contains_either(&a, &self.artist) {
            (Evidence::Match, 3)
        } else if split_artist_hits(&a, &self.artist) {
            (Evidence::Partial, 1)
        } else {
            (Evidence::Mismatch, 0)
        };

        let (duration_ev, duration_score) = if duration_sec <= 0 || self.duration_sec <= 0 {
            (Evidence::Missing, 0)
        } else {
            let delta = (duration_sec - self.duration_sec).abs();
            if delta > 15 {
                (Evidence::Mismatch, 0)
            } else if delta <= 2 {
                (Evidence::Match, 2)
            } else if delta <= 5 {
                (Evidence::Partial, 1)
            } else {
                (Evidence::Partial, 0)
            }
        };

        let (album_ev, album_score) = if al.is_empty() || self.album.is_empty() {
            (Evidence::Missing, 0)
        } else if al == self.album || contains_either(&al, &self.album) {
            (Evidence::Match, 1)
        } else {
            (Evidence::Mismatch, 0)
        };

        MatchVerdict {
            score: title_score + artist_score + duration_score + album_score,
            title: title_ev,
            artist: artist_ev,
            duration: duration_ev,
            album: album_ev,
        }
    }
}

pub fn score_track_candidate(
    title: &str,
    artist: &str,
    duration_sec: i64,
    album: Option<&str>,
    track: &TrackInfo,
    custom_regex: Option<&Regex>,
) -> i32 {
    let verdict =
        PreparedTrack::new(track, custom_regex).evaluate(title, artist, duration_sec, album);
    if verdict.is_high_match() {
        verdict.score
    } else {
        -1
    }
}

pub fn score_lyrics_candidate(
    title: &str,
    artist: &str,
    duration_sec: i64,
    lyrics: &Lyrics,
    track: &TrackInfo,
    custom_regex: Option<&Regex>,
) -> i32 {
    let s = score_track_candidate(title, artist, duration_sec, None, track, custom_regex);
    if s < 0 { s } else { s + lyrics.sync_rank() }
}

pub fn composite_score(match_score: i32, lyrics: &Lyrics, variant_penalty: i32) -> i32 {
    match_score * 10 + lyrics.sync_rank() - variant_penalty
}

#[inline]
fn is_cjk_han(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF)
}

pub fn contains_either(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.contains(b) || b.contains(a) {
        let (shorter, longer) = if a.chars().count() <= b.chars().count() {
            (a, b)
        } else {
            (b, a)
        };
        let s_len = shorter.chars().count();
        let l_len = longer.chars().count();
        let first_cjk = shorter.chars().next().is_some_and(is_cjk_han);
        if s_len < 4 && !first_cjk {
            return l_len <= s_len + 6;
        }
        if s_len < 2 && first_cjk {
            return l_len <= s_len + 4;
        }
        return true;
    }
    false
}

fn split_artist_hits(cand_artist: &str, query_artist: &str) -> bool {
    for raw in cand_artist.split(['&', ',', '/']) {
        for sub in raw.split("feat") {
            let p = sub
                .trim()
                .trim_start_matches('.')
                .trim()
                .trim_matches('×')
                .trim_matches('和')
                .trim();
            if p.is_empty() {
                continue;
            }
            if p == query_artist {
                return true;
            }
            if (p.chars().count() >= 2 || query_artist.chars().count() < 4)
                && contains_either(p, query_artist)
            {
                return true;
            }
        }
    }
    false
}

fn tokens(s: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.insert(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.insert(cur);
    }
    out
}

fn jaccard(a: &str, b: &str) -> f64 {
    let ta = tokens(a);
    let tb = tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.iter().filter(|t| tb.contains(*t)).count();
    let union = ta.len() + tb.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepared_track_scoring() {
        let track = TrackInfo::new("Get Lucky", "Daft Punk", "Random Access Memories", 248);
        let p = PreparedTrack::new(&track, None);
        let exact = p.evaluate(
            "Get Lucky",
            "Daft Punk",
            248,
            Some("Random Access Memories"),
        );
        assert_eq!(exact.score, 11);
        assert!(exact.is_high_match());

        let wrong_artist = p.evaluate("Get Lucky", "Completely Different", 248, None);
        assert_eq!(wrong_artist.artist, Evidence::Mismatch);
        assert!(!wrong_artist.is_high_match());

        let wrong_dur = p.evaluate("Get Lucky", "Daft Punk", 320, None);
        assert_eq!(wrong_dur.duration, Evidence::Mismatch);
        assert!(wrong_dur.is_high_match()); // score 8 >= 7 && artist == Match
    }
}
