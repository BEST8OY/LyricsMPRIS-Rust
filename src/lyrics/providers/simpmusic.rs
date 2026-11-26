//! SimpMusic (`api-lyrics.simpmusic.org`) provider (`SimpMusicProvider.java`).

use crate::lyrics::filter::is_cjk_ideograph;
use crate::lyrics::parsers::{parse_plain, parse_synced_with_credits};
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, ProviderContext, SOFT_MIN,
    VIDEO_ID_TRUST, score_lyrics_candidate, score_track_candidate,
};
use crate::lyrics::types::{LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

const DEFAULT_BASE_URL: &str = "https://api-lyrics.simpmusic.org/v1";
const CLIENT_USER_AGENT: &str = "SimpMusicLyrics/1.0";
const SEARCH_LIMIT: usize = 10;

static HTML_ENTITY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&#x([0-9a-fA-F]+);|&#(\d+);").unwrap());
static WHITESPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\s\u{00A0}\u{2000}-\u{200A}\u{202F}\u{3000}]+").unwrap());
static MULTI_SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]{2,}").unwrap());

pub struct SimpMusicProvider;

fn base_url(ctx: &ProviderContext) -> String {
    crate::lyrics::providers::endpoint_url(
        ctx.settings.endpoints.simpmusic.as_deref(),
        DEFAULT_BASE_URL,
    )
}

fn source_url(video_id: &str) -> String {
    format!("https://lyrics.simpmusic.org/#/video/{video_id}")
}

fn is_han_or_kana(c: char) -> bool {
    is_cjk_ideograph(c)
        || matches!(c as u32,
            0x3040..=0x309F // Hiragana
            | 0x30A0..=0x30FF // Katakana
            | 0x31F0..=0x31FF // Katakana Phonetic
        )
}

fn remove_cjk_spaces(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        if c != ' ' {
            out.push(c);
            continue;
        }
        let left = i > 0 && is_han_or_kana(chars[i - 1]);
        let right = i + 1 < chars.len() && is_han_or_kana(chars[i + 1]);
        if !(left && right) {
            out.push(c);
        }
    }
    out
}

fn decode_html_entities(input: &str) -> String {
    let replaced = HTML_ENTITY_RE.replace_all(input, |caps: &regex::Captures<'_>| {
        let code = caps
            .get(1)
            .and_then(|m| u32::from_str_radix(m.as_str(), 16).ok())
            .or_else(|| caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok()));
        code.and_then(char::from_u32)
            .map(|ch| ch.to_string())
            .unwrap_or_else(|| caps[0].to_string())
    });
    replaced
        .replace("&nbsp;", "\u{00A0}")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn sanitize_lrc(lrc: &str) -> (String, f64) {
    let mut offset_secs = 0.0_f64;
    let mut sb = String::with_capacity(lrc.len());

    for raw_line in lrc.lines() {
        let trimmed = raw_line.trim();
        if let Some(rest) = trimmed.strip_prefix("[offset:")
            && let Some(close) = rest.find(']')
        {
            if let Ok(val) = rest[..close].trim().parse::<f64>() {
                offset_secs = val;
            }
            continue;
        }
        sb.push_str(raw_line);
        sb.push('\n');
    }

    let decoded = decode_html_entities(&sb);
    let folded = MULTI_SPACE_RE.replace_all(&decoded, " ");
    (folded.trim().to_string(), offset_secs)
}

fn normalize_lines(lines: Vec<LyricLine>) -> Vec<LyricLine> {
    lines
        .into_iter()
        .map(|mut line| {
            let folded = WHITESPACE_RE.replace_all(&line.text, " ");
            line.text = remove_cjk_spaces(folded.trim());
            line
        })
        .collect()
}

fn apply_offset(lines: &mut [LyricLine], offset_secs: f64) {
    crate::lyrics::types::shift_lines_timings(lines, offset_secs);
}

fn parse_simpmusic_lrc(lrc: &str, video_id: &str) -> Option<Lyrics> {
    let (sanitized, offset_secs) = sanitize_lrc(lrc);
    let (mut lines, credits) = parse_synced_with_credits(&sanitized);
    if lines.is_empty() {
        return None;
    }
    lines = normalize_lines(lines);
    apply_offset(&mut lines, offset_secs);
    let mut lyrics = Lyrics::new(lines, "SimpMusic", true);
    lyrics.credits = credits;
    lyrics.raw_format = Some(sanitized);
    lyrics.format_type = Some("lrc".to_string());
    lyrics.source_url = Some(source_url(video_id));
    Some(lyrics)
}

fn parse_entry(entry: &Value, video_id: &str) -> Option<Lyrics> {
    let contributor = entry
        .get("contributor")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut lyrics = if let Some(rich) = entry
        .get("richSyncLyrics")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        parse_simpmusic_lrc(rich, video_id)
    } else if let Some(synced) = entry
        .get("syncedLyrics")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        parse_simpmusic_lrc(synced, video_id)
    } else if let Some(plain) = entry
        .get("plainLyric")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        let lines = normalize_lines(parse_plain(plain));
        if lines.is_empty() {
            None
        } else {
            let mut l = Lyrics::new(lines, "SimpMusic", false);
            l.raw_format = Some(plain.to_string());
            l.format_type = Some("plain".to_string());
            l.source_url = Some(source_url(video_id));
            Some(l)
        }
    } else {
        None
    }?;

    if let Some(c) = contributor {
        let credit = format!("Submitted by {c}");
        if !lyrics.credits.iter().any(|x| x == &credit) {
            lyrics.credits.push(credit);
        }
    }
    Some(lyrics)
}

async fn fetch_entries(ctx: &ProviderContext, base: &str, video_id: &str) -> Option<Vec<Value>> {
    let url = format!("{base}/{}", urlencoding::encode(video_id));
    let resp = ctx
        .client()
        .get(&url)
        .header("User-Agent", CLIENT_USER_AGENT)
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    if !root
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let arr = root.get("data")?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    Some(arr.clone())
}

fn pick_best_entry(entries: &[Value], video_id: &str) -> Option<Lyrics> {
    let mut best: Option<(i64, Lyrics)> = None;
    for entry in entries {
        let vote = entry.get("vote").and_then(Value::as_i64).unwrap_or(0);
        if let Some((_, _)) = &best
            && vote < best.as_ref().unwrap().0
        {
            continue;
        }
        if let Some(lyrics) = parse_entry(entry, video_id)
            && !lyrics.is_empty()
        {
            best = Some((vote, lyrics));
        }
    }
    best.map(|(_, l)| l)
}

impl LyricsProvider for SimpMusicProvider {
    fn id(&self) -> &'static str {
        "simpmusic"
    }

    fn display_name(&self) -> &'static str {
        "SimpMusic"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let base = base_url(ctx);

            // 1. Direct videoId lookup if present
            if let Some(vid) = ids
                .youtube_video_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                && let Some(entries) = fetch_entries(ctx, &base, vid).await
            {
                let mut voted: Vec<(i64, Lyrics)> = entries
                    .iter()
                    .filter_map(|e| {
                        let l = parse_entry(e, vid)?;
                        if l.is_empty() {
                            return None;
                        }
                        let vote = e.get("vote").and_then(Value::as_i64).unwrap_or(0);
                        Some((vote, l))
                    })
                    .collect();
                voted.sort_by(|a, b| {
                    b.0.cmp(&a.0)
                        .then_with(|| b.1.sync_rank().cmp(&a.1.sync_rank()))
                });
                let out: Vec<FetchedCandidate> = voted
                    .into_iter()
                    .take(MAX_CANDIDATES)
                    .map(|(_, l)| FetchedCandidate {
                        lyrics: l,
                        match_score: VIDEO_ID_TRUST,
                        is_blind: false,
                        video_id_keyed: true,
                    })
                    .collect();
                if !out.is_empty() {
                    return Ok(out);
                }
            }

            // 2. Metadata search
            if track.title.is_empty() || track.artist.is_empty() {
                return Ok(Vec::new());
            }
            let query = format!("{} {}", track.title, track.artist);
            let search_url = format!(
                "{base}/search?q={}&limit={SEARCH_LIMIT}",
                urlencoding::encode(&query)
            );
            let resp = ctx
                .client()
                .get(&search_url)
                .header("User-Agent", CLIENT_USER_AGENT)
                .header("Accept", "application/json")
                .send()
                .await?;
            if !resp.status().is_success() {
                return Ok(Vec::new());
            }
            let root: Value = resp.json().await?;
            if !root
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Ok(Vec::new());
            }
            let Some(data) = root.get("data").and_then(Value::as_array) else {
                return Ok(Vec::new());
            };

            struct Hit {
                video_id: String,
                title: String,
                artist: String,
                duration_sec: i64,
                score: i32,
            }

            let mut gated = Vec::new();
            let mut fallback: Option<(i32, Hit)> = None;
            for entry in data {
                let Some(vid) = entry.get("videoId").and_then(Value::as_str) else {
                    continue;
                };
                let title = entry.get("songTitle").and_then(Value::as_str).unwrap_or("");
                let artist = entry
                    .get("artistName")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let dur_sec = entry
                    .get("durationSeconds")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let score = score_track_candidate(
                    title,
                    artist,
                    dur_sec,
                    None,
                    track,
                    ctx.custom_regex.as_ref(),
                );
                let hit = Hit {
                    video_id: vid.to_string(),
                    title: title.to_string(),
                    artist: artist.to_string(),
                    duration_sec: dur_sec,
                    score,
                };
                if score >= 0 {
                    gated.push(hit);
                } else if fallback.as_ref().is_none_or(|(s, _)| score > *s) {
                    fallback = Some((score, hit));
                }
            }

            gated.sort_by(|a, b| {
                b.score.cmp(&a.score).then_with(|| {
                    if track.duration_secs <= 0 {
                        std::cmp::Ordering::Equal
                    } else {
                        (a.duration_sec - track.duration_secs)
                            .abs()
                            .cmp(&(b.duration_sec - track.duration_secs).abs())
                    }
                })
            });

            let hits: Vec<Hit> = if !gated.is_empty() {
                gated.into_iter().take(MAX_CANDIDATES).collect()
            } else {
                fallback.into_iter().map(|(_, h)| h).collect()
            };

            let mut out = Vec::new();
            for hit in hits {
                let Some(entries) = fetch_entries(ctx, &base, &hit.video_id).await else {
                    continue;
                };
                let Some(lyrics) = pick_best_entry(&entries, &hit.video_id) else {
                    continue;
                };
                if lyrics.is_empty() {
                    continue;
                }
                let mut score = score_lyrics_candidate(
                    &hit.title,
                    &hit.artist,
                    hit.duration_sec,
                    &lyrics,
                    track,
                    ctx.custom_regex.as_ref(),
                );
                if score < 0 {
                    score = hit.score + lyrics.sync_rank();
                }
                if score >= SOFT_MIN || out.is_empty() {
                    out.push(FetchedCandidate {
                        lyrics,
                        match_score: score.max(0),
                        is_blind: false,
                        video_id_keyed: false,
                    });
                }
            }

            out.sort_by_key(|a| std::cmp::Reverse(a.match_score));
            Ok(out)
        })
    }
}
