//! AMLL (Apple Music-like Lyrics DB) TTML provider (`AMLLProvider.java`).

use crate::lyrics::parsers::parse_ttml;
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, PreparedTrack, ProviderContext, throttle_ms,
};
use crate::lyrics::types::{LyricsError, TrackInfo, TrackMatchInfo};
use crate::text_utils::clean_artist;
use serde_json::Value;
use std::sync::atomic::AtomicU64;

const DEFAULT_BASE_URL: &str = "https://api.amll.dev/v1/lyrics";
const THROTTLE_MS: u64 = 500;
static LAST_REQ_MS: AtomicU64 = AtomicU64::new(0);

pub struct AMLLProvider;

fn base_url(ctx: &ProviderContext) -> String {
    crate::lyrics::providers::endpoint_url(ctx.settings.endpoints.amll.as_deref(), DEFAULT_BASE_URL)
}

fn first_path(obj: &Value) -> Option<&str> {
    for key in ["path", "file", "filename", "filePath"] {
        if let Some(s) = obj.get(key).and_then(Value::as_str).map(str::trim)
            && !s.is_empty()
        {
            return Some(s);
        }
    }
    None
}

fn source_url(get_data: &Value, best: &Value) -> Option<String> {
    let file = first_path(get_data).or_else(|| first_path(best))?;
    Some(format!(
        "https://amlldb.bikonoo.com/view-lyric.html?file={file}"
    ))
}

fn string_list(obj: &Value, key: &str) -> Vec<String> {
    obj.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn search_best(
    ctx: &ProviderContext,
    base: &str,
    title: &str,
    artist: &str,
    album: Option<&str>,
) -> Option<Value> {
    let mut url = format!(
        "{}/search?musicName={}&artistName={}",
        base,
        urlencoding::encode(title),
        urlencoding::encode(artist)
    );
    if let Some(alb) = album
        && !alb.is_empty()
    {
        url.push_str("&albumName=");
        url.push_str(&urlencoding::encode(alb));
    }

    throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
    let resp = ctx.client().get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    let items = root.get("data")?.get("items")?.as_array()?;
    let best = items.first()?;
    if best.get("id").and_then(Value::as_i64).unwrap_or(-1) < 0 {
        return None;
    }
    Some(best.clone())
}

impl LyricsProvider for AMLLProvider {
    fn id(&self) -> &'static str {
        "amll"
    }

    fn display_name(&self) -> &'static str {
        "AMLL"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let raw_title = track.title.trim();
            let raw_artist = track.artist.trim();
            if raw_title.is_empty() || raw_artist.is_empty() {
                return Ok(Vec::new());
            }

            let base = base_url(ctx);
            let cleaned_artist = clean_artist(raw_artist);
            let artist = if cleaned_artist.is_empty() {
                raw_artist
            } else {
                cleaned_artist.as_str()
            };

            let album = (!track.album.is_empty()).then_some(track.album.as_str());
            let mut best = search_best(ctx, &base, raw_title, artist, album).await;
            if best.is_none() && album.is_some() {
                best = search_best(ctx, &base, raw_title, artist, None).await;
            }
            if best.is_none() && !artist.eq_ignore_ascii_case(raw_artist) {
                best = search_best(ctx, &base, raw_title, raw_artist, None).await;
            }

            let Some(best) = best else {
                return Ok(Vec::new());
            };
            let lyric_id = best.get("id").and_then(Value::as_i64).unwrap_or(-1);
            if lyric_id < 0 {
                return Ok(Vec::new());
            }

            throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
            let get_url = format!("{base}/get?id={lyric_id}&format=ttml");
            let resp = ctx.client().get(&get_url).send().await?;
            if !resp.status().is_success() {
                return Ok(Vec::new());
            }
            let root: Value = resp.json().await?;
            let Some(data) = root.get("data") else {
                return Ok(Vec::new());
            };
            let Some(ttml) = data.get("lyrics").and_then(Value::as_str) else {
                return Ok(Vec::new());
            };

            let Some(mut lyrics) = parse_ttml(ttml, self.display_name()) else {
                return Ok(Vec::new());
            };
            if lyrics.is_empty() {
                return Ok(Vec::new());
            }
            lyrics.source_url = source_url(data, &best);

            let titles = string_list(&best, "musicNames");
            let artists = string_list(&best, "artistNames");

            if titles.is_empty() && artists.is_empty() {
                return Ok(FetchedCandidate::blind(lyrics).into_iter().collect());
            }

            let prepared = PreparedTrack::new(track, ctx.custom_regex.as_ref());
            let cand_titles = if titles.is_empty() {
                vec![""]
            } else {
                titles.iter().map(String::as_str).collect()
            };
            let cand_artists = if artists.is_empty() {
                vec![""]
            } else {
                artists.iter().map(String::as_str).collect()
            };

            let mut best_verdict = None;
            for t in &cand_titles {
                for a in &cand_artists {
                    let v = prepared.evaluate(t, a, 0, None);
                    if best_verdict.as_ref().is_none_or(
                        |cur: &crate::lyrics::providers::MatchVerdict| v.score > cur.score,
                    ) {
                        best_verdict = Some(v);
                    }
                }
            }

            if let Some(verdict) = best_verdict
                && verdict.is_high_match()
            {
                return Ok(vec![FetchedCandidate {
                    lyrics,
                    match_score: verdict.score,
                    is_blind: true,
                    video_id_keyed: false,
                }]);
            }

            Ok(Vec::new())
        })
    }
}
