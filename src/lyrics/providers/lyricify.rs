//! Lyricify (`credits.fm` ISRC resolver + `api.lyricify.app`) provider (`LyricifyProvider.java`).

use crate::lyrics::parsers::{
    parse_lyricify_lines, parse_lyricify_syllable, parse_lyricify_translation,
};
use crate::lyrics::providers::{
    BoxFuture, CHROME_DESKTOP_UA, FetchedCandidate, LyricsProvider, PreparedTrack, ProviderContext,
    throttle_ms,
};
use crate::lyrics::types::{LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;
use tokio::sync::RwLock;
use uuid::Uuid;

const API_BASE: &str = "https://api.lyricify.app/lyrics/get/mobile/new";
const CREDITS_FM_URL: &str = "https://api.credits.fm/v1/resolve/track";
const THROTTLE_MS: u64 = 500;
static LAST_REQ_MS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct Resolved {
    isrc: String,
    titles: Vec<String>,
    artists: Vec<String>,
}

static RESOLVED_CACHE: LazyLock<RwLock<HashMap<String, Resolved>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub struct LyricifyProvider;

fn random_username() -> String {
    let raw = Uuid::new_v4().simple().to_string();
    let letters: String = raw
        .bytes()
        .map(|b| match b {
            b'0'..=b'9' => (b'a' + (b - b'0')) as char,
            b'a'..=b'f' => (b'k' + (b - b'a')) as char,
            _ => 'z',
        })
        .take(8)
        .collect();
    letters
}

async fn resolve_from_credits_fm(
    ctx: &ProviderContext,
    title: &str,
    artist: &str,
) -> Option<Resolved> {
    throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
    let body = json!({ "name": title, "artist": artist });
    let resp = ctx
        .client()
        .post(CREDITS_FM_URL)
        .header("User-Agent", CHROME_DESKTOP_UA)
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    let isrc = root
        .get("isrc")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();

    let mut titles = Vec::with_capacity(2);
    if let Some(rec) = root
        .get("recording_title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        titles.push(rec.to_string());
    }
    if let Some(song) = root
        .get("song_title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && !titles.iter().any(|t| t == s))
    {
        titles.push(song.to_string());
    }
    let artists = root
        .get("artist_names")
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

    Some(Resolved {
        isrc,
        titles,
        artists,
    })
}

impl LyricsProvider for LyricifyProvider {
    fn id(&self) -> &'static str {
        "lyricify"
    }

    fn display_name(&self) -> &'static str {
        "Lyricify"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let resolved = if let Some(local_isrc) = ids
                .track_isrcs
                .first()
                .map(String::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Resolved {
                    isrc: local_isrc.to_string(),
                    titles: vec![track.title.clone()],
                    artists: vec![track.artist.clone()],
                }
            } else {
                let cache_key = format!(
                    "{}|{}",
                    track.title.to_lowercase(),
                    track.artist.to_lowercase()
                );
                let cached = { RESOLVED_CACHE.read().await.get(&cache_key).cloned() };
                match cached {
                    Some(r) => r,
                    None => {
                        let Some(r) =
                            resolve_from_credits_fm(ctx, &track.title, &track.artist).await
                        else {
                            return Ok(Vec::new());
                        };
                        RESOLVED_CACHE.write().await.insert(cache_key, r.clone());
                        r
                    }
                }
            };

            let isrc_b64 =
                base64::engine::general_purpose::STANDARD.encode(resolved.isrc.as_bytes());
            let url = format!(
                "{API_BASE}?username={}&isrc={}",
                random_username(),
                urlencoding::encode(&isrc_b64)
            );

            throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
            let resp = ctx
                .client()
                .get(&url)
                .header("User-Agent", CHROME_DESKTOP_UA)
                .header("Accept", "application/json")
                .send()
                .await?;
            if !resp.status().is_success() {
                return Ok(Vec::new());
            }
            let root: Value = resp.json().await?;
            if root
                .get("isInstrumental")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Ok(Vec::new());
            }

            let Some(text) = root
                .get("text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                return Ok(Vec::new());
            };
            let offset_ms = root.get("offset").and_then(Value::as_i64).unwrap_or(0);
            let is_syllable = text.contains("[from:AppleSyllable]");
            let lines = if is_syllable {
                parse_lyricify_syllable(text, offset_ms)
            } else {
                parse_lyricify_lines(text, offset_ms)
            };
            if lines.is_empty() {
                return Ok(Vec::new());
            }

            let mut lyrics = Lyrics::new(lines, self.display_name(), true);
            lyrics.raw_format = Some(text.to_string());
            lyrics.format_type = Some(if is_syllable { "lys" } else { "lyl" }.to_string());

            if let Some(writer) = root
                .get("writer")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                lyrics.credits.push(format!("Written by {writer}"));
            }

            if let Some(trans) = root
                .get("trans")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let parsed = parse_lyricify_translation(trans, offset_ms);
                if !parsed.is_empty() {
                    let mut trans_lines = Vec::with_capacity(lyrics.lines.len());
                    for (i, orig) in lyrics.lines.iter().enumerate() {
                        let t_text = parsed.get(i).map(|(_, s)| s.as_str()).unwrap_or("");
                        let mut line = LyricLine::new(orig.time, t_text);
                        line.end = orig.end;
                        trans_lines.push(line);
                    }
                    lyrics.translations.insert("zh".to_string(), trans_lines);
                }
            }

            if resolved.titles.is_empty() && resolved.artists.is_empty() {
                return Ok(FetchedCandidate::blind(lyrics).into_iter().collect());
            }

            let prepared = PreparedTrack::new(track, ctx.custom_regex.as_ref());
            let cand_titles = if resolved.titles.is_empty() {
                vec![""]
            } else {
                resolved.titles.iter().map(String::as_str).collect()
            };
            let cand_artists = if resolved.artists.is_empty() {
                vec![""]
            } else {
                resolved.artists.iter().map(String::as_str).collect()
            };

            let mut best = None;
            for t in &cand_titles {
                for a in &cand_artists {
                    let v = prepared.evaluate(t, a, 0, None);
                    if best
                        .as_ref()
                        .is_none_or(|cur: &crate::lyrics::providers::MatchVerdict| {
                            v.score > cur.score
                        })
                    {
                        best = Some(v);
                    }
                }
            }

            if let Some(v) = best
                && v.is_high_match()
            {
                return Ok(vec![FetchedCandidate {
                    lyrics,
                    match_score: v.score,
                    is_blind: true,
                    video_id_keyed: false,
                }]);
            }

            Ok(Vec::new())
        })
    }
}
