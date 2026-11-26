//! LunaBeat static GitHub Pages TTML hub provider (`LunaBeatProvider.java`).

use crate::lyrics::parsers::parse_ttml;
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, ProviderContext, SOFT_MIN,
    score_lyrics_candidate, score_track_candidate, throttle_ms,
};
use crate::lyrics::types::{LyricsError, TrackInfo, TrackMatchInfo};
use serde_json::Value;
use std::sync::atomic::AtomicU64;
use tokio::sync::OnceCell;
use unicode_normalization::UnicodeNormalization;

const DEFAULT_BASE_URL: &str = "https://2755337087.github.io/ttml-hub";
const THROTTLE_MS: u64 = 500;
static LAST_REQ_MS: AtomicU64 = AtomicU64::new(0);
static SONG_INDEX: OnceCell<Vec<SongEntry>> = OnceCell::const_new();

#[derive(Clone, Debug)]
struct SongEntry {
    title: String,
    artists: Vec<String>,
    path: String,
}

pub struct LunaBeatProvider;

fn base_url(ctx: &ProviderContext) -> String {
    crate::lyrics::providers::endpoint_url(
        ctx.settings.endpoints.lunabeat.as_deref(),
        DEFAULT_BASE_URL,
    )
}

fn normalize_lunabeat(s: &str) -> String {
    s.nfkc()
        .collect::<String>()
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '·' | '・' | '.' | '-' | '_'))
        .collect()
}

async fn load_song_index(ctx: &ProviderContext, base: &str) -> Vec<SongEntry> {
    throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
    let Ok(resp) = ctx
        .client()
        .get(format!("{base}/api/v1/manifest.json"))
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(manifest) = resp.json::<Value>().await else {
        return Vec::new();
    };
    if manifest
        .get("revision")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return Vec::new();
    }

    throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
    let Ok(resp) = ctx
        .client()
        .get(format!("{base}/api/v1/songs.json"))
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(root) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let Some(songs) = root.get("songs").and_then(Value::as_array) else {
        return Vec::new();
    };

    songs
        .iter()
        .filter_map(|obj| {
            let id = obj.get("id").and_then(Value::as_str).unwrap_or("");
            let title = obj.get("title").and_then(Value::as_str).unwrap_or("");
            let path = obj.get("path").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() || title.is_empty() || path.is_empty() {
                return None;
            }
            let artists = obj
                .get("artists")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Some(SongEntry {
                title: title.to_string(),
                artists,
                path: path.to_string(),
            })
        })
        .collect()
}

impl LyricsProvider for LunaBeatProvider {
    fn id(&self) -> &'static str {
        "lunabeat"
    }

    fn display_name(&self) -> &'static str {
        "LunaBeat"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let base = base_url(ctx);
            let songs = if ctx.settings.endpoints.lunabeat.is_some() {
                load_song_index(ctx, &base).await
            } else {
                SONG_INDEX
                    .get_or_init(|| async { load_song_index(ctx, &base).await })
                    .await
                    .clone()
            };

            let nt = normalize_lunabeat(&track.title);
            let na = normalize_lunabeat(&track.artist);
            let matches: Vec<&SongEntry> = songs
                .iter()
                .filter(|s| {
                    normalize_lunabeat(&s.title) == nt
                        && s.artists.iter().any(|a| normalize_lunabeat(a) == na)
                })
                .collect();

            let mut out = Vec::new();
            for song in matches {
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
                let artist = song.artists.first().map(String::as_str).unwrap_or("");
                let t_score = score_track_candidate(
                    &song.title,
                    artist,
                    0,
                    None,
                    track,
                    ctx.custom_regex.as_ref(),
                );
                if t_score < SOFT_MIN && !out.is_empty() {
                    continue;
                }
                throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
                let url = format!("{base}/{}", song.path.trim_start_matches('/'));
                let Ok(resp) = ctx.client().get(&url).send().await else {
                    continue;
                };
                if !resp.status().is_success() {
                    continue;
                }
                let Ok(ttml) = resp.text().await else {
                    continue;
                };
                let Some(lyrics) = parse_ttml(&ttml, self.display_name()) else {
                    continue;
                };
                if lyrics.is_empty() {
                    continue;
                }
                let score = score_lyrics_candidate(
                    &song.title,
                    artist,
                    0,
                    &lyrics,
                    track,
                    ctx.custom_regex.as_ref(),
                );
                out.push(FetchedCandidate {
                    lyrics,
                    match_score: score.max(0),
                    is_blind: false,
                    video_id_keyed: false,
                });
            }

            out.sort_by_key(|a| std::cmp::Reverse(a.match_score));
            Ok(out)
        })
    }
}
