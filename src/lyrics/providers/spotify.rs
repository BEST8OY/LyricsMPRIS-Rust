//! Spotify (`sp_dc` cookie + WebPlayer TOTP + `color-lyrics/v2`) provider (`SpotifyProvider.java`).

use crate::lyrics::parsers::parse_plain;
use crate::lyrics::providers::{
    BoxFuture, CHROME_DESKTOP_UA, FetchedCandidate, LyricsProvider, PreparedTrack, ProviderContext,
};
use crate::lyrics::types::{
    LyricLine, Lyrics, LyricsError, TrackInfo, TrackMatchInfo, WordTiming, create_word_timing,
};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha1::Sha1;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

const SECRETS_URL: &str = "https://raw.githubusercontent.com/xyloflake/spot-secrets-go/refs/heads/main/secrets/secretDict.json";
const TOKEN_URL: &str = "https://open.spotify.com/api/token";
const SERVER_TIME_URL: &str = "https://open.spotify.com/api/server-time";
const CLIENT_TOKEN_URL: &str = "https://clienttoken.spotify.com/v1/clienttoken";
const PARTNER_GRAPHQL_URL: &str = "https://api-partner.spotify.com/pathfinder/v2/query";
const LYRICS_URL: &str = "https://spclient.wg.spotify.com/color-lyrics/v2/track/";
const CLIENT_VERSION: &str = "1.2.57.183.g8c1b7eb0";
const SEARCH_HASHES: &[&str] = &[
    "0dff51c99e552b992377a2a6f40d213dc42b62db86ca0bcf16cf3934aec1aae6",
    "75bbf6bfcfdf85b8fc828417bfad92b7cd66bf7f556d85670f4da8292373ebec",
];
const FALLBACK_SECRET_DATA: &[i64] = &[
    99, 111, 47, 88, 49, 56, 118, 65, 52, 67, 50, 104, 117, 101, 55, 94, 95, 75, 94, 49, 69, 36,
    85, 64, 74, 60,
];
const FALLBACK_TOTP_VERSION: &str = "19";

#[derive(Default)]
struct SpotifySession {
    sp_dc: String,
    access_token: Option<String>,
    access_token_expiry_ms: u64,
    client_token: Option<String>,
    client_token_expiry_ms: u64,
    client_id: Option<String>,
    totp_secret: Option<String>,
    totp_version: Option<String>,
    last_secret_fetch_ms: u64,
}

static SESSION: LazyLock<Mutex<SpotifySession>> =
    LazyLock::new(|| Mutex::new(SpotifySession::default()));

pub struct SpotifyProvider;

fn now_ms() -> u64 {
    crate::lyrics::providers::now_ms()
}

fn decimal_string_from_ints(vals: &[i64]) -> String {
    vals.iter().map(|v| v.to_string()).collect()
}

fn generate_totp(timestamp_ms: u64, secret: &str) -> String {
    let counter = (timestamp_ms / 1000) / 30;
    let counter_bytes = counter.to_be_bytes();
    let mut mac =
        <Hmac<Sha1> as Mac>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(&counter_bytes);
    let hash = mac.finalize().into_bytes();
    let offset = (hash[hash.len() - 1] & 0x0f) as usize;
    let binary = (((hash[offset] & 0x7f) as u32) << 24)
        | ((hash[offset + 1] as u32) << 16)
        | ((hash[offset + 2] as u32) << 8)
        | (hash[offset + 3] as u32);
    format!("{:06}", binary % 1_000_000)
}

async fn ensure_totp_secrets(ctx: &ProviderContext, session: &mut SpotifySession) {
    let now = now_ms();
    if session.totp_secret.is_some()
        && session.totp_version.is_some()
        && now.saturating_sub(session.last_secret_fetch_ms) < 3_600_000
    {
        return;
    }

    for _ in 0..=2 {
        if let Ok(resp) = ctx.client().get(SECRETS_URL).send().await
            && resp.status().is_success()
            && let Ok(Value::Object(map)) = resp.json::<Value>().await
        {
            let best = map
                .iter()
                .filter_map(|(k, v)| Some((k.parse::<i64>().ok()?, k, v.as_array()?)))
                .max_by_key(|(vnum, _, _)| *vnum);
            if let Some((_, ver, arr)) = best {
                let xored: Vec<i64> = arr
                    .iter()
                    .enumerate()
                    .filter_map(|(i, val)| Some(val.as_i64()? ^ (((i as i64) % 33) + 9)))
                    .collect();
                if xored.len() == arr.len() && !xored.is_empty() {
                    let prev_ver = session.totp_version.clone();
                    session.totp_version = Some(ver.clone());
                    session.totp_secret = Some(decimal_string_from_ints(&xored));
                    if prev_ver.as_deref() != Some(ver.as_str()) {
                        session.access_token = None;
                    }
                    session.last_secret_fetch_ms = now;
                    return;
                }
            }
        }
    }

    if session.totp_secret.is_none() || session.totp_version.is_none() {
        session.totp_secret = Some(decimal_string_from_ints(FALLBACK_SECRET_DATA));
        session.totp_version = Some(FALLBACK_TOTP_VERSION.to_string());
        session.last_secret_fetch_ms = now;
    }
}

async fn get_server_time_ms(ctx: &ProviderContext, sp_dc: &str) -> u64 {
    let cookie = format!("sp_dc={sp_dc}");
    if let Ok(resp) = ctx
        .client()
        .get(SERVER_TIME_URL)
        .header("User-Agent", CHROME_DESKTOP_UA)
        .header("Origin", "https://open.spotify.com/")
        .header("Referer", "https://open.spotify.com/")
        .header("Cookie", &cookie)
        .send()
        .await
        && resp.status().is_success()
        && let Ok(json) = resp.json::<Value>().await
        && let Some(st) = json.get("serverTime").and_then(Value::as_u64)
        && st > 0
    {
        return st * 1000;
    }
    now_ms()
}

async fn get_access_token(ctx: &ProviderContext, sp_dc: &str) -> Option<(String, Option<String>)> {
    let mut sess = SESSION.lock().await;
    if sess.sp_dc != sp_dc {
        *sess = SpotifySession {
            sp_dc: sp_dc.to_string(),
            ..Default::default()
        };
    }

    let now = now_ms();
    if sess.access_token.is_none() || now >= sess.access_token_expiry_ms {
        ensure_totp_secrets(ctx, &mut sess).await;
        let secret = sess.totp_secret.clone()?;
        let ver = sess.totp_version.clone()?;
        let st_ms = get_server_time_ms(ctx, sp_dc).await;
        let totp = generate_totp(st_ms, &secret);
        let url = format!(
            "{TOKEN_URL}?reason=init&productType=web-player&totp={totp}&totpVer={ver}&totpServer={totp}"
        );
        let cookie = format!("sp_dc={sp_dc}");
        let resp = ctx
            .client()
            .get(&url)
            .header("User-Agent", CHROME_DESKTOP_UA)
            .header("Accept", "application/json")
            .header("App-Platform", "WebPlayer")
            .header("Referer", "https://open.spotify.com/")
            .header("Cookie", &cookie)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: Value = resp.json().await.ok()?;
        let token = json
            .get("accessToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())?
            .to_string();
        let exp_ms = json
            .get("accessTokenExpirationTimestampMs")
            .and_then(Value::as_u64)
            .filter(|&ms| ms > now)
            .map(|ms| ms.saturating_sub(60_000))
            .unwrap_or(now + 3_000_000);
        sess.access_token = Some(token);
        sess.access_token_expiry_ms = exp_ms;
        if let Some(cid) = json
            .get("clientId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            sess.client_id = Some(cid.to_string());
        }
    }

    let now = now_ms();
    if (sess.client_token.is_none() || now >= sess.client_token_expiry_ms)
        && let Some(cid) = sess.client_id.clone()
    {
        let device_id = Uuid::new_v4().to_string();
        let req_body = json!({
            "client_data": {
                "client_version": CLIENT_VERSION,
                "client_id": cid,
                "js_sdk_data": {
                    "device_brand": "unknown",
                    "device_model": "unknown",
                    "os": "linux",
                    "os_version": "unknown",
                    "device_id": device_id,
                    "device_type": "computer",
                }
            }
        });
        if let Ok(resp) = ctx
            .client()
            .post(CLIENT_TOKEN_URL)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", CHROME_DESKTOP_UA)
            .header("Origin", "https://open.spotify.com")
            .header("Referer", "https://open.spotify.com")
            .json(&req_body)
            .send()
            .await
            && resp.status().is_success()
            && let Ok(json) = resp.json::<Value>().await
            && let Some(gt) = json.get("granted_token")
            && let Some(tok) = gt
                .get("token")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            let expires = gt
                .get("expires_after")
                .and_then(Value::as_u64)
                .unwrap_or(3600);
            sess.client_token_expiry_ms = now + expires.saturating_mul(1000).saturating_sub(60_000);
            sess.client_token = Some(tok.to_string());
        }
    }

    Some((sess.access_token.clone()?, sess.client_token.clone()))
}

struct SearchHit {
    id: String,
    title: String,
    artists: Vec<String>,
}

fn artist_names(node: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(items) = node
        .get("artists")
        .and_then(|a| a.get("items"))
        .and_then(Value::as_array)
    {
        for item in items {
            if let Some(name) = item
                .get("profile")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .or_else(|| item.get("name").and_then(Value::as_str))
            {
                out.push(name.to_string());
            }
        }
    }
    if out.is_empty()
        && let Some(edges) = node
            .get("artists")
            .and_then(|a| a.get("edges"))
            .and_then(Value::as_array)
    {
        for e in edges {
            if let Some(name) = e
                .get("node")
                .and_then(|n| n.get("name"))
                .and_then(Value::as_str)
            {
                out.push(name.to_string());
            }
        }
    }
    out
}

fn parse_search_result(root: &Value) -> Option<SearchHit> {
    let data = root.get("data")?;
    let items = data
        .get("searchV2")
        .and_then(|s| s.get("tracks"))
        .and_then(|t| t.get("items"))
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .or_else(|| {
            data.get("search")
                .and_then(|s| s.get("tracks"))
                .and_then(|t| t.get("items"))
                .and_then(Value::as_array)
        })?;

    for wrapper in items {
        let node = wrapper.get("data").unwrap_or(wrapper);
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                node.get("uri")
                    .and_then(Value::as_str)
                    .and_then(|u| u.strip_prefix("spotify:track:"))
                    .map(str::to_string)
            });
        if let Some(id) = id
            && !id.is_empty()
        {
            let title = node
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Some(SearchHit {
                id,
                title,
                artists: artist_names(node),
            });
        }
    }
    None
}

fn parse_start_time_secs(obj: &Value) -> f64 {
    obj.get("startTimeMs")
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| v.as_i64())
        })
        .map(|ms| ms as f64 / 1000.0)
        .unwrap_or(-1.0)
}

fn fill_estimated_ends(lines: &mut [LyricLine]) {
    let len = lines.len();
    for i in 0..len {
        if lines[i].end.is_some() {
            continue;
        }
        let next = (i + 1 < len).then(|| lines[i + 1].time);
        let est =
            crate::lyrics::parsers::lrc::estimate_end_secs(lines[i].time, next, &lines[i].text);
        lines[i].end = Some(est);
    }
}

fn parse_syllables(arr: &[Value], line_text: &str) -> Vec<WordTiming> {
    let chars: Vec<(usize, char)> = line_text.char_indices().collect();
    let mut char_pos = 0_usize;
    let mut words = Vec::with_capacity(arr.len());

    for syl in arr {
        let start = parse_start_time_secs(syl);
        if start < 0.0 {
            continue;
        }
        let end = syl
            .get("endTimeMs")
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .map(|ms| ms as f64 / 1000.0)
            .unwrap_or(start);
        let num_chars = syl.get("numChars").and_then(Value::as_i64).unwrap_or(0) as usize;
        if num_chars == 0 {
            continue;
        }
        while char_pos < chars.len() && chars[char_pos].1.is_whitespace() {
            char_pos += 1;
        }
        if char_pos >= chars.len() {
            break;
        }
        let start_byte = chars[char_pos].0;
        let next_pos = (char_pos + num_chars).min(chars.len());
        let end_byte = if next_pos < chars.len() {
            chars[next_pos].0
        } else {
            line_text.len()
        };
        let word_text = &line_text[start_byte..end_byte];
        let space_after = next_pos < chars.len() && chars[next_pos].1.is_whitespace();
        words.push(create_word_timing(start, end, word_text).with_space(space_after));
        char_pos = next_pos;
    }
    words
}

fn parse_lyrics_json(root: &Value, raw_json: &str, source_url: &str) -> Option<Lyrics> {
    let lyrics_obj = root.get("lyrics")?;
    let sync_type = lyrics_obj
        .get("syncType")
        .and_then(Value::as_str)
        .unwrap_or("UNSYNCED");
    let upstream = lyrics_obj
        .get("provider")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let provider_name = upstream
        .map(|p| format!("Spotify (via {p})"))
        .unwrap_or_else(|| "Spotify".to_string());
    let lines_arr = lyrics_obj.get("lines").and_then(Value::as_array)?;
    if lines_arr.is_empty() {
        return None;
    }

    if sync_type == "UNSYNCED" {
        let plain: String = lines_arr
            .iter()
            .filter_map(|l| l.get("words").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let lines = parse_plain(&plain);
        if lines.is_empty() {
            return None;
        }
        let mut l = Lyrics::new(lines, provider_name, false);
        l.raw_format = Some(raw_json.to_string());
        l.format_type = Some("sp.json".to_string());
        l.source_url = Some(source_url.to_string());
        return Some(l);
    }

    let mut lines = Vec::with_capacity(lines_arr.len());
    for obj in lines_arr {
        let start = parse_start_time_secs(obj);
        if start < 0.0 {
            continue;
        }
        let text = obj
            .get("words")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let words = if sync_type == "SYLLABLE_SYNCED" {
            obj.get("syllables")
                .and_then(Value::as_array)
                .map(|a| parse_syllables(a, text))
                .filter(|w| !w.is_empty())
        } else {
            None
        };
        let mut line = LyricLine::new(start, text);
        line.words = words;
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    fill_estimated_ends(&mut lines);
    let mut l = Lyrics::new(lines, provider_name, true);
    l.raw_format = Some(raw_json.to_string());
    l.format_type = Some("sp.json".to_string());
    l.source_url = Some(source_url.to_string());
    Some(l)
}

impl LyricsProvider for SpotifyProvider {
    fn id(&self) -> &'static str {
        "spotify"
    }

    fn display_name(&self) -> &'static str {
        "Spotify"
    }

    fn is_configured(&self, ctx: &ProviderContext) -> bool {
        ctx.settings
            .tokens
            .spotify_sp_dc
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let Some(sp_dc) = ctx
                .settings
                .tokens
                .spotify_sp_dc
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Ok(Vec::new());
            };

            let Some((access_token, client_token)) = get_access_token(ctx, sp_dc).await else {
                return Ok(Vec::new());
            };

            let (track_id, hit_meta): (String, Option<(String, Vec<String>)>) = if let Some(sid) =
                ids.track_spotify_ids
                    .first()
                    .map(String::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            {
                (sid.to_string(), None)
            } else {
                let query = format!("{} {}", track.title, track.artist);
                let mut found: Option<SearchHit> = None;
                for hash in SEARCH_HASHES {
                    let body = json!({
                        "extensions": {
                            "persistedQuery": {
                                "sha256Hash": hash,
                                "version": 1,
                            }
                        },
                        "operationName": "searchDesktop",
                        "variables": {
                            "searchTerm": query,
                            "limit": 5,
                            "numberOfTopResults": 5,
                            "offset": 0,
                        }
                    });
                    for _attempt in 0..=2 {
                        let mut req = ctx
                            .client()
                            .post(PARTNER_GRAPHQL_URL)
                            .header("Authorization", format!("Bearer {access_token}"))
                            .header("User-Agent", CHROME_DESKTOP_UA)
                            .header("Content-Type", "application/json")
                            .header("Accept", "application/json")
                            .header("app-platform", "WebPlayer");
                        if let Some(ct) = client_token.as_deref() {
                            req = req.header("client-token", ct);
                        }
                        let Ok(resp) = req.json(&body).send().await else {
                            break;
                        };
                        if resp.status().as_u16() == 429 {
                            let wait_s = resp
                                .headers()
                                .get("Retry-After")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.trim().parse::<u64>().ok())
                                .unwrap_or(3)
                                .min(30);
                            tokio::time::sleep(Duration::from_secs(wait_s)).await;
                            continue;
                        }
                        if !resp.status().is_success() {
                            break;
                        }
                        if let Ok(json) = resp.json::<Value>().await {
                            found = parse_search_result(&json);
                        }
                        break;
                    }
                    if found.is_some() {
                        break;
                    }
                }
                let Some(hit) = found else {
                    return Ok(Vec::new());
                };
                (hit.id, Some((hit.title, hit.artists)))
            };

            let source_url = format!("https://open.spotify.com/track/{track_id}");
            let lyrics_url = format!("{LYRICS_URL}{track_id}?format=json&market=from_token");
            let mut active_token = access_token;
            let mut resp = ctx
                .client()
                .get(&lyrics_url)
                .header("Authorization", format!("Bearer {active_token}"))
                .header("User-Agent", CHROME_DESKTOP_UA)
                .header("app-platform", "WebPlayer")
                .send()
                .await?;
            if resp.status().as_u16() == 401 {
                {
                    let mut sess = SESSION.lock().await;
                    sess.access_token = None;
                    sess.access_token_expiry_ms = 0;
                    sess.client_token = None;
                }
                if let Some((fresh_token, _)) = get_access_token(ctx, sp_dc).await {
                    active_token = fresh_token;
                    resp = ctx
                        .client()
                        .get(&lyrics_url)
                        .header("Authorization", format!("Bearer {active_token}"))
                        .header("User-Agent", CHROME_DESKTOP_UA)
                        .header("app-platform", "WebPlayer")
                        .send()
                        .await?;
                }
            }
            if !resp.status().is_success() {
                return Ok(Vec::new());
            }
            let raw_json = resp.text().await?;
            let Ok(json) = serde_json::from_str::<Value>(&raw_json) else {
                return Ok(Vec::new());
            };
            let Some(lyrics) = parse_lyrics_json(&json, &raw_json, &source_url) else {
                return Ok(Vec::new());
            };
            if lyrics.is_empty() {
                return Ok(Vec::new());
            }

            if let Some((ctitle, cartists)) = hit_meta {
                let prepared = PreparedTrack::new(track, ctx.custom_regex.as_ref());
                let cand_artists: Vec<&str> = if cartists.is_empty() {
                    vec![""]
                } else {
                    cartists.iter().map(String::as_str).collect()
                };
                let mut best = None;
                for a in cand_artists {
                    let v = prepared.evaluate(&ctitle, a, 0, None);
                    if best
                        .as_ref()
                        .is_none_or(|cur: &crate::lyrics::providers::MatchVerdict| {
                            v.score > cur.score
                        })
                    {
                        best = Some(v);
                    }
                }
                if let Some(v) = best
                    && v.is_high_match()
                {
                    return Ok(vec![FetchedCandidate {
                        lyrics,
                        match_score: v.score,
                        is_blind: true,
                    }]);
                }
                return Ok(Vec::new());
            }

            Ok(FetchedCandidate::blind(lyrics).into_iter().collect())
        })
    }
}
