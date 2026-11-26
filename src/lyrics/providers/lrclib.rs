//! LRCLIB (`lrclib.net`) provider (`LRCLIBProvider.java`).

use crate::lyrics::parsers::{parse_lyrics_file, parse_plain, parse_synced_with_credits};
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, ProviderContext, SOFT_MIN,
    score_lyrics_candidate, score_track_candidate,
};
use crate::lyrics::types::{Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://lrclib.net/api";
const APP_ID: &str = "HLyf7QYkIJfl4wcObeD-caHjBZPnGhv0Vuvx2UiIFKk";

pub struct LrcLibProvider;

fn base_url(ctx: &ProviderContext) -> String {
    crate::lyrics::providers::endpoint_url(
        ctx.settings.endpoints.lrclib.as_deref(),
        DEFAULT_BASE_URL,
    )
}

fn to_lyrics(response: &Value) -> Option<Lyrics> {
    if response
        .get("instrumental")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    let id = response.get("id").and_then(Value::as_i64).unwrap_or(-1);
    let source_url = (id > 0).then(|| format!("https://lrclib.net/tracks/{id}"));

    let lyrics_file = response
        .get("lyricsFile")
        .or_else(|| response.get("lyricsfile"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(yaml) = lyrics_file
        && let Some(mut from_file) = parse_lyrics_file(yaml)
        && !from_file.is_empty()
    {
        from_file.provider_name = "LRCLIB".to_string();
        if from_file.source_url.is_none() {
            from_file.source_url = source_url;
        }
        return Some(from_file);
    }

    if let Some(synced) = response
        .get("syncedLyrics")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        let (lines, credits) = parse_synced_with_credits(synced);
        if !lines.is_empty() {
            let mut l = Lyrics::new(lines, "LRCLIB", true);
            l.credits = credits;
            l.raw_format = Some(synced.to_string());
            l.format_type = Some("lrc".to_string());
            l.source_url = source_url;
            return Some(l);
        }
    }

    if let Some(plain) = response
        .get("plainLyrics")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        let lines = parse_plain(plain);
        if !lines.is_empty() {
            let mut l = Lyrics::new(lines, "LRCLIB", false);
            l.raw_format = Some(plain.to_string());
            l.format_type = Some("lrc".to_string());
            l.source_url = source_url;
            return Some(l);
        }
    }

    None
}

async fn fetch_exact(
    ctx: &ProviderContext,
    base: &str,
    track: &TrackInfo,
) -> Option<FetchedCandidate> {
    let mut url = format!(
        "{base}/get?track_name={}&artist_name={}",
        urlencoding::encode(&track.title),
        urlencoding::encode(&track.artist)
    );
    if !track.album.is_empty() {
        url.push_str("&album_name=");
        url.push_str(&urlencoding::encode(&track.album));
    }
    if track.duration_secs > 0 {
        url.push_str(&format!("&duration={}", track.duration_secs));
    }

    let resp = ctx
        .client()
        .get(&url)
        .header("Lrclib-App-Id", APP_ID)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: Value = resp.json().await.ok()?;
    let lyrics = to_lyrics(&json)?;
    let cand_title = json.get("trackName").and_then(Value::as_str).unwrap_or("");
    let cand_artist = json.get("artistName").and_then(Value::as_str).unwrap_or("");
    let cand_dur = json
        .get("duration")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .round() as i64;
    FetchedCandidate::with_metadata(lyrics, cand_title, cand_artist, cand_dur, None, track, ctx)
}

impl LyricsProvider for LrcLibProvider {
    fn id(&self) -> &'static str {
        "lrclib"
    }

    fn display_name(&self) -> &'static str {
        "LRCLIB"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let base = base_url(ctx);
            let mut scored: Vec<FetchedCandidate> = Vec::new();

            if let Some(exact) = fetch_exact(ctx, &base, track).await {
                scored.push(exact);
            }

            let search_url = format!(
                "{base}/search?track_name={}&artist_name={}",
                urlencoding::encode(&track.title),
                urlencoding::encode(&track.artist)
            );
            let resp = ctx
                .client()
                .get(&search_url)
                .header("Lrclib-App-Id", APP_ID)
                .send()
                .await?;
            if resp.status().is_success()
                && let Ok(Value::Array(items)) = resp.json::<Value>().await
            {
                let mut raw_hits: Vec<(i32, i64, &Value)> = items
                    .iter()
                    .map(|item| {
                        let t = item.get("trackName").and_then(Value::as_str).unwrap_or("");
                        let a = item.get("artistName").and_then(Value::as_str).unwrap_or("");
                        let d = item
                            .get("duration")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0)
                            .round() as i64;
                        let sc =
                            score_track_candidate(t, a, d, None, track, ctx.custom_regex.as_ref());
                        (sc, d, item)
                    })
                    .collect();

                raw_hits.sort_by(|a, b| {
                    b.0.cmp(&a.0).then_with(|| {
                        if track.duration_secs <= 0 {
                            std::cmp::Ordering::Equal
                        } else {
                            (a.1 - track.duration_secs)
                                .abs()
                                .cmp(&(b.1 - track.duration_secs).abs())
                        }
                    })
                });

                let mut gated: Vec<&Value> = raw_hits
                    .iter()
                    .filter(|(sc, _, _)| *sc >= SOFT_MIN)
                    .map(|(_, _, item)| *item)
                    .collect();
                if gated.is_empty()
                    && let Some((_, _, first)) = raw_hits.first()
                {
                    gated.push(*first);
                }

                for item in gated {
                    let Some(lyrics) = to_lyrics(item) else {
                        continue;
                    };
                    if lyrics.is_empty() {
                        continue;
                    }
                    let t = item.get("trackName").and_then(Value::as_str).unwrap_or("");
                    let a = item.get("artistName").and_then(Value::as_str).unwrap_or("");
                    let d = item
                        .get("duration")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
                        .round() as i64;
                    let score =
                        score_lyrics_candidate(t, a, d, &lyrics, track, ctx.custom_regex.as_ref());
                    let track_score = score - lyrics.sync_rank();
                    if track_score >= SOFT_MIN {
                        scored.push(FetchedCandidate {
                            lyrics,
                            match_score: score,
                            is_blind: false,
                            video_id_keyed: false,
                        });
                    }
                }
            }

            // Order synced ranks above plain, highest score first within each rank
            scored.sort_by(|a, b| {
                b.lyrics
                    .sync_rank()
                    .cmp(&a.lyrics.sync_rank())
                    .then_with(|| b.match_score.cmp(&a.match_score))
            });
            scored.truncate(MAX_CANDIDATES);
            Ok(scored)
        })
    }
}
