//! PetitLyrics (`p0.petitlyrics.com`) WSY word XML / LSY XOR binary / LRC provider (`PetitLyricsProvider.java`).

use crate::lyrics::filter::is_cjk_ideograph;
use crate::lyrics::parsers::lrc::mark_echoes_as_background;
use crate::lyrics::parsers::{parse_plain, parse_synced_with_credits};
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, ProviderContext, throttle_ms,
};
use crate::lyrics::types::{
    LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo, WordTiming, create_word_timing,
};
use base64::Engine as _;
use quick_xml::Reader;
use quick_xml::events::Event;
use regex::Regex;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;

const API_URL: &str = "https://p0.petitlyrics.com/api/GetPetitLyricsData.php";
const CLIENT_APP_ID: &str = "p1110417";
const TERMINAL_TYPE: &str = "10";
const SOURCE_URL_PREFIX: &str = "https://petitlyrics.com/lyrics/";
const THROTTLE_MS: u64 = 500;
static LAST_REQ_MS: AtomicU64 = AtomicU64::new(0);

const TIER_UNSYNCED: i32 = 1;
const TIER_LINE_SYNC: i32 = 2;
const TIER_WORD_SYNC: i32 = 3;

const LSY_KEY_SWITCH_FLAG_OFF: usize = 0x19;
const LSY_PROTECTION_ID_OFF: usize = 0x1a;
const LSY_LINE_COUNT_OFF: usize = 0x38;
const LSY_LINE_LENGTH_OFF: usize = 0x42;
const LSY_PAYLOAD_START: usize = 0xcc;
const LSY_MAX_LINES: usize = 10_000;
const LSY_WRAP_CS: i64 = 65_536;

static TITLE_SEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[—–／〜]\s*|\s+[-/~]\s+").unwrap());

pub struct PetitLyricsProvider;

#[derive(Clone)]
struct Song {
    lyrics_id: String,
    title: String,
    artist: String,
    writer: String,
    composer: String,
    duration_ms: i64,
    lyrics_data: String,
}

#[derive(Clone)]
struct RawToken {
    text: String,
    start_ms: i64,
    end_ms: i64,
}

fn clean_title(raw: &str) -> String {
    let parts: Vec<&str> = TITLE_SEP_RE.split(raw.trim()).collect();
    if parts.len() < 2 {
        return raw.to_string();
    }
    let mut cleaned = parts[0].trim().to_string();
    for tail in &parts[1..] {
        let t = tail.trim();
        let bracketed =
            (t.starts_with('(') && t.ends_with(')')) || (t.starts_with('[') && t.ends_with(']'));
        let pure_ascii = t.bytes().all(|b| (0x20..=0x7E).contains(&b));
        if !t.is_empty() && !bracketed && !pure_ascii {
            cleaned.push_str(" - ");
            cleaned.push_str(t);
        }
    }
    let out = cleaned.trim().to_string();
    if out.is_empty() { raw.to_string() } else { out }
}

fn contains_unspaced_script(s: &str) -> bool {
    s.chars().any(|c| {
        is_cjk_ideograph(c)
            || matches!(
                c as u32,
                0x3040..=0x30FF | 0x31F0..=0x31FF | 0xAC00..=0xD7AF | 0x0E00..=0x0E7F
            )
    })
}

fn classify_payload(payload: &[u8]) -> i32 {
    if payload.starts_with(b"<wsy>") || payload.starts_with(b"<?xml") {
        TIER_WORD_SYNC
    } else if payload.len() >= LSY_PAYLOAD_START
        && (&payload[..4] == b"lsy\0" || decode_lsy_timings(payload).is_some_and(|v| !v.is_empty()))
    {
        TIER_LINE_SYNC
    } else {
        TIER_UNSYNCED
    }
}

fn derive_lsy_key(protection_id: u16, switch_flag: bool) -> u16 {
    if !switch_flag {
        return protection_id;
    }
    (protection_id & 0x0003)
        | ((protection_id & 0x000c) << 2)
        | ((protection_id & 0x0030) >> 2)
        | ((protection_id & 0x00c0) << 2)
        | ((protection_id & 0x0300) >> 2)
        | ((protection_id & 0x0c00) << 2)
        | ((protection_id & 0x3000) >> 2)
        | (protection_id & 0xc000)
}

fn decode_lsy_timings(raw: &[u8]) -> Option<Vec<i64>> {
    if raw.len() < LSY_PAYLOAD_START {
        return None;
    }
    let line_count = i32::from_le_bytes(
        raw[LSY_LINE_COUNT_OFF..LSY_LINE_COUNT_OFF + 4]
            .try_into()
            .ok()?,
    ) as usize;
    if line_count == 0 || line_count > LSY_MAX_LINES {
        return None;
    }
    let end = LSY_PAYLOAD_START + 2 * line_count;
    if raw.len() < end {
        return None;
    }
    let line_len = u16::from_le_bytes(
        raw[LSY_LINE_LENGTH_OFF..LSY_LINE_LENGTH_OFF + 2]
            .try_into()
            .ok()?,
    ) as usize;
    if line_len > 0 && raw.len() != end + line_len * line_count {
        return None;
    }
    let switch_flag = raw[LSY_KEY_SWITCH_FLAG_OFF] != 0;
    let protection_id = u16::from_le_bytes(
        raw[LSY_PROTECTION_ID_OFF..LSY_PROTECTION_ID_OFF + 2]
            .try_into()
            .ok()?,
    );
    let key = derive_lsy_key(protection_id, switch_flag);

    let mut out = Vec::with_capacity(line_count);
    let mut wraps = 0_i64;
    let mut prev = 0_u16;
    for i in 0..line_count {
        let off = LSY_PAYLOAD_START + 2 * i;
        let val = u16::from_le_bytes(raw[off..off + 2].try_into().ok()?) ^ key;
        if i > 0 && val < prev {
            wraps += 1;
        }
        prev = val;
        out.push(val as i64 + wraps * LSY_WRAP_CS);
    }
    Some(out)
}

fn strip_outer_parens(s: &str) -> (String, bool) {
    let t = s.trim();
    for (open, close) in [('(', ')'), ('（', '）'), ('[', ']')] {
        if let Some(inner) = t.strip_prefix(open).and_then(|r| r.strip_suffix(close)) {
            return (inner.trim().to_string(), true);
        }
    }
    (t.to_string(), false)
}

fn zip_line_sync(timings_cs: &[i64], text: &str) -> Vec<LyricLine> {
    let norm = text.replace("\r\n", "\n");
    let mut rows: Vec<&str> = norm.split('\n').collect();
    if rows.len() != timings_cs.len() {
        while rows.len() > timings_cs.len() && rows.last().is_some_and(|r| r.trim().is_empty()) {
            rows.pop();
        }
    }
    if rows.len() != timings_cs.len() {
        return Vec::new();
    }

    let mut lines = Vec::with_capacity(rows.len());
    for i in 0..rows.len() {
        let row = rows[i].replace(['\u{2005}', '\u{3000}', '\u{00A0}'], " ");
        let trimmed = row.trim();
        if trimmed.is_empty() {
            continue;
        }
        let start_ms = timings_cs[i] * 10;
        let mut end_ms = if i + 1 < rows.len() {
            timings_cs[i + 1] * 10
        } else {
            start_ms + 3000
        };
        if end_ms <= start_ms {
            end_ms = start_ms + 3000;
        }
        let (clean, is_bg) = strip_outer_parens(trimmed);
        lines.push(
            LyricLine::new(start_ms as f64 / 1000.0, clean)
                .with_end(Some(end_ms as f64 / 1000.0))
                .with_bg(is_bg),
        );
    }
    mark_echoes_as_background(&mut lines);
    lines
}

fn parse_wsy(payload: &[u8]) -> Vec<LyricLine> {
    let Ok(xml) = std::str::from_utf8(payload) else {
        return Vec::new();
    };
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut lines: Vec<LyricLine> = Vec::new();
    let mut in_line = false;
    let mut in_word = false;
    let mut cur_tag = String::new();
    let mut line_text = String::new();
    let mut word_text = String::new();
    let mut word_start = 0_i64;
    let mut word_end = 0_i64;
    let mut raw_tokens: Vec<RawToken> = Vec::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                cur_tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if cur_tag == "line" {
                    in_line = true;
                    raw_tokens.clear();
                    line_text.clear();
                } else if in_line && cur_tag == "word" {
                    in_word = true;
                    word_text.clear();
                    word_start = 0;
                    word_end = 0;
                }
            }
            Ok(Event::Text(e)) => {
                if let Ok(txt) = e.unescape() {
                    if in_word {
                        match cur_tag.as_str() {
                            "wordstring" => word_text.push_str(&txt),
                            "starttime" => word_start = txt.trim().parse().unwrap_or(0),
                            "endtime" => word_end = txt.trim().parse().unwrap_or(0),
                            _ => {}
                        }
                    } else if in_line && cur_tag == "linestring" {
                        line_text.push_str(&txt);
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let end_tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if in_word && end_tag == "word" {
                    in_word = false;
                    if !word_text.is_empty() {
                        let s = word_start.max(0);
                        raw_tokens.push(RawToken {
                            text: word_text.clone(),
                            start_ms: s,
                            end_ms: word_end.max(s),
                        });
                    }
                } else if in_line && end_tag == "line" {
                    in_line = false;
                    if !raw_tokens.is_empty() {
                        let mut merged: Vec<WordTiming> = Vec::new();
                        for tok in &raw_tokens {
                            if tok.text.trim().is_empty() {
                                if let Some(prev) = merged.last_mut() {
                                    prev.ends_with_space = true;
                                }
                                continue;
                            }
                            let can_merge = merged.last().is_some_and(|prev| {
                                !prev.ends_with_space
                                    && !contains_unspaced_script(&tok.text)
                                    && !contains_unspaced_script(&prev.text)
                            });
                            if can_merge {
                                let prev = merged.last_mut().unwrap();
                                let mut combined = prev.text.clone();
                                combined.push_str(&tok.text);
                                let new_end = prev.end.max(tok.end_ms as f64 / 1000.0);
                                let mut rebuilt =
                                    create_word_timing(prev.start, new_end, &combined);
                                rebuilt.ends_with_space = prev.ends_with_space;
                                *prev = rebuilt;
                            } else {
                                merged.push(create_word_timing(
                                    tok.start_ms as f64 / 1000.0,
                                    tok.end_ms as f64 / 1000.0,
                                    &tok.text,
                                ));
                            }
                        }

                        if !merged.is_empty() {
                            let mut txt = line_text
                                .replace(['\u{2005}', '\u{3000}', '\u{00A0}'], " ")
                                .trim()
                                .to_string();
                            if txt.is_empty() {
                                for w in &merged {
                                    txt.push_str(&w.text);
                                    if w.ends_with_space {
                                        txt.push(' ');
                                    }
                                }
                                txt = txt.trim().to_string();
                            }
                            let (clean_txt, is_bg) = strip_outer_parens(&txt);
                            if !clean_txt.is_empty() {
                                let start = merged.first().map(|w| w.start).unwrap_or(0.0);
                                let end = merged.last().map(|w| w.end).unwrap_or(start);
                                lines.push(
                                    LyricLine::new(start, clean_txt)
                                        .with_end(Some(end))
                                        .with_words(Some(merged))
                                        .with_bg(is_bg),
                                );
                            }
                        }
                    }
                }
                cur_tag.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    lines.sort_by(|a, b| a.time.total_cmp(&b.time));
    mark_echoes_as_background(&mut lines);
    lines
}

fn parse_xml_songs(xml: &str) -> Vec<Song> {
    let mut reader = Reader::from_str(xml);
    let mut songs = Vec::new();
    let mut cur: Option<Song> = None;
    let mut tag = String::new();
    let mut text_buf = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                text_buf.clear();
                if tag == "song" {
                    cur = Some(Song {
                        lyrics_id: String::new(),
                        title: String::new(),
                        artist: String::new(),
                        writer: String::new(),
                        composer: String::new(),
                        duration_ms: 0,
                        lyrics_data: String::new(),
                    });
                }
            }
            Ok(Event::Text(e)) => {
                if let Ok(t) = e.unescape() {
                    text_buf.push_str(&t);
                }
            }
            Ok(Event::End(ref e)) => {
                let end_tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if end_tag == "song" {
                    if let Some(s) = cur.take() {
                        songs.push(s);
                    }
                } else if let Some(s) = cur.as_mut() {
                    let val = text_buf.trim().to_string();
                    match end_tag.as_str() {
                        "lyricsId" | "lyricsid" => s.lyrics_id = val,
                        "title" => s.title = val,
                        "artist" => s.artist = val,
                        "writer" => s.writer = val,
                        "composer" => s.composer = val,
                        "duration" => s.duration_ms = val.parse().unwrap_or(0),
                        "lyricsData" => s.lyrics_data = val,
                        _ => {}
                    }
                }
                tag.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    songs
}

async fn request_tier(ctx: &ProviderContext, track: &TrackInfo, tier: i32) -> Vec<Song> {
    throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
    let title = clean_title(&track.title);
    let form = format!(
        "clientAppId={}&terminalType={}&lyricsType={tier}&key_title={}&key_artist={}&key_album={}",
        urlencoding::encode(CLIENT_APP_ID),
        urlencoding::encode(TERMINAL_TYPE),
        urlencoding::encode(&title),
        urlencoding::encode(&track.artist),
        urlencoding::encode(&track.album),
    );
    let Ok(resp) = ctx
        .client()
        .post(API_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(xml) = resp.text().await else {
        return Vec::new();
    };
    parse_xml_songs(&xml)
}

impl LyricsProvider for PetitLyricsProvider {
    fn id(&self) -> &'static str {
        "petitlyrics"
    }

    fn display_name(&self) -> &'static str {
        "PetitLyrics"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let mut lsy_timings: Option<(Vec<i64>, Song)> = None;
            let mut zip_text: Option<(String, Song)> = None;
            let mut plain_text: Option<(String, Song)> = None;

            for tier in [TIER_WORD_SYNC, TIER_LINE_SYNC, TIER_UNSYNCED] {
                let mut songs = request_tier(ctx, track, tier).await;
                if songs.is_empty() {
                    continue;
                }
                let prepared =
                    crate::lyrics::providers::PreparedTrack::new(track, ctx.custom_regex.as_ref());
                songs.sort_by_key(|song| {
                    let v =
                        prepared.evaluate(&song.title, &song.artist, song.duration_ms / 1000, None);
                    let sc = if v.is_high_match() { v.score } else { -1 };
                    std::cmp::Reverse(sc)
                });

                for song in &songs {
                    if song.lyrics_data.is_empty() {
                        continue;
                    }
                    let Ok(payload) = base64::engine::general_purpose::STANDARD
                        .decode(song.lyrics_data.replace(['\r', '\n'], ""))
                    else {
                        continue;
                    };
                    if payload.is_empty() {
                        continue;
                    }
                    match classify_payload(&payload) {
                        TIER_WORD_SYNC => {
                            let lines = parse_wsy(&payload);
                            if !lines.is_empty() {
                                let mut lyrics = Lyrics::new(lines, "PetitLyrics", true);
                                lyrics.raw_format =
                                    String::from_utf8(payload).ok().filter(|s| !s.is_empty());
                                lyrics.format_type = Some("wsy".to_string());
                                if !song.lyrics_id.is_empty() {
                                    lyrics.source_url =
                                        Some(format!("{SOURCE_URL_PREFIX}{}", song.lyrics_id));
                                }
                                if !song.writer.is_empty() {
                                    lyrics.songwriters.push(song.writer.clone());
                                }
                                if !song.composer.is_empty() {
                                    lyrics.credits.push(song.composer.clone());
                                }
                                if let Some(cand) = FetchedCandidate::with_metadata(
                                    lyrics,
                                    &song.title,
                                    &song.artist,
                                    song.duration_ms / 1000,
                                    None,
                                    track,
                                    ctx,
                                ) {
                                    return Ok(vec![cand]);
                                }
                            }
                        }
                        TIER_LINE_SYNC => {
                            if lsy_timings.is_none()
                                && let Some(timings) = decode_lsy_timings(&payload)
                                && !timings.is_empty()
                            {
                                lsy_timings = Some((timings, song.clone()));
                            }
                        }
                        _ => {
                            if let Ok(txt) = std::str::from_utf8(&payload)
                                && !txt.trim().is_empty()
                            {
                                if lsy_timings
                                    .as_ref()
                                    .is_some_and(|(_, ls)| ls.lyrics_id == song.lyrics_id)
                                {
                                    zip_text = Some((txt.to_string(), song.clone()));
                                }
                                if plain_text.is_none() {
                                    plain_text = Some((txt.to_string(), song.clone()));
                                }
                            }
                        }
                    }
                }

                if let Some((timings, lsong)) = lsy_timings.as_ref() {
                    let ztxt = zip_text
                        .as_ref()
                        .or(plain_text.as_ref())
                        .map(|(t, _)| t.as_str());
                    if let Some(txt) = ztxt {
                        let lines = zip_line_sync(timings, txt);
                        if !lines.is_empty() {
                            let mut lyrics = Lyrics::new(lines, "PetitLyrics", true);
                            lyrics.format_type = Some("lrc".to_string());
                            if !lsong.lyrics_id.is_empty() {
                                lyrics.source_url =
                                    Some(format!("{SOURCE_URL_PREFIX}{}", lsong.lyrics_id));
                            }
                            if let Some(cand) = FetchedCandidate::with_metadata(
                                lyrics,
                                &lsong.title,
                                &lsong.artist,
                                lsong.duration_ms / 1000,
                                None,
                                track,
                                ctx,
                            ) {
                                return Ok(vec![cand]);
                            }
                        }
                    }
                }
            }

            if let Some((txt, psong)) = plain_text {
                let (lrc_lines, mut credits) = parse_synced_with_credits(&txt);
                let (lines, synced, fmt) = if !lrc_lines.is_empty() {
                    (lrc_lines, true, "lrc")
                } else {
                    (parse_plain(&txt), false, "txt")
                };
                if !lines.is_empty() {
                    let mut lyrics = Lyrics::new(lines, "PetitLyrics", synced);
                    lyrics.raw_format = Some(txt);
                    lyrics.format_type = Some(fmt.to_string());
                    if !psong.lyrics_id.is_empty() {
                        lyrics.source_url = Some(format!("{SOURCE_URL_PREFIX}{}", psong.lyrics_id));
                    }
                    if !psong.composer.is_empty() {
                        credits.push(psong.composer);
                    }
                    lyrics.credits = credits;
                    if let Some(cand) = FetchedCandidate::with_metadata(
                        lyrics,
                        &psong.title,
                        &psong.artist,
                        psong.duration_ms / 1000,
                        None,
                        track,
                        ctx,
                    ) {
                        return Ok(vec![cand]);
                    }
                }
            }

            Ok(Vec::new())
        })
    }
}
