//! lrc.red TTML lyrics provider (`LRCRedProvider.java`).

use crate::lyrics::parsers::parse_ttml;
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, ProviderContext,
    score_lyrics_candidate,
};
use crate::lyrics::types::{Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

const API_BASE: &str = "https://lrc.red";
const MODEL_CREDIT: &str = "Synced by lrc.red";
const ALBUM_PREFIX: &str = "Album: ";
const ARTIST_PREFIX: &str = "Artist: ";

const MODIFIER_KEYWORDS: &[&str] = &[
    "slowed",
    "reverb",
    "remix",
    "mixed",
    "dj mix",
    "mashup",
    "cover",
    "tribute",
    "karaoke",
    "instrumental",
    "acoustic",
    "sped up",
    "speed up",
    "live",
    "edit",
    "versión",
    "version",
    "bpm",
    "guitar",
    "piano",
    "lofi",
    "lo-fi",
    "nightcore",
    "club mix",
    "extended mix",
    "radio edit",
];

static BRACKETED_TAGS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[(\[](?:feat\.?|ft\.?|with|prod\.?|bonus track|remaster(?:ed)?)[^)\]]*[)\]]")
        .unwrap()
});
static TRAILING_FEAT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:feat\.?|ft\.?|with)\s+.*$").unwrap());
static VIDEO_TAGS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[(\[](?:official(?:\s+(?:music\s+)?video|\s+audio)?|music video|video|audio|lyrics?|visualizer|album version|explicit)[^)\]]*[)\]]")
        .unwrap()
});
static SONGWRITER_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<songwriter>([^<]*)</songwriter>").unwrap());
static NON_ALNUM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}\s]").unwrap());
static MULTI_SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static ARTIST_SPLIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[,&/]|\b(?:feat\.?|ft\.?|with|x)\b").unwrap());

pub struct LRCRedProvider;

#[derive(Clone, Debug)]
struct SearchHit {
    isrc: String,
    title: String,
    artist: String,
    album: String,
    duration_secs: f64,
}

fn normalize(s: &str) -> String {
    let lower = s.to_lowercase();
    let cleaned = NON_ALNUM_RE.replace_all(&lower, " ");
    MULTI_SPACE_RE.replace_all(cleaned.trim(), " ").into_owned()
}

fn clean_base_title(s: &str) -> String {
    let s1 = BRACKETED_TAGS_RE.replace_all(s, "");
    let s2 = TRAILING_FEAT_RE.replace_all(&s1, "");
    let s3 = VIDEO_TAGS_RE.replace_all(&s2, "");
    normalize(&s3)
}

fn extract_artists(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    ARTIST_SPLIT_RE
        .split(s)
        .map(normalize)
        .filter(|p| !p.is_empty())
        .collect()
}

fn score_hit(hit: &SearchHit, track: &TrackInfo) -> i32 {
    let target_dur = track.duration_secs;
    let item_dur = hit.duration_secs.round() as i64;

    let target_full = normalize(&track.title);
    let target_base = clean_base_title(&track.title);
    let target_artist = normalize(&track.artist);
    let target_album = normalize(&track.album);

    let item_full = normalize(&hit.title);
    let item_base = clean_base_title(&hit.title);
    let item_artist = normalize(&hit.artist);
    let item_album = normalize(&hit.album);

    let mut score = 0_i32;
    let mut title_exact = false;

    if !target_full.is_empty() && target_full == item_full {
        score += 50;
        title_exact = true;
    } else if !target_base.is_empty() && !item_base.is_empty() {
        if target_base == item_base {
            score += 35;
            title_exact = true;
        } else if item_base.contains(&target_base) || target_base.contains(&item_base) {
            score += 15;
        } else {
            score -= 40;
        }
    }

    if !target_artist.is_empty() && !item_artist.is_empty() {
        if target_artist == item_artist {
            score += 30;
        } else if item_artist.contains(&target_artist) || target_artist.contains(&item_artist) {
            score += 15;
        } else {
            let ta_list = extract_artists(&track.artist);
            let ia_list = extract_artists(&hit.artist);
            let overlap = ta_list.iter().any(|ta| {
                ia_list
                    .iter()
                    .any(|ia| ia == ta || ia.contains(ta.as_str()) || ta.contains(ia.as_str()))
            });
            if overlap {
                score += 10;
            } else {
                score -= if title_exact { 25 } else { 50 };
            }
        }
    }

    if !target_album.is_empty() && !item_album.is_empty() {
        if target_album == item_album {
            score += 35;
        } else if item_album.contains(&target_album) || target_album.contains(&item_album) {
            score += 20;
        }
        let target_single = target_album.contains("single");
        let item_single = item_album.contains("single");
        if !target_single && item_single {
            score -= 15;
        } else if target_single && item_single {
            score += 10;
        }
    }

    if target_dur > 0 && item_dur > 0 {
        let diff = (item_dur - target_dur).abs();
        if diff == 0 {
            score += 25;
        } else if diff == 1 {
            score += 20;
        } else if diff == 2 {
            score += 15;
        } else if diff <= 5 {
            score += 5;
        } else if diff > 15 {
            score -= 30;
        }
    }

    let track_lower = format!("{} {}", track.title, track.album).to_lowercase();
    let item_lower = format!("{} {}", hit.title, hit.album).to_lowercase();
    for kw in MODIFIER_KEYWORDS {
        if item_lower.contains(kw) && !track_lower.contains(kw) {
            score -= 30;
        }
    }
    score
}

fn build_url(
    path: &str,
    title_key: &str,
    title: &str,
    artist: &str,
    album: &str,
    dur: i64,
) -> String {
    let mut url = format!(
        "{API_BASE}{path}?{title_key}={}",
        urlencoding::encode(title)
    );
    if !artist.is_empty() {
        url.push_str("&artist=");
        url.push_str(&urlencoding::encode(artist));
    }
    if !album.is_empty() {
        url.push_str("&album=");
        url.push_str(&urlencoding::encode(album));
    }
    if dur > 0 {
        url.push_str(&format!("&duration={dur}"));
    }
    url
}

async fn read_json(ctx: &ProviderContext, url: &str) -> Option<Value> {
    let resp = ctx.client().get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

fn score_hits(arr: Option<&Vec<Value>>, track: &TrackInfo) -> Vec<SearchHit> {
    let Some(arr) = arr else {
        return Vec::new();
    };
    let mut scored: Vec<(i32, SearchHit)> = Vec::new();
    for item in arr {
        let isrc = item
            .get("isrc")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if isrc.is_empty() {
            continue;
        }
        let hit = SearchHit {
            isrc: isrc.to_string(),
            title: item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            artist: item
                .get("artist")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            album: item
                .get("album")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            duration_secs: item.get("duration").and_then(Value::as_f64).unwrap_or(0.0),
        };
        let sc = score_hit(&hit, track);
        if sc > 0 {
            scored.push((sc, hit));
        }
    }
    scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    scored.into_iter().map(|(_, h)| h).collect()
}

async fn query_candidates(ctx: &ProviderContext, track: &TrackInfo) -> Vec<SearchHit> {
    if track.title.is_empty() {
        return Vec::new();
    }

    let url = build_url(
        "/match.json",
        "track",
        &track.title,
        &track.artist,
        &track.album,
        track.duration_secs,
    );
    if let Some(json) = read_json(ctx, &url).await {
        let hits = score_hits(json.get("hits").and_then(Value::as_array), track);
        if !hits.is_empty() {
            return hits;
        }
    }

    let clean_title = clean_base_title(&track.title);
    let has_clean = !clean_title.is_empty() && clean_title != normalize(&track.title);
    if has_clean {
        let url = build_url(
            "/match.json",
            "track",
            &clean_title,
            &track.artist,
            &track.album,
            track.duration_secs,
        );
        if let Some(json) = read_json(ctx, &url).await {
            let hits = score_hits(json.get("hits").and_then(Value::as_array), track);
            if !hits.is_empty() {
                return hits;
            }
        }
    }

    if !track.artist.is_empty() {
        let url = build_url(
            "/match.json",
            "track",
            &track.title,
            "",
            &track.album,
            track.duration_secs,
        );
        if let Some(json) = read_json(ctx, &url).await {
            let hits = score_hits(json.get("hits").and_then(Value::as_array), track);
            if !hits.is_empty() {
                return hits;
            }
        }
    }

    let search_url = build_url(
        "/search.json",
        "q",
        &track.title,
        &track.artist,
        &track.album,
        track.duration_secs,
    );
    if let Some(json) = read_json(ctx, &search_url).await {
        let hits = score_hits(json.get("hits").and_then(Value::as_array), track);
        if !hits.is_empty() {
            return hits;
        }
    }

    if has_clean {
        let url = build_url(
            "/search.json",
            "q",
            &clean_title,
            &track.artist,
            &track.album,
            track.duration_secs,
        );
        if let Some(json) = read_json(ctx, &url).await {
            return score_hits(json.get("hits").and_then(Value::as_array), track);
        }
    }

    Vec::new()
}

fn read_by_model(json: &Value) -> bool {
    match json.get("byModel") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn parse_song(isrc: &str, json: &Value) -> Option<Lyrics> {
    let ttml = json.get("source").and_then(Value::as_str)?.trim();
    if ttml.is_empty() {
        return None;
    }
    let mut lyrics = parse_ttml(ttml, "LrcRed")?;
    if lyrics.is_empty() {
        return None;
    }
    lyrics.source_url = Some(format!("{API_BASE}/s/{isrc}"));
    if read_by_model(json) {
        lyrics.about = Some(MODEL_CREDIT.to_string());
    }

    if lyrics.songwriters.is_empty() {
        lyrics.songwriters = SONGWRITER_TAG_RE
            .captures_iter(ttml)
            .filter_map(|c| {
                let s = c[1].trim();
                (!s.is_empty()).then(|| s.to_string())
            })
            .collect();
    }

    lyrics
        .credits
        .retain(|c| !c.starts_with(ALBUM_PREFIX) && !c.starts_with(ARTIST_PREFIX));
    if let Some(alb) = json.get("album").and_then(Value::as_str).map(str::trim)
        && !alb.is_empty()
    {
        lyrics.credits.push(format!("{ALBUM_PREFIX}{alb}"));
    }
    if let Some(art) = json.get("artist").and_then(Value::as_str).map(str::trim)
        && !art.is_empty()
    {
        lyrics.credits.push(format!("{ARTIST_PREFIX}{art}"));
    }
    Some(lyrics)
}

impl LyricsProvider for LRCRedProvider {
    fn id(&self) -> &'static str {
        "lrcred"
    }

    fn display_name(&self) -> &'static str {
        "LrcRed"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            if let Some(local_isrc) = ids
                .track_isrcs
                .first()
                .map(String::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let url = format!("{API_BASE}/s/{local_isrc}.song.json");
                if let Some(json) = read_json(ctx, &url).await
                    && let Some(lyrics) = parse_song(local_isrc, &json)
                {
                    let score = crate::lyrics::providers::VIDEO_ID_TRUST + lyrics.sync_rank();
                    return Ok(vec![FetchedCandidate {
                        lyrics,
                        match_score: score,
                        is_blind: false,
                    }]);
                }
            }

            let hits = query_candidates(ctx, track).await;
            if hits.is_empty() {
                return Ok(Vec::new());
            }

            let mut out = Vec::new();
            for hit in hits {
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
                let url = format!("{API_BASE}/s/{}.song.json", hit.isrc);
                let Some(json) = read_json(ctx, &url).await else {
                    continue;
                };
                let Some(lyrics) = parse_song(&hit.isrc, &json) else {
                    continue;
                };
                let score = score_lyrics_candidate(
                    &hit.title,
                    &hit.artist,
                    hit.duration_secs.round() as i64,
                    &lyrics,
                    track,
                    ctx.custom_regex.as_ref(),
                );
                out.push(FetchedCandidate {
                    lyrics,
                    match_score: score.max(0),
                    is_blind: false,
                });
            }
            out.sort_by_key(|a| std::cmp::Reverse(a.match_score));
            Ok(out)
        })
    }
}
