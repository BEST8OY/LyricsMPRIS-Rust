//! YouTube Music InnerTube (`next` + `browse` timed/plain lyrics) provider (`YouTubeMusicProvider.java`).

use crate::lyrics::parsers::parse_plain;
use crate::lyrics::providers::{
    BoxFuture, CHROME_DESKTOP_UA, FetchedCandidate, LyricsProvider, ProviderContext,
};
use crate::lyrics::types::{LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use serde_json::{Value, json};

const INNERTUBE_KEY: &str = "AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30";
const WEB_REMIX_VERSION: &str = "1.20260914.01.00";
const ANDROID_MUSIC_VERSION: &str = "7.21.50";

pub struct YouTubeMusicProvider;

fn build_context(client_name: &str, client_version: &str) -> Value {
    json!({
        "client": {
            "clientName": client_name,
            "clientVersion": client_version,
            "hl": "en",
        },
        "user": {},
    })
}

async fn post_innertube(
    ctx: &ProviderContext,
    url: &str,
    body: &Value,
    client_version: &str,
) -> Option<Value> {
    let resp = ctx
        .client()
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", CHROME_DESKTOP_UA)
        .header("X-YouTube-Client-Version", client_version)
        .header("Accept", "*/*")
        .header("Cookie", "SOCS=CAI")
        .header("Origin", "https://music.youtube.com")
        .json(body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

fn extract_browse_id(root: &Value) -> Option<String> {
    let tabs = root
        .get("contents")?
        .get("singleColumnMusicWatchNextResultsRenderer")?
        .get("tabbedRenderer")?
        .get("watchNextTabbedResultsRenderer")?
        .get("tabs")?
        .as_array()?;

    for tab in tabs {
        let Some(endpoint) = tab
            .get("tabRenderer")
            .and_then(|t| t.get("endpoint"))
            .and_then(|e| e.get("browseEndpoint"))
        else {
            continue;
        };
        let page_type = endpoint
            .get("browseEndpointContextSupportedConfigs")
            .and_then(|c| c.get("browseEndpointContextMusicConfig"))
            .and_then(|m| m.get("pageType"))
            .and_then(Value::as_str);
        if page_type == Some("MUSIC_PAGE_TYPE_TRACK_LYRICS")
            && let Some(id) = endpoint.get("browseId").and_then(Value::as_str)
        {
            return Some(id.to_string());
        }
    }
    None
}

fn is_instrumental_marker(text: &str) -> bool {
    let t = text.trim();
    t == "♪"
        || t == "♫"
        || t == "🎵"
        || t == "[Instrumental]"
        || t == "(Instrumental)"
        || t == "..."
}

fn parse_timed_lyrics(arr: &[Value]) -> Vec<LyricLine> {
    let mut lines: Vec<LyricLine> = Vec::new();
    for entry in arr {
        let text = entry
            .get("lyricLine")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let cue = entry.get("cueRange");
        let start_ms = cue
            .and_then(|c| c.get("startTimeMilliseconds"))
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<i64>().ok());
        let end_ms = cue
            .and_then(|c| c.get("endTimeMilliseconds"))
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<i64>().ok());

        if is_instrumental_marker(text) {
            if let Some(start) = start_ms
                && let Some(last) = lines.last_mut()
                && last.end.is_none()
            {
                let start_s = start as f64 / 1000.0;
                if start_s > last.time {
                    last.end = Some(start_s);
                }
            }
            continue;
        }
        let start_s = start_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(-1.0);
        let mut line = LyricLine::new(start_s, text);
        line.end = end_ms.map(|ms| ms as f64 / 1000.0);
        lines.push(line);
    }
    lines
}

fn extract_source_provider(lyrics_data: Option<&Value>, root: &Value) -> Option<String> {
    let parse_msg = |msg: &str| -> Option<String> {
        msg.strip_prefix("Source: ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let Some(msg) = lyrics_data
        .and_then(|d| d.get("sourceMessage"))
        .and_then(Value::as_str)
        && let Some(p) = parse_msg(msg)
    {
        return Some(p);
    }
    let footer_text = root
        .get("contents")?
        .get("sectionListRenderer")?
        .get("contents")?
        .as_array()?
        .first()?
        .get("musicDescriptionShelfRenderer")?
        .get("footer")?
        .get("runs")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()?;
    parse_msg(footer_text)
}

fn parse_lyrics_response(root: &Value) -> Option<Lyrics> {
    let lyrics_data = root
        .get("contents")
        .and_then(|c| c.get("elementRenderer"))
        .and_then(|e| e.get("newElement"))
        .and_then(|n| n.get("type"))
        .and_then(|t| t.get("componentType"))
        .and_then(|c| c.get("model"))
        .and_then(|m| m.get("timedLyricsModel"))
        .and_then(|t| t.get("lyricsData"));

    let via = extract_source_provider(lyrics_data, root);
    let source_tag = via
        .map(|p| format!("YouTube Music (via {p})"))
        .unwrap_or_else(|| "YouTube Music".to_string());

    if let Some(ld) = lyrics_data {
        if let Some(timed) = ld.get("timedLyricsData").and_then(Value::as_array)
            && !timed.is_empty()
        {
            let lines = parse_timed_lyrics(timed);
            if !lines.is_empty() {
                if lines.iter().any(|l| l.time >= 0.0) {
                    let fixed = lines
                        .into_iter()
                        .filter(|l| l.time >= 0.0)
                        .collect::<Vec<_>>();
                    if !fixed.is_empty() {
                        let mut l = Lyrics::new(fixed, source_tag, true);
                        l.raw_format = serde_json::to_string(timed).ok();
                        l.format_type = Some("ytm.json".to_string());
                        return Some(l);
                    }
                } else {
                    let plain: String = lines
                        .iter()
                        .map(|l| l.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let p_lines = parse_plain(&plain);
                    if !p_lines.is_empty() {
                        let mut l = Lyrics::new(p_lines, source_tag, false);
                        l.raw_format = Some(plain);
                        l.format_type = Some("txt".to_string());
                        return Some(l);
                    }
                }
            }
        }
        let inst = ld.get("lyrics").and_then(Value::as_str).unwrap_or("");
        if inst.contains("[Instrumental]") || inst.contains('♪') {
            return None;
        }
    }

    if let Some(contents) = root
        .get("contents")
        .and_then(|c| c.get("sectionListRenderer"))
        .and_then(|s| s.get("contents"))
        .and_then(Value::as_array)
    {
        for shelf in contents {
            if let Some(runs) = shelf
                .get("musicDescriptionShelfRenderer")
                .and_then(|m| m.get("description"))
                .and_then(|d| d.get("runs"))
                .and_then(Value::as_array)
            {
                let plain: String = runs
                    .iter()
                    .filter_map(|r| r.get("text").and_then(Value::as_str))
                    .collect();
                if !plain.trim().is_empty() {
                    let p_lines = parse_plain(&plain);
                    if !p_lines.is_empty() {
                        let mut l = Lyrics::new(p_lines, source_tag, false);
                        l.raw_format = Some(plain);
                        l.format_type = Some("txt".to_string());
                        return Some(l);
                    }
                }
            }
        }
    }

    None
}

async fn fetch_browse(ctx: &ProviderContext, browse_id: &str, android: bool) -> Option<Lyrics> {
    let url = format!("https://music.youtube.com/youtubei/v1/browse?key={INNERTUBE_KEY}&alt=json");
    let ver = if android {
        ANDROID_MUSIC_VERSION
    } else {
        WEB_REMIX_VERSION
    };
    let cname = if android {
        "ANDROID_MUSIC"
    } else {
        "WEB_REMIX"
    };
    let body = json!({
        "context": build_context(cname, ver),
        "browseId": browse_id,
    });
    let resp = post_innertube(ctx, &url, &body, ver).await?;
    parse_lyrics_response(&resp)
}

impl LyricsProvider for YouTubeMusicProvider {
    fn id(&self) -> &'static str {
        "ytmusic"
    }

    fn display_name(&self) -> &'static str {
        "YTMusic"
    }

    fn is_configured(&self, _ctx: &ProviderContext) -> bool {
        true
    }

    fn fetch_candidates<'a>(
        &'a self,
        _track: &'a TrackInfo,
        ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let Some(vid) = ids
                .youtube_video_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Ok(Vec::new());
            };

            let next_url =
                format!("https://music.youtube.com/youtubei/v1/next?key={INNERTUBE_KEY}&alt=json");
            let body = json!({
                "context": build_context("WEB_REMIX", WEB_REMIX_VERSION),
                "videoId": vid,
                "playlistId": format!("RDAMVM{vid}"),
            });
            let Some(next_json) = post_innertube(ctx, &next_url, &body, WEB_REMIX_VERSION).await
            else {
                return Ok(Vec::new());
            };
            let Some(browse_id) = extract_browse_id(&next_json) else {
                return Ok(Vec::new());
            };

            if let Some(plain_or_synced) = fetch_browse(ctx, &browse_id, false).await {
                if plain_or_synced.synced {
                    return Ok(FetchedCandidate::video_id(plain_or_synced)
                        .into_iter()
                        .collect());
                }
                if let Some(timed) = fetch_browse(ctx, &browse_id, true).await
                    && timed.synced
                {
                    return Ok(FetchedCandidate::video_id(timed).into_iter().collect());
                }
                return Ok(FetchedCandidate::video_id(plain_or_synced)
                    .into_iter()
                    .collect());
            }

            if let Some(android) = fetch_browse(ctx, &browse_id, true).await {
                return Ok(FetchedCandidate::video_id(android).into_iter().collect());
            }

            Ok(Vec::new())
        })
    }
}
