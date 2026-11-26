//! KuGou (`kugou.com` KRC XOR + zlib) lyrics provider (`KuGouProvider.java`).

use crate::lyrics::crypto::zlib_inflate_utf8;
use crate::lyrics::parsers::{extract_credit_metadata, parse_krc, parse_synced};
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, ProviderContext,
    score_lyrics_candidate, score_track_candidate,
};
use crate::lyrics::types::{LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use base64::Engine as _;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

const SONG_SEARCH_URL: &str =
    "https://mobiles.kugou.com/api/v3/search/song?version=10000&plat=0&correct=1&pagesize=10";
const SEARCH_URL: &str = "https://lyrics.kugou.com/search?ver=1&man=yes&client=mobi&hash=";
const DOWNLOAD_URL: &str = "https://lyrics.kugou.com/download?ver=1&client=pc&fmt=krc&charset=utf8";

const KRC_KEY: [u8; 16] = [
    64, 71, 97, 119, 94, 50, 116, 71, 81, 54, 49, 45, 206, 210, 110, 105,
];

static LRC_META_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\w+):([^]]*)]$").unwrap());

pub struct KuGouProvider;

struct SongInfo {
    hash: String,
    id: String,
    title: String,
    artist: String,
    duration_sec: i64,
}

fn decrypt_krc(raw: &[u8]) -> Option<String> {
    if raw.len() <= 4 {
        return None;
    }
    let decoded: Vec<u8> = raw[4..]
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ KRC_KEY[i % KRC_KEY.len()])
        .collect();
    zlib_inflate_utf8(&decoded)
}

fn apply_file_offset(mut lines: Vec<LyricLine>, offset_secs: f64) -> Vec<LyricLine> {
    crate::lyrics::types::shift_lines_timings(&mut lines, offset_secs);
    lines
}

fn parse_krc_language_tag(tag: &str, original: &[LyricLine]) -> Option<Vec<LyricLine>> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(tag.trim())
        .ok()?;
    let root: Value = serde_json::from_slice(&raw).ok()?;
    let content = root.get("content")?.as_array()?;
    let mut trans_content = None;
    for item in content {
        if item.get("type").and_then(Value::as_i64) == Some(1) {
            trans_content = item.get("lyricContent").and_then(Value::as_array);
        }
    }
    let arr = trans_content?;
    let mut trans = Vec::new();
    for (li, line) in original.iter().enumerate() {
        if let Some(entry) = arr.get(li).and_then(Value::as_array)
            && let Some(text) = entry.first().and_then(Value::as_str)
            && !text.is_empty()
        {
            trans.push(LyricLine::new(line.time, text));
        }
    }
    (!trans.is_empty()).then_some(trans)
}

fn decode_content(content: &str) -> Option<Lyrics> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(content.trim())
        .ok()?;

    if raw.len() > 4 && &raw[..4] == b"krc1" {
        let krc = decrypt_krc(&raw)?;
        let mut offset_secs = 0.0_f64;
        let mut language_tag: Option<String> = None;
        for raw_line in krc.lines() {
            let line = raw_line.trim();
            if let Some(caps) = LRC_META_RE.captures(line) {
                let key = caps[1].to_ascii_lowercase();
                if key == "offset"
                    && let Ok(ms) = caps[2].trim().parse::<i64>()
                {
                    offset_secs = -(ms as f64) / 1000.0;
                } else if key == "language" {
                    language_tag = Some(caps[2].to_string());
                }
            }
        }
        let credits = extract_credit_metadata(&krc);
        let lines = apply_file_offset(parse_krc(&krc), offset_secs);
        if lines.is_empty() {
            return None;
        }
        let mut l = Lyrics::new(lines, "KuGou", true);
        l.credits = credits;
        if let Some(tag) = language_tag
            && let Some(trans) = parse_krc_language_tag(&tag, &l.lines)
        {
            l.translations.insert("zh-Hans".to_string(), trans);
        }
        l.raw_format = Some(krc);
        l.format_type = Some("krc".to_string());
        Some(l)
    } else {
        let text = String::from_utf8(raw).ok()?;
        let credits = extract_credit_metadata(&text);
        let lines = parse_synced(&text);
        if lines.is_empty() {
            return None;
        }
        let mut l = Lyrics::new(lines, "KuGou", true);
        l.credits = credits;
        l.raw_format = Some(text);
        l.format_type = Some("lrc".to_string());
        Some(l)
    }
}

async fn resolve_hash(ctx: &ProviderContext, track: &TrackInfo) -> Option<SongInfo> {
    let keyword = format!("{} {}", track.artist, track.title);
    let url = format!(
        "{SONG_SEARCH_URL}&keyword={}",
        urlencoding::encode(&keyword)
    );
    let resp = ctx.client().get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    let info = root.get("data")?.get("info")?.as_array()?;

    let mut best: Option<(i32, SongInfo)> = None;
    for item in info {
        let hash = item.get("hash").and_then(Value::as_str).unwrap_or("");
        if hash.is_empty() {
            continue;
        }
        let title = item.get("songname").and_then(Value::as_str).unwrap_or("");
        let artist = item.get("singername").and_then(Value::as_str).unwrap_or("");
        let dur = item.get("duration").and_then(Value::as_i64).unwrap_or(0);
        let score =
            score_track_candidate(title, artist, dur, None, track, ctx.custom_regex.as_ref());
        if best.as_ref().is_none_or(|(bs, _)| score > *bs) {
            let id = item
                .get("id")
                .and_then(Value::as_i64)
                .map(|n| n.to_string())
                .or_else(|| item.get("id").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            best = Some((
                score,
                SongInfo {
                    hash: hash.to_string(),
                    id,
                    title: title.to_string(),
                    artist: artist.to_string(),
                    duration_sec: dur,
                },
            ));
        }
    }

    best.and_then(|(score, s)| (score >= crate::lyrics::providers::SOFT_MIN).then_some(s))
}

impl LyricsProvider for KuGouProvider {
    fn id(&self) -> &'static str {
        "kugou"
    }

    fn display_name(&self) -> &'static str {
        "KuGou"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let Some(song) = resolve_hash(ctx, track).await else {
                return Ok(Vec::new());
            };
            let song_id = if song.id.is_empty() {
                song.hash.as_str()
            } else {
                song.id.as_str()
            };
            let source_url = format!("https://www.kugou.com/song/{song_id}.html");

            let search_url = format!("{SEARCH_URL}{}", urlencoding::encode(&song.hash));
            let resp = ctx.client().get(&search_url).send().await?;
            if !resp.status().is_success() {
                return Ok(Vec::new());
            }
            let root: Value = resp.json().await?;
            let Some(cands) = root.get("candidates").and_then(Value::as_array) else {
                return Ok(Vec::new());
            };

            let mut out = Vec::new();
            for cand in cands {
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
                let id = cand.get("id").and_then(Value::as_str).unwrap_or("");
                let key = cand.get("accesskey").and_then(Value::as_str).unwrap_or("");
                if id.is_empty() || key.is_empty() {
                    continue;
                }
                let dl_url = format!(
                    "{DOWNLOAD_URL}&id={}&accesskey={}",
                    urlencoding::encode(id),
                    urlencoding::encode(key)
                );
                let Ok(dl_resp) = ctx.client().get(&dl_url).send().await else {
                    continue;
                };
                if !dl_resp.status().is_success() {
                    continue;
                }
                let Ok(dl_json) = dl_resp.json::<Value>().await else {
                    continue;
                };
                let content = dl_json.get("content").and_then(Value::as_str).unwrap_or("");
                if content.is_empty() {
                    continue;
                }
                let Some(mut lyrics) = decode_content(content) else {
                    continue;
                };
                lyrics.source_url = Some(source_url.clone());
                let score = score_lyrics_candidate(
                    &song.title,
                    &song.artist,
                    song.duration_sec,
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
