//! LyricsPlus (`lyricsplus.prjktla.my.id` + mirrors) TTML / v2 JSON provider (`LyricsPlusProvider.java`).

use crate::lyrics::filter::is_cjk_ideograph;
use crate::lyrics::parsers::parse_ttml;
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, ProviderContext, SOFT_MIN,
    score_lyrics_candidate,
};
use crate::lyrics::types::{
    LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo, WordTiming, create_word_timing,
};
use serde_json::Value;
use std::collections::HashMap;

const API_HOSTS: &[&str] = &[
    "https://lyricsplus.prjktla.my.id",
    "https://lyricsplus.binimum.org",
    "https://lyricsplus.prjktla.workers.dev",
];

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

pub struct LyricsPlusProvider;

fn hosts(ctx: &ProviderContext) -> Vec<String> {
    if let Some(custom) =
        crate::config::validate_http_base_url(ctx.settings.endpoints.lyricsplus.as_deref())
    {
        vec![custom]
    } else {
        API_HOSTS.iter().map(|s| (*s).to_string()).collect()
    }
}

fn encode_pct20(val: &str) -> String {
    urlencoding::encode(val).into_owned()
}

fn is_self_source(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    let trimmed = lower.strip_prefix('q').unwrap_or(&lower);
    trimmed == "lyricsplus"
}

fn is_agent_duet(agent_id: Option<&str>) -> bool {
    let Some(id) = agent_id else {
        return false;
    };
    if !id.starts_with('v') || id.len() > 7 {
        return false;
    }
    let Ok(num) = id[1..].parse::<u32>() else {
        return false;
    };
    num > 0 && num % 2 == 0
}

fn is_continuous_cjk(text: &str) -> bool {
    let mut has_cjk = false;
    for c in text.chars() {
        if is_cjk_ideograph(c)
            || matches!(c as u32, 0x3040..=0x30FF | 0x31F0..=0x31FF | 0xAC00..=0xD7AF)
        {
            has_cjk = true;
        }
    }
    has_cjk && !text.contains(' ')
}

struct WordCand<'a> {
    syl: &'a Value,
    romaji: Option<String>,
}

fn strip_parens(s: &str) -> String {
    s.replace(['(', ')', '（', '）'], "").trim().to_string()
}

#[allow(clippy::too_many_arguments)]
fn build_line_from_candidates(
    cands: &[WordCand<'_>],
    line_start_ms: i64,
    line_end_ms: Option<i64>,
    line_dur_ms: i64,
    fallback_text: Option<&str>,
    agent_id: Option<String>,
    is_duet: bool,
    is_bg: bool,
    song_part: Option<String>,
) -> Option<LyricLine> {
    if cands.is_empty() {
        return None;
    }

    let mut words: Vec<WordTiming> = Vec::with_capacity(cands.len());
    let mut buf = String::new();

    for c in cands {
        let start_ms = c.syl.get("time").and_then(Value::as_i64).unwrap_or(0);
        let dur_ms = c.syl.get("duration").and_then(Value::as_i64).unwrap_or(0);
        let end_ms = start_ms + dur_ms;
        let text = c.syl.get("text").and_then(Value::as_str).unwrap_or("");
        let ends_with_space = text.ends_with(' ') || text.ends_with('\t');

        let clean_word = if is_bg {
            strip_parens(text)
        } else {
            text.trim().to_string()
        };
        let clean_roma = c.romaji.as_deref().map(|r| {
            if is_bg {
                strip_parens(r)
            } else {
                r.to_string()
            }
        });

        if !clean_word.is_empty() {
            let wt = create_word_timing(
                start_ms as f64 / 1000.0,
                end_ms as f64 / 1000.0,
                &clean_word,
            )
            .with_space(ends_with_space)
            .with_annotations(clean_roma, None);
            words.push(wt);
            buf.push_str(&clean_word);
            if ends_with_space {
                buf.push(' ');
            }
        }
    }

    let mut full_text = buf.trim().to_string();
    if !full_text.contains(' ') && words.len() > 1 && !is_continuous_cjk(&words[0].text) {
        let mut rebuilt = String::new();
        let last_idx = words.len() - 1;
        for (i, w) in words.iter_mut().enumerate() {
            if i > 0 {
                rebuilt.push(' ');
            }
            rebuilt.push_str(&w.text);
            w.ends_with_space = i < last_idx;
        }
        full_text = rebuilt;
    }

    if let Some(last) = words.last_mut() {
        last.ends_with_space = false;
    }

    if full_text.is_empty()
        && let Some(fb) = fallback_text
        && !fb.trim().is_empty()
    {
        full_text = fb.trim().to_string();
    }

    if full_text.is_empty() && words.is_empty() {
        return None;
    }

    let start = words
        .first()
        .map(|w| w.start)
        .unwrap_or(line_start_ms as f64 / 1000.0);
    let mut end = words
        .last()
        .map(|w| w.end)
        .or(line_end_ms.map(|e| e as f64 / 1000.0));
    if end.is_some_and(|e| e < start) && line_dur_ms > 0 {
        end = Some(start + line_dur_ms as f64 / 1000.0);
    }

    Some(
        LyricLine::new(start, full_text)
            .with_end(end)
            .with_words((!words.is_empty()).then_some(words))
            .with_agent(agent_id, is_duet)
            .with_bg(is_bg)
            .with_song_part(song_part),
    )
}

fn parse_v2_json(root: &Value) -> Option<Lyrics> {
    let arr = root.get("lyrics").and_then(Value::as_array)?;
    if arr.is_empty() {
        return None;
    }

    let meta = root.get("metadata");
    let upstream = meta
        .and_then(|m| m.get("source"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && !is_self_source(s));

    let songwriters: Vec<String> = meta
        .and_then(|m| m.get("songWriters"))
        .and_then(Value::as_array)
        .map(|sw| {
            sw.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let song_parts = meta
        .and_then(|m| m.get("songParts"))
        .and_then(Value::as_array);

    let mut lines = Vec::new();
    let mut roma_lines = Vec::new();
    let mut trans_lines = Vec::new();
    let mut has_word_sync = false;

    for item in arr {
        let line_time_ms = item.get("time").and_then(Value::as_i64).unwrap_or(-1);
        let line_dur_ms = item.get("duration").and_then(Value::as_i64).unwrap_or(0);
        let line_end_ms =
            (line_time_ms >= 0 && line_dur_ms > 0).then_some(line_time_ms + line_dur_ms);
        let raw_text = item.get("text").and_then(Value::as_str).unwrap_or("");

        let elem = item.get("element");
        let agent_id = elem
            .and_then(|e| e.get("singer"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let song_part = elem
            .and_then(|e| e.get("songPartIndex"))
            .and_then(Value::as_i64)
            .and_then(|idx| {
                song_parts
                    .and_then(|sp| usize::try_from(idx).ok().and_then(|i| sp.get(i)))
                    .and_then(|sp| sp.get("name"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });
        let is_duet = is_agent_duet(agent_id.as_deref());

        let translit = item.get("transliteration");
        let trans_syls = translit
            .and_then(|t| t.get("syllabus"))
            .and_then(Value::as_array);
        let translit_text = translit.and_then(|t| t.get("text")).and_then(Value::as_str);
        let trans_text = item.get("translation").and_then(Value::as_str);

        if let Some(syllabus) = item.get("syllabus").and_then(Value::as_array)
            && !syllabus.is_empty()
        {
            let mut main_syls = Vec::new();
            let mut bg_syls = Vec::new();
            for (s, syl) in syllabus.iter().enumerate() {
                let roma = trans_syls
                    .and_then(|ts| ts.get(s))
                    .and_then(|t| t.get("text"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let is_bg = syl
                    .get("isBackground")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let cand = WordCand { syl, romaji: roma };
                if is_bg {
                    bg_syls.push(cand);
                } else {
                    main_syls.push(cand);
                }
            }

            if let Some(main_line) = build_line_from_candidates(
                &main_syls,
                line_time_ms,
                line_end_ms,
                line_dur_ms,
                Some(raw_text),
                agent_id.clone(),
                is_duet,
                false,
                song_part.clone(),
            ) {
                if main_line.has_word_timings() {
                    has_word_sync = true;
                }
                lines.push(main_line);
            }
            if let Some(bg_line) = build_line_from_candidates(
                &bg_syls,
                line_time_ms,
                line_end_ms,
                line_dur_ms,
                None,
                agent_id,
                is_duet,
                true,
                song_part,
            ) {
                if bg_line.has_word_timings() {
                    has_word_sync = true;
                }
                lines.push(bg_line);
            }
        } else {
            let mut text = raw_text.trim();
            let mut is_bg = false;
            if (text.starts_with('(') && text.ends_with(')'))
                || (text.starts_with('（') && text.ends_with('）'))
            {
                is_bg = true;
                let mut chs = text.chars();
                chs.next();
                chs.next_back();
                text = chs.as_str().trim();
            }
            if !text.is_empty() && line_time_ms >= 0 {
                lines.push(
                    LyricLine::new(line_time_ms as f64 / 1000.0, text)
                        .with_end(line_end_ms.map(|e| e as f64 / 1000.0))
                        .with_agent(agent_id, is_duet)
                        .with_bg(is_bg)
                        .with_song_part(song_part),
                );
            }
        }

        if let Some(rt) = translit_text.map(str::trim).filter(|s| !s.is_empty()) {
            roma_lines.push(LyricLine::new(0.0, rt));
        }
        if let Some(tt) = trans_text.map(str::trim).filter(|s| !s.is_empty()) {
            trans_lines.push(LyricLine::new(0.0, tt));
        }
    }

    if lines.is_empty() {
        return None;
    }

    let provider_name = upstream
        .map(|s| format!("LyricsPlus (via {s})"))
        .unwrap_or_else(|| "LyricsPlus".to_string());
    let synced = lines
        .first()
        .is_some_and(|l| l.time > 0.0 || lines.len() > 1);
    let mut l = Lyrics::new(lines, provider_name, synced);
    l.songwriters = songwriters;
    if !roma_lines.is_empty() {
        let mut r = HashMap::new();
        r.insert("default".to_string(), roma_lines);
        l.romanizations = r;
    }
    if !trans_lines.is_empty() {
        let mut t = HashMap::new();
        t.insert("default".to_string(), trans_lines);
        l.translations = t;
    }
    l.format_type = Some(if has_word_sync { "syllable" } else { "line" }.to_string());
    Some(l)
}

async fn fetch_from_url(ctx: &ProviderContext, url: &str) -> Option<Lyrics> {
    let resp = ctx
        .client()
        .get(url)
        .header("User-Agent", BROWSER_UA)
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    if let Some(ttml) = root.get("ttml").and_then(Value::as_str)
        && !ttml.is_empty()
    {
        let source = root
            .get("processingTime")
            .and_then(|p| p.get("selectedSongMetadata"))
            .and_then(|m| m.get("source"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && !is_self_source(s));
        let pname = source
            .map(|s| format!("LyricsPlus (via {s})"))
            .unwrap_or_else(|| "LyricsPlus".to_string());
        if let Some(l) = parse_ttml(ttml, &pname) {
            return Some(l);
        }
    }
    parse_v2_json(&root)
}

async fn fetch_from_api(
    ctx: &ProviderContext,
    host_list: &[String],
    track: &TrackInfo,
) -> Option<Lyrics> {
    for host in host_list {
        let mut url = format!(
            "{host}/v1/ttml/get?title={}&artist={}",
            encode_pct20(&track.title),
            encode_pct20(&track.artist)
        );
        if !track.album.is_empty() {
            url.push_str("&album=");
            url.push_str(&encode_pct20(&track.album));
        }
        if track.duration_secs > 0 {
            url.push_str(&format!("&duration={}", track.duration_secs));
        }
        if let Some(lyrics) = fetch_from_url(ctx, &url).await
            && !lyrics.is_empty()
        {
            return Some(lyrics);
        }
    }
    None
}

async fn fetch_with_param(
    ctx: &ProviderContext,
    host_list: &[String],
    param: &str,
    value: &str,
) -> Option<Lyrics> {
    for host in host_list {
        let url = format!("{host}/v1/ttml/get?{param}={}", encode_pct20(value));
        if let Some(l) = fetch_from_url(ctx, &url).await
            && !l.is_empty()
        {
            return Some(l);
        }
    }
    None
}

impl LyricsProvider for LyricsPlusProvider {
    fn id(&self) -> &'static str {
        "lyricsplus"
    }

    fn display_name(&self) -> &'static str {
        "LyricsPlus"
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

            let host_list = hosts(ctx);
            let mut out = Vec::new();

            if let Some(lyrics) = fetch_from_api(ctx, &host_list, track).await
                && let Some(c) = FetchedCandidate::blind(lyrics)
            {
                out.push(c);
            } else {
                let clean_title = track
                    .title
                    .replace("(Official Video)", "")
                    .replace("(Official Music Video)", "")
                    .replace("(Official Audio)", "")
                    .trim()
                    .to_string();
                if clean_title != track.title && !clean_title.is_empty() {
                    let clean_track = TrackInfo::new(
                        clean_title,
                        track.artist.clone(),
                        track.album.clone(),
                        track.duration_secs,
                    );
                    if let Some(lyrics) = fetch_from_api(ctx, &host_list, &clean_track).await
                        && let Some(c) = FetchedCandidate::blind(lyrics)
                    {
                        out.push(c);
                    }
                }
            }

            if !out.is_empty() {
                return Ok(out);
            }

            // Fallback candidate search (/v1/songlist/search)
            let q = format!("{} {}", track.title, track.artist)
                .trim()
                .to_string();
            for host in &host_list {
                let url = format!("{host}/v1/songlist/search?q={}", encode_pct20(&q));
                let Ok(resp) = ctx
                    .client()
                    .get(&url)
                    .header("User-Agent", BROWSER_UA)
                    .header("Accept", "application/json")
                    .send()
                    .await
                else {
                    continue;
                };
                if !resp.status().is_success() {
                    continue;
                }
                let Ok(root) = resp.json::<Value>().await else {
                    continue;
                };
                let Some(results) = root.get("results").and_then(Value::as_array) else {
                    continue;
                };
                for item in results {
                    if out.len() >= MAX_CANDIDATES {
                        break;
                    }
                    let c_title = item.get("title").and_then(Value::as_str).unwrap_or("");
                    let c_artist = item.get("artist").and_then(Value::as_str).unwrap_or("");
                    let dur_sec = item
                        .get("durationMs")
                        .and_then(Value::as_i64)
                        .map(|ms| ms / 1000)
                        .unwrap_or(0);

                    let mut lyrics = None;
                    if let Some(isrc) = item
                        .get("isrc")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        lyrics = fetch_with_param(ctx, &host_list, "isrc", isrc).await;
                    }
                    if lyrics.is_none()
                        && let Some(ids_obj) = item.get("id")
                    {
                        for key in ["spotify", "qq", "appleMusic", "apple", "deezer"] {
                            if let Some(pid) = ids_obj
                                .get(key)
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                            {
                                lyrics = fetch_with_param(ctx, &host_list, "platformId", pid).await;
                                if lyrics.is_some() {
                                    break;
                                }
                            }
                        }
                    }
                    if lyrics.is_none() && !c_title.is_empty() && !c_artist.is_empty() {
                        let cand_track = TrackInfo::new(c_title, c_artist, "", dur_sec);
                        lyrics = fetch_from_api(ctx, &host_list, &cand_track).await;
                    }

                    let Some(lyrics) = lyrics else {
                        continue;
                    };
                    let score = score_lyrics_candidate(
                        c_title,
                        c_artist,
                        dur_sec,
                        &lyrics,
                        track,
                        ctx.custom_regex.as_ref(),
                    );
                    if score >= SOFT_MIN {
                        out.push(FetchedCandidate {
                            lyrics,
                            match_score: score,
                            is_blind: true,
                            video_id_keyed: false,
                        });
                    }
                }
                if !out.is_empty() {
                    break;
                }
            }

            out.sort_by_key(|a| std::cmp::Reverse(a.match_score));
            Ok(out)
        })
    }
}
