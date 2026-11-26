//! Genius (`genius.com`) plain-text lyrics & credits scraper (`GeniusProvider.java`).

use crate::lyrics::parsers::parse_plain;
use crate::lyrics::providers::{
    BoxFuture, CHROME_DESKTOP_UA, FetchedCandidate, LyricsProvider, ProviderContext, SOFT_MIN,
    score_track_candidate, throttle_ms,
};
use crate::lyrics::types::{Lyrics, LyricsError, TrackInfo, TrackMatchInfo};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;

const SEARCH_URL: &str = "https://genius.com/api/search/multi?q=";
const SONG_RECORD_URL: &str = "https://genius.com/api/songs/";
const THROTTLE_MS: u64 = 300;
static LAST_REQ_MS: AtomicU64 = AtomicU64::new(0);

static CONTAINER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<div[^>]*data-lyrics-container=["']true["'][^>]*>"#).unwrap()
});
static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(/?)([a-zA-Z0-9]+)([^>]*)>").unwrap());
static BLANK_RUN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

const NESTING_TAGS: &[&str] = &[
    "div", "span", "a", "i", "b", "em", "strong", "p", "button", "ul", "li", "svg", "path",
    "label", "section",
];

pub struct GeniusProvider;

fn is_song(hit: &Value) -> bool {
    let t = hit
        .get("_type")
        .or_else(|| hit.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    t.is_empty() || t == "song"
}

fn primary_artist_name(hit: &Value) -> &str {
    hit.get("primary_artist")
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

pub fn extract_lyrics_text_from_html(html: &str) -> Option<String> {
    if html.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut from = 0_usize;

    while let Some(open) = CONTAINER_RE.find(&html[from..]) {
        let start_at = from + open.end();
        let mut container = String::new();
        let mut depth = 1_i32;
        let mut excluded_at = -1_i32;
        let mut at = start_at;

        while depth > 0 {
            let Some(caps) = TAG_RE.captures(&html[at..]) else {
                break;
            };
            let m = caps.get(0).unwrap();
            if excluded_at < 0 {
                container.push_str(&html[at..at + m.start()]);
            }
            at += m.end();

            let closing = caps.get(1).is_some_and(|g| g.as_str() == "/");
            let name = caps[2].to_ascii_lowercase();
            let attrs = &caps[3];

            if name == "br" {
                if excluded_at < 0 {
                    container.push('\n');
                }
                continue;
            }
            if !NESTING_TAGS.contains(&name.as_str()) || attrs.trim().ends_with('/') {
                continue;
            }
            if closing {
                depth -= 1;
                if depth == excluded_at {
                    excluded_at = -1;
                }
            } else {
                if excluded_at < 0 && attrs.contains("data-exclude-from-selection=\"true\"") {
                    excluded_at = depth;
                }
                depth += 1;
            }
        }

        if !container.is_empty() {
            out.push_str(&container);
            out.push('\n');
        }
        from = at.max(start_at);
    }

    if out.is_empty() {
        return None;
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&quot;", "\"");
    let trimmed_lines: Vec<&str> = decoded.split('\n').map(str::trim).collect();
    let joined = trimmed_lines.join("\n");
    let collapsed = BLANK_RUN_RE.replace_all(&joined, "\n\n").trim().to_string();
    (!collapsed.is_empty()).then_some(collapsed)
}

fn add_credit(credits: &mut Vec<String>, label: &str, artists: Option<&Vec<Value>>) {
    let Some(arr) = artists else {
        return;
    };
    let names: Vec<&str> = arr
        .iter()
        .filter_map(|a| a.get("name").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return;
    }
    let joined = names.join(", ");
    if label.is_empty() {
        credits.push(joined);
    } else {
        credits.push(format!("{label}: {joined}"));
    }
}

fn extract_credits(record: &Value) -> Vec<String> {
    let mut credits = Vec::new();
    if let Some(rel) = record
        .get("release_date_for_display")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        credits.push(format!("Released on: {rel}"));
    }
    add_credit(
        &mut credits,
        "Producers",
        record.get("producer_artists").and_then(Value::as_array),
    );
    add_credit(
        &mut credits,
        "Writer",
        record.get("writer_artists").and_then(Value::as_array),
    );
    if let Some(perfs) = record.get("custom_performances").and_then(Value::as_array) {
        for p in perfs {
            let label = p.get("label").and_then(Value::as_str).unwrap_or("").trim();
            add_credit(
                &mut credits,
                label,
                p.get("artists").and_then(Value::as_array),
            );
        }
    }
    credits
}

async fn genius_get(ctx: &ProviderContext, url: &str) -> Option<String> {
    for attempt in 0..=1 {
        throttle_ms(&LAST_REQ_MS, THROTTLE_MS).await;
        let Ok(resp) = ctx
            .client()
            .get(url)
            .header("User-Agent", CHROME_DESKTOP_UA)
            .header("Referer", "https://genius.com/")
            .send()
            .await
        else {
            if attempt == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
            return None;
        };
        let status = resp.status().as_u16();
        if status == 200 {
            return resp.text().await.ok();
        }
        if attempt == 0 && (status == 403 || status == 429) {
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            continue;
        }
        return None;
    }
    None
}

impl LyricsProvider for GeniusProvider {
    fn id(&self) -> &'static str {
        "genius"
    }

    fn display_name(&self) -> &'static str {
        "Genius"
    }

    fn is_configured(&self, ctx: &ProviderContext) -> bool {
        ctx.settings.allow_plain
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            let term = format!("{} {}", track.artist, track.title);
            let search_url = format!("{SEARCH_URL}{}", urlencoding::encode(&term));
            let Some(body) = genius_get(ctx, &search_url).await else {
                return Ok(Vec::new());
            };
            let Ok(root) = serde_json::from_str::<Value>(&body) else {
                return Ok(Vec::new());
            };
            let Some(sections) = root
                .get("response")
                .and_then(|r| r.get("sections"))
                .and_then(Value::as_array)
            else {
                return Ok(Vec::new());
            };

            let mut best_hit: Option<Value> = None;
            'outer: for section in sections {
                let stype = section.get("type").and_then(Value::as_str).unwrap_or("");
                if stype != "top_hit" && stype != "song" {
                    continue;
                }
                let Some(hits) = section.get("hits").and_then(Value::as_array) else {
                    continue;
                };
                for wrapper in hits {
                    let Some(res) = wrapper.get("result") else {
                        continue;
                    };
                    if !is_song(res) {
                        continue;
                    }
                    let t = res.get("title").and_then(Value::as_str).unwrap_or("");
                    let a = primary_artist_name(res);
                    if score_track_candidate(t, a, 0, None, track, ctx.custom_regex.as_ref())
                        >= SOFT_MIN
                    {
                        best_hit = Some(res.clone());
                        break 'outer;
                    }
                }
            }

            let Some(hit) = best_hit else {
                return Ok(Vec::new());
            };
            let url = hit.get("url").and_then(Value::as_str).unwrap_or("");
            if url.is_empty() {
                return Ok(Vec::new());
            }
            let cand_title = hit.get("title").and_then(Value::as_str).unwrap_or("");
            let cand_artist = primary_artist_name(&hit);
            let song_id = hit.get("id").and_then(Value::as_i64).unwrap_or(0);

            let (html_opt, rec_opt) = if song_id > 0 {
                let rec_url = format!("{SONG_RECORD_URL}{song_id}");
                tokio::join!(genius_get(ctx, url), genius_get(ctx, &rec_url))
            } else {
                (genius_get(ctx, url).await, None)
            };

            let Some(html) = html_opt else {
                return Ok(Vec::new());
            };
            let Some(text) = extract_lyrics_text_from_html(&html) else {
                return Ok(Vec::new());
            };
            let lines = parse_plain(&text);
            if lines.is_empty() {
                return Ok(Vec::new());
            }

            let mut lyrics = Lyrics::new(lines, "Genius", false);
            lyrics.raw_format = Some(text);
            lyrics.format_type = Some("txt".to_string());
            lyrics.source_url = Some(url.to_string());

            if let Some(rec_body) = rec_opt
                && let Ok(rec_root) = serde_json::from_str::<Value>(&rec_body)
                && let Some(song_rec) = rec_root.get("response").and_then(|r| r.get("song"))
            {
                lyrics.credits = extract_credits(song_rec);
            }

            Ok(FetchedCandidate::with_metadata(
                lyrics,
                cand_title,
                cand_artist,
                0,
                None,
                track,
                ctx,
            )
            .into_iter()
            .collect())
        })
    }
}
