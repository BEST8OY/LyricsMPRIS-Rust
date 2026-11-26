//! Luna (`api.qishui.com` / Douyin Qishui) KRC / LRC provider (`LunaProvider.java`).

use crate::lyrics::parsers::{parse_krc, parse_plain, parse_synced};
use crate::lyrics::providers::{
    BoxFuture, FetchedCandidate, LyricsProvider, MAX_CANDIDATES, ProviderContext,
    score_lyrics_candidate, score_track_candidate, throttle_ms,
};
use crate::lyrics::types::{LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use serde_json::Value;
use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SEARCH_URL: &str = "https://api.qishui.com/luna/search/track";
const DETAIL_URL: &str = "https://beta-luna.douyin.com/luna/h5/seo_track";
const SEARCH_UA: &str = "com.luna.music/100198030 (Linux; U; Android 15; zh_CN_#Hans; ABR-AL80; Build/V417IR;tt-ok/3.12.13.19)";
const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const THROTTLE_MS: u64 = 250;
static LAST_REQ_MS: AtomicU64 = AtomicU64::new(0);

pub struct LunaProvider;

fn random_numeric_id() -> String {
    let n = Uuid::new_v4().as_u128();
    format!("{:019}", n % 10_000_000_000_000_000_000_u128)
}

fn first_artist_name(obj: &Value) -> &str {
    obj.get("artists")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn merge_aligned(orig: &[LyricLine], aligned: &[LyricLine]) -> Vec<LyricLine> {
    orig.iter()
        .map(|o| {
            let best = aligned
                .iter()
                .min_by_key(|a| ((a.time - o.time) * 1000.0).abs() as i64)
                .filter(|a| (a.time - o.time).abs() <= 0.35);
            let txt = best.map(|b| b.text.as_str()).unwrap_or("");
            let mut line = LyricLine::new(o.time, txt);
            line.end = o.end;
            line
        })
        .collect()
}

async fn fetch_by_track_id(ctx: &ProviderContext, track_id: &str) -> Option<Lyrics> {
    throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
    let url = format!(
        "{DETAIL_URL}?track_id={}&device_platform=web",
        urlencoding::encode(track_id)
    );
    let resp = ctx
        .client()
        .get(&url)
        .header("User-Agent", WEB_UA)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    let lyric_info = root.get("lyric")?;
    let content = lyric_info.get("content").and_then(Value::as_str)?;
    if content.is_empty() {
        return None;
    }
    let ltype = lyric_info
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("lrc");

    let (lines, fmt, synced) = if ltype == "krc" {
        let k = parse_krc(content);
        if !k.is_empty() {
            (k, "krc", true)
        } else {
            let l = parse_synced(content);
            if !l.is_empty() {
                (l, "lrc", true)
            } else {
                (parse_plain(content), "txt", false)
            }
        }
    } else {
        let l = parse_synced(content);
        if !l.is_empty() {
            (l, "lrc", true)
        } else {
            (parse_plain(content), "txt", false)
        }
    };
    if lines.is_empty() {
        return None;
    }

    let mut lyrics = Lyrics::new(lines, "Luna", synced);
    lyrics.raw_format = Some(content.to_string());
    lyrics.format_type = Some(fmt.to_string());
    lyrics.source_url = Some(format!("https://www.douyin.com/qishui/song/{track_id}"));

    if let Some(lang_map) = lyric_info
        .get("lang_translations")
        .and_then(Value::as_object)
    {
        for (lang, tobj) in lang_map {
            let tcontent = tobj.get("content").and_then(Value::as_str).unwrap_or("");
            if tcontent.is_empty() {
                continue;
            }
            let ttype = tobj.get("type").and_then(Value::as_str).unwrap_or("lrc");
            let tlines = if ttype == "krc" {
                parse_krc(tcontent)
            } else {
                parse_synced(tcontent)
            };
            if !tlines.is_empty() {
                let merged = merge_aligned(&lyrics.lines, &tlines);
                if merged.iter().any(|l| !l.text.is_empty()) {
                    lyrics.translations.insert(lang.clone(), merged);
                }
            }
        }
    }

    let track_node = root
        .get("track")
        .or_else(|| root.get("seo_track").and_then(|s| s.get("track")));
    if let Some(team) = track_node.and_then(|t| t.get("song_maker_team")) {
        for key in ["composers", "lyricists"] {
            if let Some(arr) = team.get(key).and_then(Value::as_array) {
                for entry in arr {
                    if let Some(name) = entry.get("name").and_then(Value::as_str).map(str::trim)
                        && !name.is_empty()
                    {
                        lyrics.songwriters.push(name.to_string());
                    }
                }
            }
        }
    }

    Some(lyrics)
}

impl LyricsProvider for LunaProvider {
    fn id(&self) -> &'static str {
        "luna"
    }

    fn display_name(&self) -> &'static str {
        "Luna"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
            let keyword = format!("{} {}", track.artist, track.title);
            let dev_id = random_numeric_id();
            let inst_id = random_numeric_id();
            let rticket = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();

            let url = format!(
                "{SEARCH_URL}?device_platform=android&os=android&ssmix=a\
                 &cdid=46556f98-1720-4248-83da-62b74b60b46a&channel=xiaomi_8478_64\
                 &aid=386088&app_name=luna&version_code=100198030&version_name=19.8.0\
                 &manifest_version_code=100198030&update_version_code=100198030\
                 &resolution=1080*1920&dpi=480&device_type=ABR-AL80&device_brand=HUAWEI\
                 &language=zh&os_api=35&os_version=15&ac=wifi&device_model=ABR-AL80\
                 &tz_name=Asia/Shanghai&tz_offset=28800&package=com.luna.music\
                 &sim_region=cn&iid={inst_id}&device_id={dev_id}&_rticket={rticket}\
                 &q={}&cursor=0&count=20",
                urlencoding::encode(&keyword)
            );
            let resp = ctx
                .client()
                .get(&url)
                .header("User-Agent", SEARCH_UA)
                .send()
                .await?;
            if !resp.status().is_success() {
                return Ok(Vec::new());
            }
            let root: Value = resp.json().await?;
            let Some(groups) = root.get("result_groups").and_then(Value::as_array) else {
                return Ok(Vec::new());
            };

            let mut tracks: Vec<(i32, &Value)> = Vec::new();
            for group in groups {
                if let Some(items) = group.get("data").and_then(Value::as_array) {
                    for item in items {
                        if let Some(entity) = item.get("entity").and_then(|e| e.get("track")) {
                            let t = entity.get("name").and_then(Value::as_str).unwrap_or("");
                            let a = first_artist_name(entity);
                            let d = entity
                                .get("duration")
                                .and_then(Value::as_i64)
                                .map(|ms| ms / 1000)
                                .unwrap_or(0);
                            let sc = score_track_candidate(
                                t,
                                a,
                                d,
                                None,
                                track,
                                ctx.custom_regex.as_ref(),
                            );
                            if sc >= crate::lyrics::providers::SOFT_MIN {
                                tracks.push((sc, entity));
                            }
                        }
                    }
                }
            }

            tracks.sort_by_key(|a| std::cmp::Reverse(a.0));
            let mut out = Vec::new();
            for (_, item) in tracks {
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
                let tid = item.get("id").and_then(Value::as_str).unwrap_or("");
                if tid.is_empty() {
                    continue;
                }
                let Some(lyrics) = fetch_by_track_id(ctx, tid).await else {
                    continue;
                };
                let t = item.get("name").and_then(Value::as_str).unwrap_or("");
                let a = first_artist_name(item);
                let d = item
                    .get("duration")
                    .and_then(Value::as_i64)
                    .map(|ms| ms / 1000)
                    .unwrap_or(0);
                let score =
                    score_lyrics_candidate(t, a, d, &lyrics, track, ctx.custom_regex.as_ref());
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
