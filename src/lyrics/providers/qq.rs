//! QQ Music (`y.qq.com` 3DES-ECB + zlib QRC & LRC) provider (`QQProvider.java`).

use crate::lyrics::crypto::{hex_to_bytes, triple_des_ecb_decrypt, zlib_inflate_utf8};
use crate::lyrics::parsers::{extract_credit_metadata, parse_synced};
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, ProviderContext, SOFT_MIN,
    score_lyrics_candidate, score_track_candidate,
};
use crate::lyrics::types::{
    LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo, create_word_timing,
};
use base64::Engine as _;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

const MUSICU_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const SHU6_URL: &str = "https://shu6.y.qq.com/cgi-bin/musicu.fcg";
const WEB_SEARCH_URL: &str = "https://c.y.qq.com/soso/fcgi-bin/search_for_qq_cp";
const QRC_KEY: &str = "!@#)(*$%123ZXC!@!@#)(NHL";

static QRC_XML_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<Lyric_1 LyricType="1" LyricContent="([\s\S]*?)"/>"#).unwrap());
static NUMERIC_ENTITY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#(\d+);").unwrap());
static QRC_LINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\d+),(\d+)](.*)").unwrap());
static QRC_WORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d+),(\d+)\)").unwrap());
static LRC_META_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\w+):([^]]*)]$").unwrap());

pub struct QQProvider;

fn musicu_comm() -> Value {
    json!({
        "ct": "11",
        "cv": "1003006",
        "v": "1003006",
        "os_ver": "15",
        "phonetype": "24122RKC7C",
        "tmeAppID": "qqmusiclight",
        "nettype": "NETWORK_WIFI",
    })
}

fn singers(item: &Value) -> String {
    item.get("singer")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("name").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default()
}

fn decode_xml_entities(input: &str) -> String {
    let dec = NUMERIC_ENTITY_RE.replace_all(input, |caps: &regex::Captures<'_>| {
        caps[1]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string())
            .unwrap_or_else(|| caps[0].to_string())
    });
    dec.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn decode_qq_payload(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if let Some(bytes) = hex_to_bytes(raw)
        && !bytes.is_empty()
        && bytes.len() % 8 == 0
    {
        let decrypted = triple_des_ecb_decrypt(&bytes, QRC_KEY);
        if let Some(inflated) = zlib_inflate_utf8(&decrypted)
            && !inflated.is_empty()
        {
            return inflated;
        }
    }
    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(raw.trim())
        && let Ok(text) = String::from_utf8(decoded)
    {
        return text;
    }
    raw.to_string()
}

fn parse_qrc_format(text: &str) -> Vec<LyricLine> {
    if text.is_empty() {
        return Vec::new();
    }
    let content = if let Some(caps) = QRC_XML_RE.captures(text) {
        decode_xml_entities(&caps[1])
    } else {
        text.to_string()
    };

    let mut lines = Vec::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || LRC_META_RE.is_match(line) {
            continue;
        }
        let Some(caps) = QRC_LINE_RE.captures(line) else {
            continue;
        };
        let (Ok(line_start_ms), Ok(line_dur_ms)) = (caps[1].parse::<i64>(), caps[2].parse::<i64>())
        else {
            continue;
        };
        let line_end_ms = line_start_ms + line_dur_ms;
        let line_content = &caps[3];
        if line_content.contains("//") {
            continue;
        }

        let mut offsets = Vec::new();
        let mut texts = Vec::new();
        let mut prev_end = 0_usize;
        for wm in QRC_WORD_RE.captures_iter(line_content) {
            let m = wm.get(0).unwrap();
            let Ok(off) = wm[1].parse::<i64>() else {
                continue;
            };
            offsets.push(off);
            texts.push(&line_content[prev_end..m.start()]);
            prev_end = m.end();
        }

        let mut words = Vec::new();
        let mut full = String::new();
        for (i, &w_start_ms) in offsets.iter().enumerate() {
            let w_end_ms = offsets.get(i + 1).copied().unwrap_or(line_end_ms);
            let w_text = texts[i];
            if w_text.is_empty() {
                continue;
            }
            let space = w_text.chars().last().is_some_and(char::is_whitespace);
            words.push(
                create_word_timing(
                    w_start_ms as f64 / 1000.0,
                    w_end_ms as f64 / 1000.0,
                    w_text.trim(),
                )
                .with_space(space),
            );
            full.push_str(w_text);
        }
        if words.is_empty() && !line_content.is_empty() {
            let stripped = QRC_WORD_RE.replace_all(line_content, "").trim().to_string();
            full.push_str(&stripped);
        }

        let full_text = full.trim().to_string();
        if full_text.is_empty() {
            continue;
        }
        lines.push(
            LyricLine::new(line_start_ms as f64 / 1000.0, full_text)
                .with_end(Some(line_end_ms as f64 / 1000.0))
                .with_words((!words.is_empty()).then_some(words)),
        );
    }

    lines.sort_by(|a, b| a.time.total_cmp(&b.time));
    lines
}

fn collect_grp_nodes(node: &Value, out: &mut Vec<Value>, seen: &mut HashSet<i64>) {
    let id = node.get("id").and_then(Value::as_i64).unwrap_or(-1);
    if id <= 0 || !seen.insert(id) {
        return;
    }
    out.push(node.clone());
    if let Some(grp) = node.get("grp").and_then(Value::as_array) {
        for child in grp {
            collect_grp_nodes(child, out, seen);
        }
    }
}

fn expand_grp_tree(items: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        collect_grp_nodes(item, &mut out, &mut seen);
    }
    out
}

async fn musicu_search(ctx: &ProviderContext, url: &str, keyword: &str) -> Option<Vec<Value>> {
    let search_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let payload = json!({
        "comm": musicu_comm(),
        "req_0": {
            "module": "music.search.SearchCgiService",
            "method": "DoSearchForQQMusicLite",
            "param": {
                "search_id": search_id,
                "remoteplace": "search.android.keyboard",
                "query": keyword,
                "search_type": 0,
                "num_per_page": 5,
                "page_num": 1,
                "highlight": 0,
                "nqc_flag": 0,
                "page_id": 1,
                "grp": 1,
            }
        }
    });
    let resp = ctx.client().post(url).json(&payload).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    root.get("req_0")?
        .get("data")?
        .get("body")?
        .get("item_song")?
        .as_array()
        .cloned()
}

async fn web_search(ctx: &ProviderContext, keyword: &str) -> Option<Vec<Value>> {
    let url = format!(
        "{WEB_SEARCH_URL}?format=json&aggr=1&w={}&n=5&p=1",
        urlencoding::encode(keyword)
    );
    let resp = ctx
        .client()
        .get(&url)
        .header("Referer", "https://y.qq.com/")
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    let list = root.get("data")?.get("song")?.get("list")?.as_array()?;
    let normalized = list
        .iter()
        .map(|raw| {
            json!({
                "id": raw.get("songid").and_then(Value::as_i64).unwrap_or(0),
                "mid": raw.get("songmid").and_then(Value::as_str).unwrap_or(""),
                "title": raw.get("songname").and_then(Value::as_str).unwrap_or(""),
                "singer": raw.get("singer"),
                "album": {
                    "name": raw.get("albumname").and_then(Value::as_str).unwrap_or(""),
                    "mid": raw.get("albummid").and_then(Value::as_str).unwrap_or(""),
                },
                "interval": raw.get("interval").and_then(Value::as_i64).unwrap_or(0),
                "grp": raw.get("grp"),
            })
        })
        .collect();
    Some(normalized)
}

async fn fetch_from_song(ctx: &ProviderContext, song: &Value) -> Option<Lyrics> {
    let song_id = song.get("id").and_then(Value::as_i64).unwrap_or(0);
    if song_id <= 0 {
        return None;
    }
    let title = song.get("title").and_then(Value::as_str).unwrap_or("");
    let album_name = song
        .get("album")
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let singer_name = singers(song);
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());

    let payload = json!({
        "comm": musicu_comm(),
        "req_0": {
            "module": "music.musichallSong.PlayLyricInfo",
            "method": "GetPlayLyricInfo",
            "param": {
                "songID": song_id,
                "songName": b64(title),
                "albumName": b64(album_name),
                "singerName": b64(&singer_name),
                "crypt": 1,
                "qrc": 1,
                "trans": 1,
                "roma": 1,
                "cv": 2111,
                "ct": 19,
                "lrc_t": 0,
                "qrc_t": 0,
                "roma_t": 0,
                "trans_t": 0,
                "type": 0,
                "interval": song.get("interval").and_then(Value::as_i64).unwrap_or(0),
            }
        }
    });

    let resp = ctx
        .client()
        .post(MUSICU_URL)
        .json(&payload)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    let data = root.get("req_0")?.get("data")?;

    let mut original = decode_qq_payload(data.get("lyric").and_then(Value::as_str).unwrap_or(""));
    if let Some(caps) = QRC_XML_RE.captures(&original) {
        original = decode_xml_entities(&caps[1]);
    }

    let mut offset_ms = 0_i64;
    for raw_line in original.lines() {
        if let Some(caps) = LRC_META_RE.captures(raw_line.trim())
            && caps[1].eq_ignore_ascii_case("offset")
            && let Ok(v) = caps[2].trim().parse::<i64>()
        {
            offset_ms = v;
        }
    }
    let credits = extract_credit_metadata(&original);

    let mut lines = parse_qrc_format(&original);
    if lines.is_empty() {
        lines = parse_synced(&original);
    }
    if lines.is_empty() {
        return None;
    }

    if offset_ms != 0 {
        crate::lyrics::types::shift_lines_timings(&mut lines, offset_ms as f64 / 1000.0);
    }

    let mut l = Lyrics::new(lines, "QQ", true);
    l.credits = credits;
    l.raw_format = Some(original);
    l.format_type = Some("qrc".to_string());
    l.source_url = Some(format!("https://y.qq.com/n/ryqq/songDetail/{song_id}"));
    Some(l)
}

impl LyricsProvider for QQProvider {
    fn id(&self) -> &'static str {
        "qq"
    }

    fn display_name(&self) -> &'static str {
        "QQ"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let keyword = format!("{} {}", track.title, track.artist);
            let mut songs = None;
            for url in [MUSICU_URL, SHU6_URL] {
                if let Some(s) = musicu_search(ctx, url, &keyword).await
                    && !s.is_empty()
                {
                    songs = Some(s);
                    break;
                }
            }
            if songs.is_none() {
                songs = web_search(ctx, &keyword).await.filter(|s| !s.is_empty());
            }
            let Some(raw_songs) = songs else {
                return Ok(Vec::new());
            };

            let expanded = expand_grp_tree(&raw_songs);
            let mut scored: Vec<(i32, Value)> = expanded
                .into_iter()
                .map(|item| {
                    let t = item.get("title").and_then(Value::as_str).unwrap_or("");
                    let a = singers(&item);
                    let d = item.get("interval").and_then(Value::as_i64).unwrap_or(0);
                    let sc =
                        score_track_candidate(t, &a, d, None, track, ctx.custom_regex.as_ref());
                    (sc, item)
                })
                .collect();
            scored.sort_by_key(|a| std::cmp::Reverse(a.0));

            let mut passed: Vec<Value> = scored
                .iter()
                .filter(|(sc, _)| *sc >= SOFT_MIN)
                .map(|(_, v)| v.clone())
                .collect();
            if passed.is_empty()
                && let Some((_, first)) = scored.into_iter().next()
            {
                passed.push(first);
            }

            let mut out = Vec::new();
            for song in passed {
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
                let Some(lyrics) = fetch_from_song(ctx, &song).await else {
                    continue;
                };
                let t = song.get("title").and_then(Value::as_str).unwrap_or("");
                let a = singers(&song);
                let d = song.get("interval").and_then(Value::as_i64).unwrap_or(0);
                let score =
                    score_lyrics_candidate(t, &a, d, &lyrics, track, ctx.custom_regex.as_ref());
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
