//! NetEase Cloud Music EAPI (AES-ECB + YRC / mixed / LRC) provider (`NetEaseProvider.java`).

use crate::lyrics::crypto::{aes_ecb_pkcs7_encrypt_hex, md5_bytes, md5_hex};
use crate::lyrics::parsers::parse_synced;
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
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use uuid::Uuid;

const EAPI_HOST: &str = "https://interface.music.163.com";
const EAPI_KEY: &[u8; 16] = b"e82ckenh8dichen8";
const REGISTER_PATH: &str = "/eapi/register/anonimous";
const NETEASE_UA: &str = "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) Safari/537.36 Chrome/91.0.4472.164 NeteaseMusicDesktop/3.1.3.203419";
const APP_VER: &str = "3.1.3.203419";
const DEVICEID_XOR_KEY: &str = "3go8&$8*3*3h0k(2)2";

static YRC_LINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\d+),(\d+)](.*)").unwrap());
static YRC_WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\((\d+),(\d+),\d+\)([^()]*)").unwrap());
static LRC_TIME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\d+):(\d{2})(?:[.:](\d{1,3}))?]").unwrap());
static RICH_JSON_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^\s*\{"#).unwrap());

struct NetEaseSession {
    device_id: String,
    client_sign: String,
    osver: String,
    cookies: HashMap<String, String>,
    initialized: bool,
}

impl NetEaseSession {
    fn new() -> Self {
        let u = Uuid::new_v4().simple().to_string();
        let u2 = Uuid::new_v4().simple().to_string();
        let u3 = Uuid::new_v4().simple().to_string();
        let mac = format!(
            "{}:{}:{}:{}:{}:{}",
            &u[0..2],
            &u[2..4],
            &u[4..6],
            &u[6..8],
            &u[8..10],
            &u[10..12]
        )
        .to_uppercase();
        let client_sign = format!("{mac}@@@DESKTOPAPP@@@@@@{u2}{u3}");
        Self {
            device_id: u,
            client_sign,
            osver: "Microsoft-Windows-10--build-22631-64bit".to_string(),
            cookies: HashMap::new(),
            initialized: false,
        }
    }

    fn reset_pre_cookies(&mut self) {
        self.cookies.clear();
        self.cookies
            .insert("deviceId".to_string(), self.device_id.clone());
        self.cookies.insert("os".to_string(), "pc".to_string());
        self.cookies.insert("osver".to_string(), self.osver.clone());
        self.cookies
            .insert("appver".to_string(), APP_VER.to_string());
        self.cookies
            .insert("channel".to_string(), "netease".to_string());
        self.cookies
            .insert("__remember_me".to_string(), "true".to_string());
    }

    fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn capture_cookies(&mut self, headers: &reqwest::header::HeaderMap) {
        for val in headers.get_all(reqwest::header::SET_COOKIE) {
            let Ok(line) = val.to_str() else {
                continue;
            };
            let pair = line.split(';').next().unwrap_or("");
            if let Some((k, v)) = pair.split_once('=') {
                let key = k.trim();
                let value = v.trim();
                if !key.is_empty() && !value.is_empty() {
                    self.cookies.insert(key.to_string(), value.to_string());
                }
            }
        }
    }

    fn anon_username(&self) -> String {
        let xor_bytes: Vec<u8> = self
            .device_id
            .bytes()
            .enumerate()
            .map(|(i, b)| b ^ DEVICEID_XOR_KEY.as_bytes()[i % DEVICEID_XOR_KEY.len()])
            .collect();
        let digest = md5_bytes(&xor_bytes);
        let b64_md5 = base64::engine::general_purpose::STANDARD.encode(digest);
        let combined = format!("{} {b64_md5}", self.device_id);
        base64::engine::general_purpose::STANDARD.encode(combined.as_bytes())
    }

    fn build_eapi_params(&self, path: &str, mut params: Value) -> String {
        let req_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .to_string();
        let header = json!({
            "clientSign": self.client_sign,
            "osver": self.osver,
            "deviceId": self.device_id,
            "os": "pc",
            "appver": APP_VER,
            "requestId": req_id,
        });
        if let Some(obj) = params.as_object_mut() {
            obj.insert("header".to_string(), Value::String(header.to_string()));
            if !obj.contains_key("e_r") {
                obj.insert("e_r".to_string(), Value::Bool(true));
            }
        }
        let actual_path = path.replace("/eapi/", "/api/");
        let params_text = params.to_string();
        let digest = md5_hex(&format!("nobody{actual_path}use{params_text}md5forencrypt"));
        let data = format!("{actual_path}-36cd479b6b5-{params_text}-36cd479b6b5-{digest}");
        aes_ecb_pkcs7_encrypt_hex(&data, EAPI_KEY)
    }
}

static SESSION: LazyLock<Mutex<NetEaseSession>> =
    LazyLock::new(|| Mutex::new(NetEaseSession::new()));

pub struct NetEaseProvider;

async fn post_eapi_raw(ctx: &ProviderContext, path: &str, params: Value) -> Option<Value> {
    let (encrypted_hex, cookie_hdr) = {
        let s = SESSION.lock().await;
        (s.build_eapi_params(path, params), s.cookie_header())
    };
    let body = format!("params={encrypted_hex}");
    let mut req = ctx
        .client()
        .post(format!("{EAPI_HOST}{path}"))
        .header("Referer", "https://music.163.com/")
        .header("User-Agent", NETEASE_UA)
        .header("Accept", "*/*")
        .header("Content-Type", "application/x-www-form-urlencoded");
    if !cookie_hdr.is_empty() {
        req = req.header("Cookie", cookie_hdr);
    }
    let resp = req.body(body).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await.ok()?;
    SESSION.lock().await.capture_cookies(&headers);

    let decrypted = crate::lyrics::crypto::aes_ecb_pkcs7_decrypt_bytes(&bytes, EAPI_KEY)
        .and_then(|raw| String::from_utf8(raw).ok())
        .or_else(|| String::from_utf8(bytes.to_vec()).ok())?;
    serde_json::from_str::<Value>(&decrypted).ok()
}

async fn ensure_session(ctx: &ProviderContext) {
    let need_init = {
        let mut s = SESSION.lock().await;
        if s.initialized {
            false
        } else {
            s.reset_pre_cookies();
            true
        }
    };
    if !need_init {
        return;
    }
    let username = SESSION.lock().await.anon_username();
    if let Some(resp) = post_eapi_raw(ctx, REGISTER_PATH, json!({ "username": username })).await {
        let code = resp.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code == 200 {
            SESSION.lock().await.initialized = true;
        }
    }
}

async fn eapi_request(ctx: &ProviderContext, path: &str, params: Value) -> Option<Value> {
    ensure_session(ctx).await;
    let root = post_eapi_raw(ctx, path, params).await?;
    let code = root.get("code").and_then(Value::as_i64).unwrap_or(200);
    if code == 301 || code == 401 {
        let mut s = SESSION.lock().await;
        s.initialized = false;
        s.cookies.clear();
        return None;
    }
    Some(root)
}

fn parse_yrc_credits(yrc: &str) -> Vec<String> {
    let mut credits = Vec::new();
    for raw in yrc.lines() {
        let trimmed = raw.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(parts) = obj.get("c").and_then(Value::as_array) else {
            continue;
        };
        let joined: String = parts
            .iter()
            .filter_map(|p| p.get("tx").and_then(Value::as_str))
            .collect();
        let val = joined.trim();
        if !val.is_empty() {
            credits.push(val.to_string());
        }
    }
    credits
}

fn parse_yrc(text: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.starts_with('{') {
            continue;
        }
        let Some(caps) = YRC_LINE_RE.captures(line) else {
            continue;
        };
        let (Ok(line_start_ms), Ok(line_dur_ms)) = (caps[1].parse::<i64>(), caps[2].parse::<i64>())
        else {
            continue;
        };
        let line_end_ms = line_start_ms + line_dur_ms;
        let content = &caps[3];

        let mut marks = Vec::new();
        for wm in YRC_WORD_RE.captures_iter(content) {
            let m = wm.get(0).unwrap();
            let text_g = wm.get(3).unwrap();
            let (Ok(ws), Ok(wd)) = (wm[1].parse::<i64>(), wm[2].parse::<i64>()) else {
                continue;
            };
            marks.push((ws, ws + wd, m.start(), text_g.start()));
        }

        let mut words = Vec::new();
        let mut full = String::new();
        if let Some(first) = marks.first()
            && first.2 > 0
        {
            full.push_str(&content[..first.2]);
        }
        for i in 0..marks.len() {
            let (ws, we, _, seg_start) = marks[i];
            let seg_end = marks.get(i + 1).map(|n| n.2).unwrap_or(content.len());
            let word_text = &content[seg_start..seg_end];
            full.push_str(word_text);
            let trimmed = word_text.trim();
            if trimmed.is_empty() {
                continue;
            }
            let space = word_text.chars().last().is_some_and(char::is_whitespace);
            words.push(
                create_word_timing(ws as f64 / 1000.0, we as f64 / 1000.0, trimmed)
                    .with_space(space),
            );
        }
        if words.is_empty() && full.is_empty() && !content.is_empty() {
            full.push_str(content);
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

fn parse_mixed_or_lrc(text: &str) -> Vec<LyricLine> {
    let is_mixed = RICH_JSON_RE.is_match(text);
    let mut items: Vec<(i64, String)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if is_mixed && line.starts_with('{') {
            if let Ok(obj) = serde_json::from_str::<Value>(line)
                && let Some(parts) = obj.get("c").and_then(Value::as_array)
            {
                let start_ms = obj.get("t").and_then(Value::as_i64).unwrap_or(0);
                let txt: String = parts
                    .iter()
                    .filter_map(|p| p.get("tx").and_then(Value::as_str))
                    .collect();
                let t = txt.trim();
                if !t.is_empty() {
                    items.push((start_ms, t.to_string()));
                }
            }
            continue;
        }

        let mut starts = Vec::new();
        let mut last_end = 0_usize;
        for caps in LRC_TIME_RE.captures_iter(line) {
            let mm: i64 = caps[1].parse().unwrap_or(0);
            let ss: i64 = caps[2].parse().unwrap_or(0);
            let frac = caps.get(3).map(|m| m.as_str()).unwrap_or("0");
            let mut padded = frac.to_string();
            while padded.len() < 3 {
                padded.push('0');
            }
            let ms: i64 = padded[..3].parse().unwrap_or(0);
            starts.push((mm * 60 + ss) * 1000 + ms);
            last_end = caps.get(0).unwrap().end();
        }
        if starts.is_empty() {
            continue;
        }
        let body = line[last_end..].trim();
        if !body.is_empty() {
            for s in starts {
                items.push((s, body.to_string()));
            }
        }
    }
    items.sort_by_key(|(s, _)| *s);
    items
        .into_iter()
        .map(|(s, t)| LyricLine::new(s as f64 / 1000.0, t))
        .collect()
}

fn map_song(raw: &Value) -> Option<(i64, String, String, i64)> {
    let id = raw.get("id").and_then(Value::as_i64)?;
    let name = raw
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let artist = raw
        .get("ar")
        .or_else(|| raw.get("artists"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default();
    let dur_ms = raw
        .get("dt")
        .or_else(|| raw.get("duration"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Some((id, name, artist, dur_ms))
}

impl LyricsProvider for NetEaseProvider {
    fn id(&self) -> &'static str {
        "netease"
    }

    fn display_name(&self) -> &'static str {
        "NetEase"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let keyword = format!("{} {}", track.title, track.artist);
            let mut songs = Vec::new();

            if let Some(root) = eapi_request(
                ctx,
                "/eapi/search/song/list/page",
                json!({
                    "limit": "30",
                    "offset": "0",
                    "keyword": keyword,
                    "scene": "NORMAL",
                    "needCorrect": "true",
                }),
            )
            .await
                && let Some(resources) = root
                    .get("data")
                    .and_then(|d| d.get("resources"))
                    .and_then(Value::as_array)
            {
                for res in resources {
                    if let Some(simple) = res.get("baseInfo").and_then(|b| b.get("simpleSongData"))
                        && let Some(mapped) = map_song(simple)
                    {
                        songs.push(mapped);
                    }
                }
            }

            if songs.is_empty() {
                let form = format!(
                    "s={}&type=1&offset=0&limit=30",
                    urlencoding::encode(&keyword)
                );
                if let Ok(resp) = ctx
                    .client()
                    .post("https://music.163.com/api/cloudsearch/pc")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(form)
                    .send()
                    .await
                    && resp.status().is_success()
                    && let Ok(root) = resp.json::<Value>().await
                    && let Some(arr) = root
                        .get("result")
                        .and_then(|r| r.get("songs"))
                        .and_then(Value::as_array)
                {
                    songs.extend(arr.iter().filter_map(map_song));
                }
            }

            if songs.is_empty() {
                return Ok(Vec::new());
            }

            let mut scored: Vec<(i32, (i64, String, String, i64))> = songs
                .into_iter()
                .map(|s| {
                    let dur_s = if s.3 > 0 { s.3 / 1000 } else { 0 };
                    let sc = score_track_candidate(
                        &s.1,
                        &s.2,
                        dur_s,
                        None,
                        track,
                        ctx.custom_regex.as_ref(),
                    );
                    (sc, s)
                })
                .collect();
            scored.sort_by_key(|a| std::cmp::Reverse(a.0));

            let mut passed: Vec<_> = scored
                .iter()
                .filter(|(sc, _)| *sc >= SOFT_MIN)
                .map(|(_, s)| s.clone())
                .collect();
            if passed.is_empty()
                && let Some((_, first)) = scored.into_iter().next()
            {
                passed.push(first);
            }

            let mut out = Vec::new();
            for (song_id, c_title, c_artist, dur_ms) in passed {
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
                let Some(root) = eapi_request(
                    ctx,
                    "/eapi/song/lyric/v1",
                    json!({
                        "id": song_id,
                        "lv": "-1",
                        "tv": "-1",
                        "rv": "-1",
                        "yv": "-1",
                    }),
                )
                .await
                else {
                    continue;
                };

                let yrc = root
                    .get("yrc")
                    .and_then(|y| y.get("lyric"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let lrc = root
                    .get("lrc")
                    .and_then(|l| l.get("lyric"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let romalrc = root
                    .get("romalrc")
                    .and_then(|r| r.get("lyric"))
                    .and_then(Value::as_str)
                    .unwrap_or("");

                let lines = if !yrc.is_empty() {
                    let y = parse_yrc(yrc);
                    if y.is_empty() {
                        parse_mixed_or_lrc(lrc)
                    } else {
                        y
                    }
                } else {
                    parse_mixed_or_lrc(lrc)
                };
                if lines.is_empty() {
                    continue;
                }

                let mut lyrics = Lyrics::new(lines, "NetEase", true);
                lyrics.credits = parse_yrc_credits(yrc);
                if !romalrc.is_empty() {
                    let r_lines = parse_synced(romalrc);
                    if !r_lines.is_empty() {
                        lyrics.romanizations.insert("default".to_string(), r_lines);
                    }
                }
                let raw_src = if !yrc.is_empty() { yrc } else { lrc };
                let filtered_raw: Vec<&str> = raw_src
                    .lines()
                    .filter(|l| !l.trim().starts_with('{'))
                    .collect();
                lyrics.raw_format = Some(filtered_raw.join("\n"));
                lyrics.format_type = Some(if !yrc.is_empty() { "yrc" } else { "lrc" }.to_string());
                lyrics.source_url = Some(format!("https://music.163.com/song?id={song_id}"));

                let dur_s = if dur_ms > 0 { dur_ms / 1000 } else { 0 };
                let score = score_lyrics_candidate(
                    &c_title,
                    &c_artist,
                    dur_s,
                    &lyrics,
                    track,
                    ctx.custom_regex.as_ref(),
                );
                out.push(FetchedCandidate {
                    lyrics,
                    match_score: score.max(0),
                    is_blind: false,
                });
            }

            out.sort_by_key(|a| std::cmp::Reverse(a.match_score));
            Ok(out)
        })
    }
}
