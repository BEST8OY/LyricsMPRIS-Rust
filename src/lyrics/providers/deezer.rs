//! Deezer (`arl` cookie + GraphQL `SynchronizedTrackLyrics`) provider (`DeezerProvider.java`).

use crate::lyrics::parsers::lrc::estimate_end_secs;
use crate::lyrics::parsers::parse_plain;
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, PreparedTrack, ProviderContext,
    SOFT_MIN, score_lyrics_candidate, throttle_ms,
};
use crate::lyrics::types::{LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const SEARCH_URL: &str = "https://api.deezer.com/search";
const AUTH_URL: &str = "https://auth.deezer.com/login/arl?jo=p&rto=c&i=c";
const PIPE_URL: &str = "https://pipe.deezer.com/api";
const JWT_TTL_MS: u64 = 4 * 60 * 1000;
const THROTTLE_MS: u64 = 250;

const LYRICS_QUERY: &str = "query SynchronizedTrackLyrics($trackId: String!) { track(trackId: $trackId) { id lyrics { id text copyright writers synchronizedLines { lrcTimestamp line milliseconds duration } } } }";
const LYRICS_QUERY_MINIMAL: &str = "query SynchronizedTrackLyrics($trackId: String!) { track(trackId: $trackId) { id lyrics { id text synchronizedLines { lrcTimestamp line milliseconds duration } } } }";

static LAST_REQ_MS: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct DeezerJwt {
    arl: String,
    jwt: Option<String>,
    expiry_ms: u64,
}

static JWT_CACHE: LazyLock<Mutex<DeezerJwt>> = LazyLock::new(|| Mutex::new(DeezerJwt::default()));

pub struct DeezerProvider;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

async fn get_jwt(ctx: &ProviderContext, arl: &str) -> Option<String> {
    let mut cache = JWT_CACHE.lock().await;
    let now = now_ms();
    if cache.arl == arl && cache.jwt.is_some() && now < cache.expiry_ms {
        return cache.jwt.clone();
    }

    let resp = ctx
        .client()
        .post(AUTH_URL)
        .header("Cookie", format!("arl={arl}"))
        .header("Accept", "*/*")
        .header("Content-Type", "application/json")
        .body("")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let json: Value = serde_json::from_str(&body).ok()?;
    let jwt = json
        .get("jwt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();

    cache.arl = arl.to_string();
    cache.jwt = Some(jwt.clone());
    cache.expiry_ms = now + JWT_TTL_MS;
    Some(jwt)
}

async fn post_pipe(ctx: &ProviderContext, track_id: i64, query: &str, arl: &str) -> Option<Value> {
    let payload = json!({
        "operationName": "SynchronizedTrackLyrics",
        "variables": { "trackId": track_id.to_string() },
        "query": query,
    });

    for retry in 0..=1 {
        throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
        let jwt = get_jwt(ctx, arl).await?;
        let Ok(resp) = ctx
            .client()
            .post(PIPE_URL)
            .header("Authorization", format!("Bearer {jwt}"))
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .await
        else {
            return None;
        };
        let status = resp.status().as_u16();
        if (status == 401 || status == 403) && retry == 0 {
            let mut cache = JWT_CACHE.lock().await;
            cache.jwt = None;
            cache.expiry_ms = 0;
            continue;
        }
        if !resp.status().is_success() {
            return None;
        }
        return resp.json().await.ok();
    }
    None
}

fn flatten_writers(val: Option<&Value>) -> Vec<String> {
    let Some(val) = val else {
        return Vec::new();
    };
    let joined = match val {
        Value::String(s) => s.trim().to_string(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    };
    joined
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

async fn fetch_by_track_id(ctx: &ProviderContext, track_id: i64, arl: &str) -> Option<Lyrics> {
    let mut resp = post_pipe(ctx, track_id, LYRICS_QUERY, arl).await;
    if resp.as_ref().is_some_and(|r| r.get("errors").is_some()) {
        resp = post_pipe(ctx, track_id, LYRICS_QUERY_MINIMAL, arl).await;
    }
    let lyrics_obj = resp?.get("data")?.get("track")?.get("lyrics")?.clone();

    let songwriters = flatten_writers(lyrics_obj.get("writers"));
    let credits: Vec<String> = lyrics_obj
        .get("copyright")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|c| format!("Copyright: {c}"))
        .into_iter()
        .collect();
    let source_url = format!("https://www.deezer.com/track/{track_id}");

    if let Some(synced) = lyrics_obj
        .get("synchronizedLines")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        let mut lines: Vec<LyricLine> = Vec::with_capacity(synced.len());
        for item in synced {
            let ms = item
                .get("milliseconds")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let text = item
                .get("line")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if text.is_empty() {
                continue;
            }
            lines.push(LyricLine::new(ms as f64 / 1000.0, text));
        }
        if !lines.is_empty() {
            let len = lines.len();
            for i in 0..len {
                let next = (i + 1 < len).then(|| lines[i + 1].time);
                lines[i].end = Some(estimate_end_secs(lines[i].time, next, &lines[i].text));
            }
            let mut l = Lyrics::new(lines, "Deezer", true);
            l.songwriters = songwriters;
            l.credits = credits;
            l.raw_format = serde_json::to_string(synced).ok();
            l.format_type = Some("dzr.json".to_string());
            l.source_url = Some(source_url);
            return Some(l);
        }
    }

    if let Some(text) = lyrics_obj
        .get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        let lines = parse_plain(text);
        if !lines.is_empty() {
            let mut l = Lyrics::new(lines, "Deezer", false);
            l.songwriters = songwriters;
            l.credits = credits;
            l.raw_format = Some(text.to_string());
            l.format_type = Some("txt".to_string());
            l.source_url = Some(source_url);
            return Some(l);
        }
    }

    None
}

impl LyricsProvider for DeezerProvider {
    fn id(&self) -> &'static str {
        "deezer"
    }

    fn display_name(&self) -> &'static str {
        "Deezer"
    }

    fn is_configured(&self, ctx: &ProviderContext) -> bool {
        ctx.settings
            .tokens
            .deezer_arl
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty() && s != "null")
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let Some(arl) = ctx
                .settings
                .tokens
                .deezer_arl
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "null")
            else {
                return Ok(Vec::new());
            };

            if get_jwt(ctx, arl).await.is_none() {
                return Ok(Vec::new());
            }

            throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
            let q = format!("{} - {}", track.artist, track.title);
            let url = format!(
                "{SEARCH_URL}?q={}&limit=10&output=json",
                urlencoding::encode(&q)
            );
            let resp = ctx
                .client()
                .get(&url)
                .header("Accept", "application/json")
                .send()
                .await?;
            if !resp.status().is_success() {
                return Ok(Vec::new());
            }
            let root: Value = resp.json().await?;
            let Some(data) = root.get("data").and_then(Value::as_array) else {
                return Ok(Vec::new());
            };

            let prepared = PreparedTrack::new(track, ctx.custom_regex.as_ref());
            let mut scored: Vec<(i32, &Value)> = data
                .iter()
                .map(|item| {
                    let t = item.get("title").and_then(Value::as_str).unwrap_or("");
                    let a = item
                        .get("artist")
                        .and_then(|x| x.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let alb = item
                        .get("album")
                        .and_then(|x| x.get("title"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let d = item.get("duration").and_then(Value::as_i64).unwrap_or(0);
                    let v = prepared.evaluate(t, a, d, (!alb.is_empty()).then_some(alb));
                    (v.score, item)
                })
                .collect();
            scored.sort_by_key(|a| std::cmp::Reverse(a.0));

            let mut passed: Vec<&Value> = scored
                .iter()
                .filter(|(sc, _)| *sc >= SOFT_MIN)
                .map(|(_, item)| *item)
                .collect();
            if passed.is_empty()
                && let Some((_, first)) = scored.into_iter().next()
            {
                passed.push(first);
            }

            let mut out = Vec::new();
            for item in passed {
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
                let tid = item.get("id").and_then(Value::as_i64).unwrap_or(-1);
                if tid <= 0 {
                    continue;
                }
                let Some(lyrics) = fetch_by_track_id(ctx, tid, arl).await else {
                    continue;
                };
                let t = item.get("title").and_then(Value::as_str).unwrap_or("");
                let a = item
                    .get("artist")
                    .and_then(|x| x.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let d = item.get("duration").and_then(Value::as_i64).unwrap_or(0);
                let score =
                    score_lyrics_candidate(t, a, d, &lyrics, track, ctx.custom_regex.as_ref());
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
