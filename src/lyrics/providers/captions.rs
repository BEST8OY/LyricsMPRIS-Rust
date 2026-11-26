//! YouTube closed-captions (`youtubei/v1/player` + `fmt=json3` timedtext) provider (`CaptionsFetcher.java`).

use crate::lyrics::crypto::to_hex;
use crate::lyrics::providers::{
    BoxFuture, CHROME_DESKTOP_UA, FetchedCandidate, LyricsProvider, ProviderContext,
};
use crate::lyrics::types::{
    LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo, WordTiming, create_word_timing,
};
use regex::Regex;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

const INNERTUBE_PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
const TIMEDTEXT_URL: &str = "https://www.youtube.com/api/timedtext";

static BRACKETS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[[^]]*]$").unwrap());
static PARENS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\([^)]*\)$").unwrap());

pub struct CaptionsProvider;

#[derive(Clone, Debug)]
struct CaptionTrack {
    url: String,
    lang_code: String,
    is_asr: bool,
    is_translatable: bool,
    is_translation: bool,
    has_tlang: bool,
}

fn normalize_cookies(raw: &str) -> String {
    let mut val = raw.trim();
    if let Some(rest) = val
        .strip_prefix("Cookie:")
        .or_else(|| val.strip_prefix("cookie:"))
    {
        val = rest.trim();
    }
    if val.len() >= 2
        && ((val.starts_with('"') && val.ends_with('"'))
            || (val.starts_with('\'') && val.ends_with('\'')))
    {
        val = val[1..val.len() - 1].trim();
    }
    val.split([';', '\r', '\n'])
        .map(str::trim)
        .filter(|p| !p.is_empty() && p.contains('='))
        .collect::<Vec<_>>()
        .join("; ")
}

fn extract_cookie_value<'a>(cookies: &'a str, key: &str) -> Option<&'a str> {
    for part in cookies.split(';') {
        let trimmed = part.trim();
        if let Some(val) = trimmed.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            return Some(val);
        }
    }
    None
}

fn compute_sapisid_hash(cookies: &str) -> Option<String> {
    let sapisid = extract_cookie_value(cookies, "SAPISID")
        .or_else(|| extract_cookie_value(cookies, "__Secure-3PAPISID"))
        .filter(|s| !s.is_empty())?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let input = format!("{ts} {sapisid} https://www.youtube.com");
    let mut hasher = Sha1::new();
    hasher.update(input.as_bytes());
    let hex = to_hex(&hasher.finalize());
    Some(format!("SAPISIDHASH {ts}_{hex}"))
}

fn parse_json3(body: &str) -> Vec<LyricLine> {
    let Ok(root) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(events) = root.get("events").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut lines = Vec::new();
    for ev in events {
        if ev.get("aAppend").and_then(Value::as_i64) == Some(1) {
            continue;
        }
        let Some(segs) = ev.get("segs").and_then(Value::as_array) else {
            continue;
        };
        let start_ms = ev.get("tStartMs").and_then(Value::as_i64).unwrap_or(0);

        let mut text_buf = String::new();
        for seg in segs {
            if let Some(u) = seg.get("utf8").and_then(Value::as_str) {
                text_buf.push_str(u);
            }
        }
        let trimmed = text_buf.trim();
        if trimmed.is_empty() || BRACKETS_RE.is_match(trimmed) || PARENS_RE.is_match(trimmed) {
            continue;
        }

        let words = parse_word_timings(ev, segs, start_ms);
        let mut line = LyricLine::new(start_ms as f64 / 1000.0, trimmed);
        if !words.is_empty() {
            line.end = words.last().map(|w| w.end);
            line.words = Some(words);
        }
        lines.push(line);
    }
    lines
}

fn parse_word_timings(ev: &Value, segs: &[Value], start_ms: i64) -> Vec<WordTiming> {
    if segs.len() < 2 {
        return Vec::new();
    }
    let mut raw_words: Vec<(i64, String, bool)> = Vec::with_capacity(segs.len());
    let mut offset_by_word = false;
    for seg in segs {
        let raw = seg.get("utf8").and_then(Value::as_str).unwrap_or("");
        let word = raw.trim();
        if word.is_empty() {
            continue;
        }
        let s = start_ms + seg.get("tOffsetMs").and_then(Value::as_i64).unwrap_or(0);
        offset_by_word |= s > start_ms;
        let ends_space = raw.chars().last().is_some_and(char::is_whitespace);
        raw_words.push((s, word.to_string(), ends_space));
    }
    if !offset_by_word || raw_words.len() < 2 {
        return Vec::new();
    }
    let event_end_ms = start_ms + ev.get("dDurationMs").and_then(Value::as_i64).unwrap_or(0);
    let total = raw_words.len();
    let mut words = Vec::with_capacity(total);
    for i in 0..total {
        let s = raw_words[i].0;
        let e = if i + 1 < total {
            raw_words[i + 1].0
        } else {
            event_end_ms.max(s)
        };
        words.push(
            create_word_timing(s as f64 / 1000.0, e as f64 / 1000.0, &raw_words[i].1)
                .with_space(raw_words[i].2),
        );
    }
    words
}

fn build_caption_url(base_url: &str) -> String {
    let mut u = base_url.to_string();
    if !u.contains("fmt=") {
        u.push_str(if u.contains('?') { "&" } else { "?" });
        u.push_str("fmt=json3");
    }
    u
}

fn select_primary_tracks<'a>(
    tracks: &'a [CaptionTrack],
    source_lang: Option<&str>,
) -> Vec<&'a CaptionTrack> {
    let mut source_manual = None;
    let mut fallback_manual = None;
    let mut fallback_asr = None;
    for t in tracks {
        if t.url.contains("variant=gemini") {
            continue;
        }
        if !t.is_asr {
            if source_lang == Some(t.lang_code.as_str()) {
                if source_manual.is_none() {
                    source_manual = Some(t);
                }
            } else if fallback_manual.is_none() {
                fallback_manual = Some(t);
            }
        } else if fallback_asr.is_none() {
            fallback_asr = Some(t);
        }
    }
    let mut out = Vec::new();
    if let Some(t) = source_manual {
        out.push(t);
    } else if let Some(t) = fallback_manual {
        out.push(t);
    }
    if let Some(t) = fallback_asr {
        out.push(t);
    }
    out
}

async fn fetch_caption_url(ctx: &ProviderContext, url: &str, cookies: &str) -> Option<String> {
    let mut req = ctx
        .client()
        .get(url)
        .header("User-Agent", CHROME_DESKTOP_UA);
    if !cookies.is_empty() {
        req = req.header("Cookie", cookies);
        if let Some(auth) = compute_sapisid_hash(cookies) {
            req = req.header("Authorization", auth);
        }
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().await.ok()
}

impl LyricsProvider for CaptionsProvider {
    fn id(&self) -> &'static str {
        "captions"
    }

    fn display_name(&self) -> &'static str {
        "Captions"
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

            let cookies = ctx
                .settings
                .tokens
                .youtube_cookies
                .as_deref()
                .map(normalize_cookies)
                .unwrap_or_default();

            let body = json!({
                "context": {
                    "client": {
                        "clientName": "WEB",
                        "clientVersion": "2.20250101.00.00",
                        "hl": "en",
                        "gl": "US",
                    }
                },
                "videoId": vid,
                "captionParams": { "captionsEnabled": true },
                "contentCheckOk": true,
                "racyCheckOk": true,
            });

            let mut req = ctx
                .client()
                .post(INNERTUBE_PLAYER_URL)
                .header("Content-Type", "application/json")
                .header("User-Agent", CHROME_DESKTOP_UA)
                .header("Origin", "https://www.youtube.com")
                .header("Referer", "https://www.youtube.com/")
                .header("X-Origin", "https://www.youtube.com")
                .header("X-YouTube-Client-Name", "1")
                .header("X-YouTube-Client-Version", "2.20250101.00.00")
                .header("X-Goog-AuthUser", "0");
            if !cookies.is_empty() {
                req = req.header("Cookie", &cookies);
                if let Some(auth) = compute_sapisid_hash(&cookies) {
                    req = req.header("Authorization", auth);
                }
            }
            let resp = req.json(&body).send().await?;
            let Ok(root) = resp.json::<Value>().await else {
                return Ok(Vec::new());
            };

            let cap_tracks_json = root
                .get("captions")
                .and_then(|c| c.get("playerCaptionsTracklistRenderer"))
                .and_then(|r| r.get("captionTracks"))
                .and_then(Value::as_array);

            let mut out = Vec::new();
            if let Some(arr) = cap_tracks_json {
                let tracks: Vec<CaptionTrack> = arr
                    .iter()
                    .filter_map(|obj| {
                        let url = obj.get("baseUrl").and_then(Value::as_str)?.to_string();
                        let lang_code =
                            obj.get("languageCode").and_then(Value::as_str)?.to_string();
                        let vss_id = obj.get("vssId").and_then(Value::as_str).unwrap_or("");
                        let is_asr = obj.get("kind").and_then(Value::as_str) == Some("asr");
                        let is_translatable = obj
                            .get("isTranslatable")
                            .and_then(Value::as_bool)
                            .unwrap_or(true);
                        let is_translation = vss_id.starts_with('.') || vss_id.starts_with(".a.");
                        let has_tlang = url.contains("&tlang=") || url.contains("?tlang=");
                        Some(CaptionTrack {
                            url,
                            lang_code,
                            is_asr,
                            is_translatable,
                            is_translation,
                            has_tlang,
                        })
                    })
                    .collect();

                let source_lang = tracks
                    .iter()
                    .find(|t| t.is_asr)
                    .or_else(|| tracks.iter().find(|t| !t.is_asr && !t.is_translation))
                    .or_else(|| tracks.iter().find(|t| !t.is_asr && !t.has_tlang))
                    .or_else(|| tracks.iter().find(|t| !t.is_asr && t.is_translatable))
                    .map(|t| t.lang_code.as_str());

                let selected = select_primary_tracks(&tracks, source_lang);
                for (idx, ctrack) in selected.into_iter().enumerate() {
                    if idx >= 2 {
                        break;
                    }
                    let u = build_caption_url(&ctrack.url);
                    let Some(json3) = fetch_caption_url(ctx, &u, &cookies).await else {
                        continue;
                    };
                    let lines = parse_json3(&json3);
                    if lines.is_empty() {
                        continue;
                    }
                    let pname = if ctrack.is_asr {
                        "Captions (ASR)"
                    } else {
                        "Captions"
                    };
                    let mut lyrics = Lyrics::new(lines, pname, true);
                    lyrics.raw_format = Some(json3);
                    lyrics.format_type = Some("json3".to_string());
                    if let Some(c) = FetchedCandidate::video_id(lyrics) {
                        out.push(c);
                    }
                }
            } else {
                let url = format!("{TIMEDTEXT_URL}?lang=en&v={vid}&kind=asr&fmt=json3");
                if let Some(json3) = fetch_caption_url(ctx, &url, &cookies).await {
                    let lines = parse_json3(&json3);
                    if !lines.is_empty() {
                        let mut lyrics = Lyrics::new(lines, "Captions", true);
                        lyrics.raw_format = Some(json3);
                        lyrics.format_type = Some("json3".to_string());
                        if let Some(c) = FetchedCandidate::video_id(lyrics) {
                            out.push(c);
                        }
                    }
                }
            }

            Ok(out)
        })
    }
}
