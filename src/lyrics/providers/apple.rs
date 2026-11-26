//! Apple Music (`amp-api` + `media-user-token` or unauthenticated Paxsenix / Lyrically fallback) (`AppleMusicProvider.java`).

use crate::lyrics::parsers::{parse_synced_with_credits, parse_ttml};
use crate::lyrics::providers::{
    BoxFuture, CHROME_DESKTOP_UA, FetchedCandidate, ITUNES_SEARCH_URL, LyricsProvider,
    MAX_CANDIDATES, ProviderContext, SOFT_MIN, score_lyrics_candidate, score_track_candidate,
};
use crate::lyrics::types::{Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use tokio::sync::Mutex;

const BROWSE_URL: &str = "https://music.apple.com";
const API_BASE: &str = "https://amp-api.music.apple.com/v1/catalog/";
const LYRICALLY_BASE: &str = "https://lyrics.paxsenix.org";

static JS_BUNDLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"/assets/index~[^/"'\s]+\.js"#).unwrap());
static JWT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"eyJ[A-Za-z0-9\-_=]+\.[A-Za-z0-9\-_=]+\.[A-Za-z0-9\-_=]+").unwrap()
});

#[derive(Default)]
struct AppleSession {
    dev_token: Option<String>,
    storefront: Option<String>,
    language: Option<String>,
}

static SESSION: LazyLock<Mutex<AppleSession>> =
    LazyLock::new(|| Mutex::new(AppleSession::default()));

pub struct AppleMusicProvider;

async fn fetch_dev_token(ctx: &ProviderContext) -> Option<String> {
    let resp = ctx
        .client()
        .get(BROWSE_URL)
        .header("User-Agent", CHROME_DESKTOP_UA)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let html = resp.text().await.ok()?;
    let js_path = JS_BUNDLE_RE.find(&html)?.as_str();
    let js_resp = ctx
        .client()
        .get(format!("{BROWSE_URL}{js_path}"))
        .header("User-Agent", CHROME_DESKTOP_UA)
        .send()
        .await
        .ok()?;
    if !js_resp.status().is_success() {
        return None;
    }
    let js = js_resp.text().await.ok()?;
    JWT_RE.find(&js).map(|m| m.as_str().to_string())
}

async fn apple_get(
    ctx: &ProviderContext,
    url: &str,
    dev_token: &str,
    user_token: &str,
    lang: &str,
) -> Option<Value> {
    let resp = ctx
        .client()
        .get(url)
        .header("User-Agent", CHROME_DESKTOP_UA)
        .header("Authorization", format!("Bearer {dev_token}"))
        .header("media-user-token", user_token)
        .header("origin", "https://music.apple.com")
        .header("referer", "https://music.apple.com/")
        .header("accept", "application/json")
        .header("accept-language", format!("{lang},en;q=0.9"))
        .header("Cookie", format!("media-user-token={user_token}"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

async fn resolve_session(
    ctx: &ProviderContext,
    user_token: &str,
) -> Option<(String, String, String)> {
    let mut sess = SESSION.lock().await;
    if sess.dev_token.is_none() {
        sess.dev_token = fetch_dev_token(ctx).await;
    }
    let dev_token = sess.dev_token.clone()?;

    if sess.storefront.is_none()
        && let Some(root) = apple_get(
            ctx,
            "https://amp-api.music.apple.com/v1/me/storefront",
            &dev_token,
            user_token,
            "en-US",
        )
        .await
        && let Some(first) = root
            .get("data")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
    {
        if let Some(id) = first.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            sess.storefront = Some(id.to_string());
        }
        if let Some(lang) = first
            .get("attributes")
            .and_then(|a| a.get("defaultLanguageTag"))
            .and_then(Value::as_str)
            && !lang.is_empty()
        {
            sess.language = Some(lang.to_string());
        }
    }

    let storefront = sess.storefront.clone().unwrap_or_else(|| "us".to_string());
    let language = sess.language.clone().unwrap_or_else(|| "en-US".to_string());
    Some((dev_token, storefront, language))
}

fn extract_ttml_from_attrs(attrs: &Value) -> Option<&str> {
    attrs
        .get("ttmlLocalizations")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            attrs
                .get("ttml")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
}

async fn fetch_apple_song_lyrics(
    ctx: &ProviderContext,
    dev_token: &str,
    user_token: &str,
    storefront: &str,
    language: &str,
    song_id: &str,
) -> Option<Lyrics> {
    let source_url = format!("https://music.apple.com/song/{song_id}");
    let l_enc = urlencoding::encode(language);

    let url_syl = format!(
        "{API_BASE}{storefront}/songs/{song_id}/syllable-lyrics?l={l_enc}&extend=ttmlLocalizations"
    );
    let parse_ep = |root: Option<Value>| -> Option<Lyrics> {
        let root = root?;
        let attrs = root.get("data")?.as_array()?.first()?.get("attributes")?;
        let ttml = extract_ttml_from_attrs(attrs)?;
        let mut l = parse_ttml(ttml, "Apple")?;
        l.source_url = Some(source_url.clone());
        Some(l)
    };

    let syl_lyrics = parse_ep(apple_get(ctx, &url_syl, dev_token, user_token, language).await);
    if syl_lyrics.as_ref().is_some_and(|l| l.sync_rank() >= 2) {
        return syl_lyrics;
    }

    let url_ded =
        format!("{API_BASE}{storefront}/songs/{song_id}/lyrics?l={l_enc}&extend=ttmlLocalizations");
    let url_inc = format!(
        "{API_BASE}{storefront}/songs/{song_id}?include[songs]=albums,lyrics,syllable-lyrics&l={l_enc}"
    );

    let (r_ded, r_inc) = tokio::join!(
        apple_get(ctx, &url_ded, dev_token, user_token, language),
        apple_get(ctx, &url_inc, dev_token, user_token, language),
    );

    let parse_inc = |root: Option<Value>| -> Option<Lyrics> {
        let root = root?;
        let rels = root
            .get("data")?
            .as_array()?
            .first()?
            .get("relationships")?;
        let ttml = ["syllable-lyrics", "lyrics"].iter().find_map(|key| {
            let attrs = rels
                .get(*key)?
                .get("data")?
                .as_array()?
                .first()?
                .get("attributes")?;
            extract_ttml_from_attrs(attrs)
        })?;
        let mut l = parse_ttml(ttml, "Apple")?;
        l.source_url = Some(source_url.clone());
        Some(l)
    };

    let mut best: Option<Lyrics> = syl_lyrics;
    for cand in [parse_ep(r_ded), parse_inc(r_inc)].into_iter().flatten() {
        if best
            .as_ref()
            .is_none_or(|b| cand.sync_rank() > b.sync_rank())
        {
            best = Some(cand);
        }
    }
    best
}

async fn fetch_paxsenix_url(ctx: &ProviderContext, url: &str) -> Option<String> {
    let resp = ctx
        .client()
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: Value = resp.json().await.ok()?;
    for key in ["ttmlContent", "elrcMultiPerson", "elrc"] {
        if let Some(s) = root
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Some(s.to_string());
        }
    }
    None
}

async fn fetch_via_lyrically(ctx: &ProviderContext, track: &TrackInfo) -> Vec<FetchedCandidate> {
    if track.title.is_empty() || track.artist.is_empty() {
        return Vec::new();
    }
    let term = format!("{} {}", track.title, track.artist);
    let url = format!(
        "{ITUNES_SEARCH_URL}?term={}&entity=song&limit=5",
        urlencoding::encode(&term)
    );
    let Ok(resp) = ctx.client().get(&url).send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(root) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let Some(results) = root.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };

    let t_lower = track.title.trim().to_lowercase();
    let a_lower = track.artist.trim().to_lowercase();
    let mut best_item: Option<(i32, bool, &Value)> = None;
    for item in results {
        let track_id = item.get("trackId").and_then(Value::as_i64).unwrap_or(0);
        if track_id == 0 {
            continue;
        }
        let it = item.get("trackName").and_then(Value::as_str).unwrap_or("");
        let ia = item.get("artistName").and_then(Value::as_str).unwrap_or("");
        let dur = item
            .get("trackTimeMillis")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            / 1000;
        let sc = score_track_candidate(it, ia, dur, None, track, ctx.custom_regex.as_ref());
        if sc < SOFT_MIN {
            continue;
        }
        let exact = it.to_lowercase().contains(&t_lower) && ia.to_lowercase().contains(&a_lower);
        if exact
            || best_item
                .as_ref()
                .is_none_or(|(bs, bex, _)| !*bex && sc > *bs)
        {
            best_item = Some((sc, exact, item));
            if exact {
                break;
            }
        }
    }

    let Some((_, _, item)) = best_item else {
        return Vec::new();
    };
    let track_id = item.get("trackId").and_then(Value::as_i64).unwrap_or(0);
    let base = format!("{LYRICALLY_BASE}/apple-music/lyrics?id={track_id}");
    let body = match fetch_paxsenix_url(ctx, &base).await {
        Some(s) => s,
        None => match fetch_paxsenix_url(ctx, &format!("{base}&skip_cache=true")).await {
            Some(s) => s,
            None => return Vec::new(),
        },
    };

    let source_url = format!("https://music.apple.com/song/{track_id}");
    let mut lyrics = if body.trim_start().starts_with('<') {
        parse_ttml(&body, "Apple (via Lyrically)")
    } else {
        let (lines, credits) = parse_synced_with_credits(&body);
        if lines.is_empty() {
            None
        } else {
            let mut l = Lyrics::new(lines, "Apple (via Lyrically)", true);
            l.credits = credits;
            l.raw_format = Some(body);
            l.format_type = Some("elrc".to_string());
            Some(l)
        }
    };
    let Some(mut l) = lyrics.take() else {
        return Vec::new();
    };
    l.source_url = Some(source_url);

    let it = item.get("trackName").and_then(Value::as_str).unwrap_or("");
    let ia = item.get("artistName").and_then(Value::as_str).unwrap_or("");
    let dur = item
        .get("trackTimeMillis")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        / 1000;
    FetchedCandidate::with_metadata(l, it, ia, dur, None, track, ctx)
        .into_iter()
        .collect()
}

impl LyricsProvider for AppleMusicProvider {
    fn id(&self) -> &'static str {
        "apple"
    }

    fn display_name(&self) -> &'static str {
        "Apple"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let user_token = ctx
                .settings
                .tokens
                .apple_media_user_token
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());

            if let Some(utok) = user_token
                && let Some((dev_token, storefront, language)) = resolve_session(ctx, utok).await
            {
                let term = format!("{} {}", track.title, track.artist);
                let search_url = format!(
                    "{API_BASE}{storefront}/search?term={}&types=songs&limit=5",
                    urlencoding::encode(&term)
                );
                if let Some(root) = apple_get(ctx, &search_url, &dev_token, utok, &language).await
                    && let Some(items) = root
                        .get("results")
                        .and_then(|r| r.get("songs"))
                        .and_then(|s| s.get("data"))
                        .and_then(Value::as_array)
                    && !items.is_empty()
                {
                    let prepared = crate::lyrics::providers::PreparedTrack::new(
                        track,
                        ctx.custom_regex.as_ref(),
                    );
                    let mut scored: Vec<(i32, &Value)> = items
                        .iter()
                        .map(|s| {
                            let attrs = s.get("attributes");
                            let t = attrs
                                .and_then(|a| a.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let a = attrs
                                .and_then(|a| a.get("artistName"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let alb = attrs
                                .and_then(|a| a.get("albumName"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let d = attrs
                                .and_then(|a| a.get("durationInMillis"))
                                .and_then(Value::as_i64)
                                .unwrap_or(0)
                                / 1000;
                            let ver = prepared.evaluate(t, a, d, (!alb.is_empty()).then_some(alb));
                            (ver.score, s)
                        })
                        .collect();
                    scored.sort_by_key(|a| std::cmp::Reverse(a.0));

                    let mut out = Vec::new();
                    for (_, song) in scored {
                        if out.len() >= MAX_CANDIDATES {
                            break;
                        }
                        let Some(song_id) = song.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(lyrics) = fetch_apple_song_lyrics(
                            ctx,
                            &dev_token,
                            utok,
                            &storefront,
                            &language,
                            song_id,
                        )
                        .await
                        else {
                            continue;
                        };
                        let attrs = song.get("attributes");
                        let t = attrs
                            .and_then(|a| a.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let a = attrs
                            .and_then(|a| a.get("artistName"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let d = attrs
                            .and_then(|a| a.get("durationInMillis"))
                            .and_then(Value::as_i64)
                            .unwrap_or(0)
                            / 1000;
                        let score = score_lyrics_candidate(
                            t,
                            a,
                            d,
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
                    if !out.is_empty() {
                        out.sort_by_key(|a| std::cmp::Reverse(a.match_score));
                        return Ok(out);
                    }
                }
            }

            Ok(fetch_via_lyrically(ctx, track).await)
        })
    }
}
