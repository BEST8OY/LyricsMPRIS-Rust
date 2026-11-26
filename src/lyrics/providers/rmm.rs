//! RMM Revival (`lyrics.rmmreviv.al` via iTunes Search) provider (`RMMProvider.java`).

use crate::lyrics::parsers::{parse_plain, parse_synced, parse_ttml};
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, ITUNES_SEARCH_URL, LyricsProvider, MAX_CANDIDATES,
    ProviderContext, SOFT_MIN, score_lyrics_candidate, score_track_candidate,
};
use crate::lyrics::types::{LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use serde_json::Value;
use std::time::Duration;

const RMM_LYRICS: &str = "https://lyrics.rmmreviv.al/lyrics?id=";
const DISPLAY_NAME: &str = "RMM Revival";
const DISPLAY_NAME_VIA: &str = "RMM Revival (via SpicyLyrics)";

pub struct RMMProvider;

fn usable_url(url: Option<&str>) -> Option<String> {
    let trimmed = url?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("//") {
        return Some(format!("https:{trimmed}"));
    }
    if !trimmed.contains("://") {
        return Some(format!("https://{trimmed}"));
    }
    Some(trimmed.to_string())
}

fn parse_uploader(root: &Value) -> Option<(String, Option<String>)> {
    let attr = root
        .get("uploadAttribution")
        .or_else(|| root.get("UploadAttribution"))?;
    let uploader = attr.get("Uploader")?;
    let username = uploader.get("username")?.as_str()?.trim();
    if username.is_empty() {
        return None;
    }
    let url = usable_url(uploader.get("url").and_then(Value::as_str));
    Some((username.to_string(), url))
}

fn parse_line_lyrics(root: &Value) -> Option<Vec<LyricLine>> {
    let arr = root.get("lineLyrics").and_then(Value::as_array)?;
    let mut lines = Vec::with_capacity(arr.len());
    for entry in arr {
        let text = entry
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let secs = entry.get("time").and_then(Value::as_f64).unwrap_or(-1.0);
        if secs < 0.0 {
            continue;
        }
        let agent = entry
            .get("speakerLabel")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut line = LyricLine::new(secs, text);
        line.agent_id = agent;
        lines.push(line);
    }
    (!lines.is_empty()).then_some(lines)
}

fn build_lyrics(root: &Value) -> Option<Lyrics> {
    if !root
        .get("hasLyrics")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    let uploader = parse_uploader(root);
    let provider_name = if uploader.is_some() {
        DISPLAY_NAME_VIA
    } else {
        DISPLAY_NAME
    };
    let source_url = uploader.as_ref().and_then(|(_, u)| u.clone());
    let uploader_credit = uploader.as_ref().map(|(u, _)| format!("Uploaded by {u}"));
    let songwriters: Vec<String> = root
        .get("songWriters")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    if let Some(ttml) = root
        .get("ttml")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        && let Some(mut parsed) = parse_ttml(ttml, provider_name)
        && !parsed.is_empty()
    {
        if parsed.source_url.is_none() {
            parsed.source_url = source_url;
        }
        if let Some(uc) = uploader_credit
            && !parsed.credits.iter().any(|c| c == &uc)
        {
            parsed.credits.push(uc);
        }
        return Some(parsed);
    }

    let base_credits: Vec<String> = uploader_credit.into_iter().collect();

    if let Some(lines) = parse_line_lyrics(root) {
        let mut l = Lyrics::new(lines, provider_name, true);
        l.songwriters = songwriters;
        l.credits = base_credits;
        l.format_type = Some("lrc".to_string());
        l.source_url = source_url;
        return Some(l);
    }

    if let Some(synced) = root
        .get("syncedLyrics")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let lines = parse_synced(synced);
        if !lines.is_empty() {
            let mut l = Lyrics::new(lines, provider_name, true);
            l.songwriters = songwriters;
            l.credits = base_credits;
            l.raw_format = Some(synced.to_string());
            l.format_type = Some("lrc".to_string());
            l.source_url = source_url;
            return Some(l);
        }
    }

    if let Some(plain) = root
        .get("lyrics")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let lines = parse_plain(plain);
        if !lines.is_empty() {
            let mut l = Lyrics::new(lines, provider_name, false);
            l.songwriters = songwriters;
            l.credits = base_credits;
            l.raw_format = Some(plain.to_string());
            l.format_type = Some("txt".to_string());
            l.source_url = source_url;
            return Some(l);
        }
    }

    None
}

impl LyricsProvider for RMMProvider {
    fn id(&self) -> &'static str {
        "rmm"
    }

    fn display_name(&self) -> &'static str {
        DISPLAY_NAME
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let term = format!("{} {}", track.title, track.artist);
            let search_url = format!(
                "{ITUNES_SEARCH_URL}?term={}&entity=song&limit=5",
                urlencoding::encode(&term)
            );
            let resp = ctx.client().get(&search_url).send().await?;
            if !resp.status().is_success() {
                return Ok(Vec::new());
            }
            let root: Value = resp.json().await?;
            let Some(results) = root.get("results").and_then(Value::as_array) else {
                return Ok(Vec::new());
            };

            let title_lower = track.title.trim().to_lowercase();
            let artist_lower = track.artist.trim().to_lowercase();

            struct ItunesHit<'a> {
                track_id: i64,
                title: &'a str,
                artist: &'a str,
                dur_secs: i64,
                score: i32,
                exact: bool,
            }

            let mut hits = Vec::new();
            for item in results {
                let track_id = item.get("trackId").and_then(Value::as_i64).unwrap_or(0);
                if track_id == 0 {
                    continue;
                }
                let item_title = item.get("trackName").and_then(Value::as_str).unwrap_or("");
                let item_artist = item.get("artistName").and_then(Value::as_str).unwrap_or("");
                let dur_secs = item
                    .get("trackTimeMillis")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    / 1000;
                let score = score_track_candidate(
                    item_title,
                    item_artist,
                    dur_secs,
                    None,
                    track,
                    ctx.custom_regex.as_ref(),
                );
                if score < SOFT_MIN {
                    continue;
                }
                let exact = !title_lower.is_empty()
                    && !artist_lower.is_empty()
                    && item_title.to_lowercase().contains(&title_lower)
                    && item_artist.to_lowercase().contains(&artist_lower);
                hits.push(ItunesHit {
                    track_id,
                    title: item_title,
                    artist: item_artist,
                    dur_secs,
                    score,
                    exact,
                });
            }

            hits.sort_by(|a, b| b.exact.cmp(&a.exact).then_with(|| b.score.cmp(&a.score)));

            let mut out = Vec::new();
            for hit in hits {
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
                let url = format!("{RMM_LYRICS}{}", hit.track_id);
                let Ok(resp) = ctx
                    .client()
                    .get(&url)
                    .header("Accept", "application/json")
                    .timeout(Duration::from_secs(20))
                    .send()
                    .await
                else {
                    continue;
                };
                if !resp.status().is_success() {
                    continue;
                }
                let Ok(json) = resp.json::<Value>().await else {
                    continue;
                };
                let Some(lyrics) = build_lyrics(&json) else {
                    continue;
                };
                let score = score_lyrics_candidate(
                    hit.title,
                    hit.artist,
                    hit.dur_secs,
                    &lyrics,
                    track,
                    ctx.custom_regex.as_ref(),
                );
                out.push(FetchedCandidate {
                    lyrics,
                    match_score: score.max(0),
                    is_blind: false,
                });
                if score >= 10 {
                    break;
                }
            }

            out.sort_by_key(|a| std::cmp::Reverse(a.match_score));
            Ok(out)
        })
    }
}
