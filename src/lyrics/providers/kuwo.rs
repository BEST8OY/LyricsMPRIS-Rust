//! Kuwo (`kuwo.cn` XOR + zlib + GB18030 word LRC & mobile fallback) provider (`KuwoProvider.java`).

use crate::lyrics::crypto::zlib_inflate_bytes;
use crate::lyrics::parsers::extract_credit_metadata;
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, ProviderContext, SOFT_MIN,
    score_lyrics_candidate, score_track_candidate,
};
use crate::lyrics::types::{
    LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo, WordTiming, create_word_timing,
};
use base64::Engine as _;
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;

const SEARCH_URL: &str = "https://search.kuwo.cn/r.s";
const LYRIC_URL: &str = "https://newlyric.kuwo.cn/newlyric.lrc";
const MOBILE_LYRIC_URL: &str = "https://m.kuwo.cn/newh5/singles/songinfoandlrc";
const KUWO_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const XOR_KEY: &[u8] = b"yeelion";

static TIME_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(\d{2}):(\d{2})\.(\d{2,3})](.*)$").unwrap());
static TAG_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[([A-Za-z]+):([^]]*)]$").unwrap());
static WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(-?\d+),(-?\d+)>([^<]*)").unwrap());
static SCALE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[kuwo:(\d+)]").unwrap());
static HTML_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());
static HAN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\u{4E00}-\u{9FFF}]").unwrap());

pub struct KuwoProvider;

fn xor_yeelion(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ XOR_KEY[i % XOR_KEY.len()])
        .collect()
}

fn build_lyric_params(params: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(xor_yeelion(params.as_bytes()))
}

fn decrypt_lyrics(raw: &[u8]) -> Option<String> {
    if raw.len() < 10 || !raw[..10].eq_ignore_ascii_case(b"tp=content") {
        return None;
    }
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)?;
    let inflated_bytes = zlib_inflate_bytes(&raw[header_end..])?;
    let inflated_str = std::str::from_utf8(&inflated_bytes).ok()?.trim();
    if inflated_str.is_empty() {
        return None;
    }
    let payload = base64::engine::general_purpose::STANDARD
        .decode(inflated_str)
        .ok()?;
    if payload.is_empty() {
        return None;
    }
    let plain = xor_yeelion(&payload);
    let (decoded, _, _) = encoding_rs::GB18030.decode(&plain);
    Some(decoded.into_owned())
}

fn parse_scale(text: &str) -> (i64, i64) {
    let Some(caps) = SCALE_RE.captures(text) else {
        return (1, 1);
    };
    let Ok(val) = i64::from_str_radix(&caps[1], 8) else {
        return (1, 1);
    };
    let first = val / 10;
    let second = val % 10;
    if first == 0 || second == 0 {
        (1, 1)
    } else {
        (first, second)
    }
}

fn parse_kuwo_lrc(text: &str, source_url: &str) -> Option<Lyrics> {
    let scale = parse_scale(text);
    let mut writer: Option<String> = None;
    let mut file_offset_ms = 0_i64;

    struct RawLine {
        start_ms: i64,
        text: String,
        words: Vec<WordTiming>,
    }

    let mut raw_lines: Vec<RawLine> = Vec::new();
    let mut trans_lines: Vec<(i64, String)> = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(caps) = TAG_LINE_RE.captures(line) {
            let key = &caps[1];
            let val = caps[2].trim();
            if key.eq_ignore_ascii_case("offset") {
                if let Ok(ms) = val.parse::<i64>() {
                    file_offset_ms = -ms;
                }
            } else if (key.eq_ignore_ascii_case("by") || key.eq_ignore_ascii_case("lyricist"))
                && !val.is_empty()
                && writer.is_none()
            {
                writer = Some(val.to_string());
            }
            continue;
        }

        let Some(caps) = TIME_LINE_RE.captures(line) else {
            continue;
        };
        let mm: i64 = caps[1].parse().unwrap_or(0);
        let ss: i64 = caps[2].parse().unwrap_or(0);
        let frac = &caps[3];
        let millis: i64 = if frac.len() == 2 {
            frac.parse::<i64>().unwrap_or(0) * 10
        } else {
            frac.parse::<i64>().unwrap_or(0)
        };
        let start_ms = mm * 60_000 + ss * 1000 + millis;
        let content = &caps[4];
        let plain = HTML_TAG_RE.replace_all(content, "").trim().to_string();
        if plain.is_empty() {
            continue;
        }

        if content.trim_start().starts_with("<0,0>") && HAN_RE.is_match(content) {
            trans_lines.push((start_ms, plain));
            continue;
        }

        let mut words = Vec::new();
        for wm in WORD_RE.captures_iter(content) {
            let (Ok(p1), Ok(p2)) = (wm[1].parse::<i64>(), wm[2].parse::<i64>()) else {
                continue;
            };
            let w_text = &wm[3];
            if w_text.is_empty() {
                continue;
            }
            let word_start_ms = start_ms + ((p1 + p2) / (scale.0 * 2)).abs();
            let dur_ms = ((p1 - p2) / (scale.1 * 2)).abs().max(80);
            let word_end_ms = word_start_ms + dur_ms;
            words.push(create_word_timing(
                word_start_ms as f64 / 1000.0,
                word_end_ms as f64 / 1000.0,
                w_text,
            ));
        }

        raw_lines.push(RawLine {
            start_ms,
            text: plain,
            words,
        });
    }

    raw_lines.sort_by_key(|l| l.start_ms);

    let mut deduped: Vec<RawLine> = Vec::with_capacity(raw_lines.len());
    let mut seen = HashSet::new();
    for r in raw_lines {
        let key = format!("{}|{}", r.start_ms, r.text);
        if seen.insert(key) {
            deduped.push(r);
        }
    }

    let mut lines: Vec<LyricLine> = Vec::with_capacity(deduped.len());
    for (i, cur) in deduped.iter().enumerate() {
        let mut end_ms = if let Some(last_w) = cur.words.last() {
            (last_w.end * 1000.0).round() as i64
        } else {
            let next_start = deduped
                .get(i + 1)
                .map(|n| n.start_ms)
                .unwrap_or(cur.start_ms + 4000);
            if next_start > cur.start_ms + 200 {
                next_start - 10
            } else {
                cur.start_ms + 2000
            }
        };
        if end_ms <= cur.start_ms {
            end_ms = cur.start_ms + 200;
        }
        let s = (cur.start_ms + file_offset_ms).max(0) as f64 / 1000.0;
        let e = (end_ms + file_offset_ms).max(0) as f64 / 1000.0;
        let offset_secs = file_offset_ms as f64 / 1000.0;
        let shifted_words: Vec<WordTiming> = cur
            .words
            .iter()
            .map(|w| {
                let mut sw = w.clone();
                sw.start = (sw.start + offset_secs).max(0.0);
                sw.end = (sw.end + offset_secs).max(0.0);
                for gt in &mut sw.grapheme_times {
                    *gt = (*gt + offset_secs).max(0.0);
                }
                sw
            })
            .collect();

        lines.push(
            LyricLine::new(s, cur.text.clone())
                .with_end(Some(e))
                .with_words((!shifted_words.is_empty()).then_some(shifted_words)),
        );
    }

    if lines.is_empty() {
        return None;
    }

    let mut translated: Vec<LyricLine> = Vec::new();
    for (t_ms, t_text) in trans_lines {
        let ts = (t_ms + file_offset_ms).max(0) as f64 / 1000.0;
        if let Some(target) = lines.iter().rev().find(|l| l.time <= ts + 0.03) {
            if translated
                .last()
                .is_some_and(|prev| (prev.time - target.time).abs() < 0.001)
            {
                let last = translated.last_mut().unwrap();
                last.text = t_text;
            } else {
                let mut tl = LyricLine::new(target.time, t_text);
                tl.end = target.end;
                translated.push(tl);
            }
        }
    }

    let mut l = Lyrics::new(lines, "Kuwo", true);
    l.credits = extract_credit_metadata(text);
    l.songwriters = writer.into_iter().collect();
    if !translated.is_empty() {
        l.translations.insert("default".to_string(), translated);
    }
    l.raw_format = Some(text.to_string());
    l.format_type = Some("kw".to_string());
    l.source_url = Some(source_url.to_string());
    Some(l)
}

fn parse_mobile_lrc(lrclist: &[Value], source_url: &str) -> Option<Lyrics> {
    let mut items: Vec<(i64, String)> = Vec::new();
    for item in lrclist {
        let time_s = item
            .get("time")
            .and_then(|v| {
                v.as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .or_else(|| v.as_f64())
            })
            .filter(|t| t.is_finite());
        let Some(secs) = time_s else {
            continue;
        };
        let raw_txt = item
            .get("lineLyric")
            .or_else(|| item.get("lyric"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let clean = HTML_TAG_RE.replace_all(raw_txt, "").trim().to_string();
        if !clean.is_empty() {
            items.push(((secs * 1000.0).round() as i64, clean));
        }
    }
    if items.is_empty() {
        return None;
    }
    items.sort_by_key(|(t, _)| *t);

    let mut lines = Vec::with_capacity(items.len());
    for (i, (start_ms, txt)) in items.iter().enumerate() {
        let mut end_ms = items
            .get(i + 1)
            .map(|(t, _)| *t)
            .unwrap_or(*start_ms + 4000);
        if end_ms <= *start_ms {
            end_ms = *start_ms + 500;
        }
        lines.push(
            LyricLine::new(*start_ms as f64 / 1000.0, txt.clone())
                .with_end(Some(end_ms as f64 / 1000.0)),
        );
    }
    let mut l = Lyrics::new(lines, "Kuwo", true);
    l.format_type = Some("lrc".to_string());
    l.source_url = Some(source_url.to_string());
    Some(l)
}

async fn fetch_lyrics(ctx: &ProviderContext, music_id: &str) -> Option<Lyrics> {
    let source_url = format!("https://www.kuwo.cn/play_detail/{music_id}");
    let params =
        format!("user=12345,web,web,web&requester=localhost&req=1&rid=MUSIC_{music_id}&lrcx=1");
    let url = format!("{LYRIC_URL}?{}", build_lyric_params(&params));
    if let Ok(resp) = ctx
        .client()
        .get(&url)
        .header("User-Agent", KUWO_UA)
        .header("Referer", "https://www.kuwo.cn/")
        .send()
        .await
        && resp.status().is_success()
        && let Ok(bytes) = resp.bytes().await
        && let Some(decrypted) = decrypt_lyrics(&bytes)
        && let Some(l) = parse_kuwo_lrc(&decrypted, &source_url)
    {
        return Some(l);
    }

    let mob_url = format!(
        "{MOBILE_LYRIC_URL}?musicId={}",
        urlencoding::encode(music_id)
    );
    let resp = ctx
        .client()
        .get(&mob_url)
        .header("User-Agent", KUWO_UA)
        .header("Accept", "application/json")
        .header("Referer", "https://www.kuwo.cn/")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    let lrclist = root.get("data")?.get("lrclist")?.as_array()?;
    parse_mobile_lrc(lrclist, &source_url)
}

impl LyricsProvider for KuwoProvider {
    fn id(&self) -> &'static str {
        "kuwo"
    }

    fn display_name(&self) -> &'static str {
        "Kuwo"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            if track.title.is_empty() || track.artist.is_empty() {
                return Ok(Vec::new());
            }

            let keyword = format!("{} {}", track.artist, track.title);
            let url = format!(
                "{SEARCH_URL}?all={}&ft=music&rformat=json&encoding=utf8&pcjson=1&rn=20&pn=0",
                urlencoding::encode(&keyword)
            );
            let resp = ctx
                .client()
                .get(&url)
                .header("User-Agent", KUWO_UA)
                .header("Accept", "application/json, text/plain, */*")
                .header("Referer", "https://www.kuwo.cn/")
                .send()
                .await?;
            if !resp.status().is_success() {
                return Ok(Vec::new());
            }
            let text = resp.text().await?;
            let Ok(root) = serde_json::from_str::<Value>(&text) else {
                return Ok(Vec::new());
            };
            let Some(abslist) = root.get("abslist").and_then(Value::as_array) else {
                return Ok(Vec::new());
            };

            struct Cand {
                music_id: String,
                title: String,
                artist: String,
                duration: i64,
                score: i32,
            }

            let mut seen = HashSet::new();
            let mut cands = Vec::new();
            for item in abslist {
                let raw_id = item
                    .get("DC_TARGETID")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| item.get("MUSICRID").and_then(Value::as_str))
                    .unwrap_or("");
                let music_id: String = raw_id.chars().filter(|c| c.is_ascii_digit()).collect();
                if music_id.is_empty() || !seen.insert(music_id.clone()) {
                    continue;
                }
                let title = item
                    .get("SONGNAME")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| item.get("NAME").and_then(Value::as_str))
                    .unwrap_or("");
                let artist = item.get("ARTIST").and_then(Value::as_str).unwrap_or("");
                let dur = item
                    .get("DURATION")
                    .and_then(|v| {
                        v.as_i64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .unwrap_or(0);
                let score = score_track_candidate(
                    title,
                    artist,
                    dur,
                    None,
                    track,
                    ctx.custom_regex.as_ref(),
                );
                if score < SOFT_MIN {
                    continue;
                }
                cands.push(Cand {
                    music_id,
                    title: title.to_string(),
                    artist: artist.to_string(),
                    duration: dur,
                    score,
                });
            }

            cands.sort_by_key(|a| std::cmp::Reverse(a.score));
            cands.truncate(MAX_CANDIDATES);

            let mut out = Vec::new();
            for c in cands {
                let Some(lyrics) = fetch_lyrics(ctx, &c.music_id).await else {
                    continue;
                };
                let score = score_lyrics_candidate(
                    &c.title,
                    &c.artist,
                    c.duration,
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

            out.sort_by(|a, b| {
                b.lyrics
                    .sync_rank()
                    .cmp(&a.lyrics.sync_rank())
                    .then_with(|| b.match_score.cmp(&a.match_score))
            });
            out.truncate(MAX_CANDIDATES);
            Ok(out)
        })
    }
}
