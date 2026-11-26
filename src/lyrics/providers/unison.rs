//! Unison (`unison.boidu.dev`) TTML / LRC / plain provider (`UnisonProvider.java`).

use crate::lyrics::crypto::to_hex;
use crate::lyrics::parsers::{parse_plain, parse_synced_with_credits, parse_ttml};
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, ProviderContext, SOFT_MIN,
    VIDEO_ID_TRUST, score_lyrics_candidate, score_track_candidate,
};
use crate::lyrics::types::{Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use uuid::Uuid;

const DEFAULT_BASE_URL: &str = "https://unison.boidu.dev/lyrics";
const SEARCH_LIMIT: usize = 10;
static KEY_ID: OnceLock<String> = OnceLock::new();

pub struct UnisonProvider;

fn key_id() -> &'static str {
    KEY_ID.get_or_init(|| {
        let mut hasher = Sha256::new();
        hasher.update(format!("unison-key-{}", Uuid::new_v4()).as_bytes());
        to_hex(&hasher.finalize())
    })
}

fn base_url(ctx: &ProviderContext) -> String {
    crate::lyrics::providers::endpoint_url(
        ctx.settings.endpoints.unison.as_deref(),
        DEFAULT_BASE_URL,
    )
}

fn source_url(video_id: &str) -> String {
    format!("https://unison.boidu.dev/song/{video_id}")
}

fn parse_variant(variant: &Value, video_id: &str, display_name: &str) -> Option<Lyrics> {
    let format = variant.get("format").and_then(Value::as_str)?.trim();
    let body = variant.get("lyrics").and_then(Value::as_str)?;
    if body.trim().is_empty() {
        return None;
    }
    let submitter = variant
        .get("submitter")
        .and_then(|s| s.get("displayName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let src = source_url(video_id);
    let mut lyrics = match format.to_ascii_lowercase().as_str() {
        "ttml" => parse_ttml(body, display_name)?,
        "lrc" => {
            let (lines, credits) = parse_synced_with_credits(body);
            if lines.is_empty() {
                return None;
            }
            let mut l = Lyrics::new(lines, display_name, true);
            l.credits = credits;
            l.raw_format = Some(body.to_string());
            l.format_type = Some("lrc".to_string());
            l
        }
        "plain" => {
            let lines = parse_plain(body);
            if lines.is_empty() {
                return None;
            }
            let mut l = Lyrics::new(lines, display_name, false);
            l.raw_format = Some(body.to_string());
            l.format_type = Some("plain".to_string());
            l
        }
        _ => return None,
    };

    if lyrics.source_url.is_none() {
        lyrics.source_url = Some(src);
    }
    if let Some(sub) = submitter {
        let credit = format!("Submitted by {sub}");
        if !lyrics.credits.iter().any(|c| c == &credit) {
            lyrics.credits.push(credit);
        }
    }
    Some(lyrics)
}

async fn get_json(ctx: &ProviderContext, url: &str) -> Option<Value> {
    let resp = ctx
        .client()
        .get(url)
        .header("X-Key-ID", key_id())
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

impl LyricsProvider for UnisonProvider {
    fn id(&self) -> &'static str {
        "unison"
    }

    fn display_name(&self) -> &'static str {
        "Unison"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let base = base_url(ctx);

            // 1. Direct YouTube videoId lookup if available
            if let Some(vid) = ids
                .youtube_video_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let mut results = Vec::new();
                let variants_url = format!("{base}/variants/{}", urlencoding::encode(vid));
                if let Some(root) = get_json(ctx, &variants_url).await
                    && let Some(arr) = root.get("data").and_then(Value::as_array)
                {
                    let mut voted: Vec<(i64, Lyrics)> = arr
                        .iter()
                        .filter(|v| !v.get("hidden").and_then(Value::as_bool).unwrap_or(false))
                        .filter_map(|v| {
                            let l = parse_variant(v, vid, self.display_name())?;
                            let vote = v.get("voteCount").and_then(Value::as_i64).unwrap_or(0);
                            Some((vote, l))
                        })
                        .collect();
                    voted.sort_by(|a, b| {
                        b.0.cmp(&a.0)
                            .then_with(|| b.1.sync_rank().cmp(&a.1.sync_rank()))
                    });
                    for (_, l) in voted.into_iter().take(MAX_CANDIDATES) {
                        results.push(FetchedCandidate {
                            match_score: VIDEO_ID_TRUST,
                            lyrics: l,
                            is_blind: false,
                        });
                    }
                }
                if results.is_empty() {
                    let mut url = format!("{base}?v={}", urlencoding::encode(vid));
                    if !track.title.is_empty() {
                        url.push_str("&song=");
                        url.push_str(&urlencoding::encode(&track.title));
                    }
                    if !track.artist.is_empty() {
                        url.push_str("&artist=");
                        url.push_str(&urlencoding::encode(&track.artist));
                    }
                    if track.duration_secs > 0 {
                        url.push_str(&format!("&duration={}", track.duration_secs));
                    }
                    if !track.album.is_empty() {
                        url.push_str("&album=");
                        url.push_str(&urlencoding::encode(&track.album));
                    }
                    if let Some(root) = get_json(ctx, &url).await
                        && let Some(data) = root.get("data")
                        && let Some(l) = parse_variant(data, vid, self.display_name())
                        && let Some(cand) = FetchedCandidate::video_id(l)
                    {
                        results.push(cand);
                    }
                }
                if !results.is_empty() {
                    return Ok(results);
                }
            }

            // 2. Fallback to metadata search (/search?song=...&artist=...&limit=10)
            if track.title.is_empty() || track.artist.is_empty() {
                return Ok(Vec::new());
            }
            let url = format!(
                "{base}/search?song={}&artist={}&limit={SEARCH_LIMIT}",
                urlencoding::encode(&track.title),
                urlencoding::encode(&track.artist),
            );
            let Some(root) = get_json(ctx, &url).await else {
                return Ok(Vec::new());
            };
            let Some(data) = root.get("data").and_then(Value::as_array) else {
                return Ok(Vec::new());
            };

            struct Hit<'v> {
                video_id: &'v str,
                song: &'v str,
                artist: &'v str,
                duration_sec: i64,
                score: i32,
                variant: &'v Value,
            }

            let mut gated = Vec::new();
            let mut fallback: Option<(i32, Hit<'_>)> = None;

            for entry in data {
                if entry
                    .get("hidden")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    continue;
                }
                let Some(vid) = entry.get("videoId").and_then(Value::as_str) else {
                    continue;
                };
                let song = entry.get("song").and_then(Value::as_str).unwrap_or("");
                let artist = entry.get("artist").and_then(Value::as_str).unwrap_or("");
                let dur_sec = entry.get("duration").and_then(Value::as_i64).unwrap_or(0);
                let score = score_track_candidate(
                    song,
                    artist,
                    dur_sec,
                    None,
                    track,
                    ctx.custom_regex.as_ref(),
                );
                let hit = Hit {
                    video_id: vid,
                    song,
                    artist,
                    duration_sec: dur_sec,
                    score,
                    variant: entry,
                };
                if score >= 0 {
                    gated.push(hit);
                } else if fallback.as_ref().is_none_or(|(raw, _)| score > *raw) {
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

            let hits: Vec<Hit<'_>> = if !gated.is_empty() {
                gated.into_iter().take(MAX_CANDIDATES).collect()
            } else {
                fallback.into_iter().map(|(_, h)| h).collect()
            };

            let mut out = Vec::new();
            for hit in hits {
                let Some(lyrics) = parse_variant(hit.variant, hit.video_id, self.display_name())
                else {
                    continue;
                };
                if lyrics.is_empty() {
                    continue;
                }
                let mut score = score_lyrics_candidate(
                    hit.song,
                    hit.artist,
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
                    });
                }
            }

            out.sort_by_key(|a| std::cmp::Reverse(a.match_score));
            Ok(out)
        })
    }
}
