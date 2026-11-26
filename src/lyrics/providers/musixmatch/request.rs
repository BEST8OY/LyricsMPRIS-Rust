//! HTTP request execution and response handling for Musixmatch API.

use std::sync::LazyLock;

use super::extract::{extract_track_ids_from_macro, get_root_hint, get_root_status_code};
use super::token::DEFAULT_USER_AGENT;
use crate::lyrics::types::{LyricLine, LyricsError, TrackMatchInfo};
use crate::text_utils::{DURATION_TOLERANCE_SECS, clean_title_annotations, normalize};
use reqwest::Client;
use serde_json::Value;

/// Outcome of a single token's fetch attempt.
#[derive(Debug, PartialEq)]
pub(crate) enum FetchOutcome {
    /// Successfully found lyrics: (parsed lines, raw response content, track identifiers)
    Success(Vec<LyricLine>, Option<String>, TrackMatchInfo),
    /// Track was not found on Musixmatch (200 status but empty list/no lyrics).
    TrackNotFound,
    /// A token-specific issue (401 unauthorized, 402 payment, 403 forbidden, 429 rate limit).
    TokenError { reason: String, is_renewable: bool },
}

/// Returns true if the status code indicates a token/auth/quota/rate-limit error.
pub(crate) fn is_token_error_code(code: i64) -> bool {
    matches!(code, 401 | 402 | 403 | 429)
}

/// Returns true if the HTTP response status indicates a token/auth/quota/rate-limit error.
pub(crate) fn is_token_error_status(status: reqwest::StatusCode) -> bool {
    let code = status.as_u16();
    matches!(code, 401 | 402 | 403 | 429)
}

/// Evaluates HTTP status code and optional body JSON to determine if a token/rate-limit error occurred.
/// Differentiates between renewable token expiration (`401` with `hint: "renew"`) and throttling.
pub(crate) fn evaluate_token_outcome(
    http_status: reqwest::StatusCode,
    json: Option<&Value>,
) -> Option<FetchOutcome> {
    if let Some(json) = json {
        let code = get_root_status_code(json);
        let hint = get_root_hint(json);
        if let Some(code) = code
            && is_token_error_code(code)
        {
            let is_renewable = code == 401 && hint.as_deref() == Some("renew");
            let reason = match hint {
                Some(h) => format!("API Status {code} (hint: {h})"),
                None => format!("API Status {code}"),
            };
            return Some(FetchOutcome::TokenError {
                reason,
                is_renewable,
            });
        }
    }

    if is_token_error_status(http_status) {
        return Some(FetchOutcome::TokenError {
            reason: format!("HTTP {http_status}"),
            is_renewable: false,
        });
    }

    None
}

/// Check if a macro response has a successful status code (200).
pub(crate) fn is_success(macro_calls: &Value, endpoint: &str) -> bool {
    macro_calls
        .get(endpoint)
        .and_then(|v| v.pointer("/message/header/status_code"))
        .and_then(|v| v.as_i64())
        .map(|code| code == 200)
        .unwrap_or(false)
}

/// Try to call macro.subtitles.get and extract richsync or subtitle_body.
pub(crate) async fn try_macro_for_lyrics(
    client: &Client,
    params: &[(String, String)],
) -> Result<FetchOutcome, LyricsError> {
    let macro_base = "https://apic-appmobile.musixmatch.com/ws/1.1/macro.subtitles.get?format=json&namespace=lyrics_richsynched&subtitle_format=mxm&optional_calls=track.richsync&app_id=mac-ios-v2.0&richsync_compact_type=words&";
    let macro_url = macro_base.to_string()
        + &params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

    let macro_resp = client
        .get(&macro_url)
        .header("User-Agent", DEFAULT_USER_AGENT)
        .header("X-User-Agent", DEFAULT_USER_AGENT)
        .header("Cookie", "x-mxm-token-guid=")
        .header("X-Cookie", "x-mxm-token-guid=")
        .header("x-mxm-app-version", "10.1.1")
        .header("Accept", "application/json")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await?;

    let macro_status = macro_resp.status();

    // Server-side failures are transient network-level issues, not "track not found"
    if macro_status.is_server_error() {
        return Err(LyricsError::Http(macro_status.as_u16()));
    }

    // Try reading JSON body even on HTTP error (e.g. 401/403/429) to inspect status and hint
    let macro_json: Option<Value> = macro_resp.json().await.ok();

    if let Some(outcome) = evaluate_token_outcome(macro_status, macro_json.as_ref()) {
        return Ok(outcome);
    }

    let Some(macro_json) = macro_json else {
        return Ok(FetchOutcome::TrackNotFound);
    };

    if !macro_status.is_success() {
        return Ok(FetchOutcome::TrackNotFound);
    }

    let macro_calls = macro_json.pointer("/message/body/macro_calls");

    if let Some(calls) = macro_calls {
        let track_ids = extract_track_ids_from_macro(calls);

        // Check instrumental status first
        let is_instrumental = calls
            .pointer("/matcher.track.get/message/body/track/instrumental")
            .map(|v| v.as_i64() == Some(1) || v.as_bool() == Some(true))
            .unwrap_or(false);

        if is_instrumental {
            let line = LyricLine::new(0.0, "♪ Instrumental ♪");
            return Ok(FetchOutcome::Success(vec![line], None, track_ids));
        }

        // Prefer richsync (word-level timing) if available
        if is_success(calls, "track.richsync.get")
            && let Some(richsync_body) = calls
                .pointer("/track.richsync.get/message/body/richsync/richsync_body")
                .and_then(|v| v.as_str())
            && let Some(parsed) = crate::lyrics::parse::parse_richsync_body(richsync_body)
        {
            return Ok(FetchOutcome::Success(
                parsed,
                Some(richsync_body.to_string()),
                track_ids,
            ));
        }

        // Fall back to subtitles (line-level timing)
        if is_success(calls, "track.subtitles.get")
            && let Some(subtitle_body) = calls
                .pointer("/track.subtitles.get/message/body/subtitle_list/0/subtitle/subtitle_body")
                .and_then(|v| v.as_str())
            && let Some(parsed) = crate::lyrics::parse::parse_subtitle_body(subtitle_body)
        {
            return Ok(FetchOutcome::Success(
                parsed,
                Some(subtitle_body.to_string()),
                track_ids,
            ));
        }
    }

    Ok(FetchOutcome::TrackNotFound)
}

/// Fetch lyrics with a single token.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fetch_lyrics_with_token(
    client: &Client,
    token: &str,
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    track_spotify_id: Option<&str>,
    track_isrc: Option<&str>,
    track_itunes_id: Option<&str>,
) -> Result<FetchOutcome, LyricsError> {
    // Build unified single-shot query combining track identifiers and track metadata.
    // Musixmatch resolves matcher.track.get, track.richsync.get, and track.subtitles.get
    // in a single macro call, matching by ID if indexed and falling back seamlessly within
    // the same round-trip.
    let mut params = vec![
        ("q_artist".to_string(), artist.to_string()),
        ("q_artists".to_string(), artist.to_string()),
        ("q_track".to_string(), title.to_string()),
        ("usertoken".to_string(), token.to_string()),
    ];
    if !album.is_empty() {
        params.push(("q_album".to_string(), album.to_string()));
    }
    if let Some(d) = duration {
        let secs = (d.round() as i64).to_string();
        params.push(("q_duration".to_string(), secs));
    }

    let has_ids = track_spotify_id.is_some() || track_isrc.is_some() || track_itunes_id.is_some();
    if let Some(sid) = track_spotify_id {
        params.push(("track_spotify_id".to_string(), sid.to_string()));
    }
    if let Some(isrc) = track_isrc {
        params.push(("track_isrc".to_string(), isrc.to_string()));
    }
    if let Some(itunes) = track_itunes_id {
        params.push(("track_itunes_id".to_string(), itunes.to_string()));
    }

    let outcome = try_macro_for_lyrics(client, &params).await?;

    // If an ID was supplied and the query resulted in TrackNotFound,
    // execute a resilient fallback with pure metadata search (omitting IDs)
    // in case the ID is unmapped or stale in Musixmatch's catalog.
    let outcome = if has_ids && matches!(outcome, FetchOutcome::TrackNotFound) {
        let mut fallback_params = vec![
            ("q_artist".to_string(), artist.to_string()),
            ("q_artists".to_string(), artist.to_string()),
            ("q_track".to_string(), title.to_string()),
            ("usertoken".to_string(), token.to_string()),
        ];
        if !album.is_empty() {
            fallback_params.push(("q_album".to_string(), album.to_string()));
        }
        if let Some(d) = duration {
            let secs = (d.round() as i64).to_string();
            fallback_params.push(("q_duration".to_string(), secs));
        }

        try_macro_for_lyrics(client, &fallback_params).await?
    } else {
        outcome
    };

    if !matches!(outcome, FetchOutcome::TrackNotFound) {
        return Ok(outcome);
    }

    // Step A: If title has annotations (e.g. "(feat. ...)", "[Remaster]"), try macro with clean title
    if let Some(clean) = clean_title_annotations(title) {
        let mut clean_params = vec![
            ("q_artist".to_string(), artist.to_string()),
            ("q_artists".to_string(), artist.to_string()),
            ("q_track".to_string(), clean),
            ("usertoken".to_string(), token.to_string()),
        ];
        if !album.is_empty() {
            clean_params.push(("q_album".to_string(), album.to_string()));
        }
        if let Some(d) = duration {
            let secs = (d.round() as i64).to_string();
            clean_params.push(("q_duration".to_string(), secs));
        }
        let clean_outcome = try_macro_for_lyrics(client, &clean_params).await?;
        if !matches!(clean_outcome, FetchOutcome::TrackNotFound) {
            return Ok(clean_outcome);
        }
    }

    // Step B: Fallback to track.search to find candidates with duration matching
    // (recovering when matcher.track.get misidentifies tracks or selects unsynced singles)
    if let Some(candidate_outcome) =
        search_and_fetch_best_candidate(client, token, artist, title, album, duration).await?
    {
        return Ok(candidate_outcome);
    }

    Ok(FetchOutcome::TrackNotFound)
}

static VERSION_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)(?:\s*[-–—/:]\s*|\s*[\(\[])(?:[^\(\)\[\]]*\b)?(live|en vivo|dal vivo|acoustic|ac[uú]stico|acustico|unplugged|stripped|remix|re-mix|dub mix|club mix|extended mix|dance mix|vip mix|demo|early version|commentary|interview|track by track|instrumental|karaoke|backing track)\b[^\)\]]*[\)\]]?"#,
    )
    .unwrap()
});

fn extract_version_and_base(title: &str) -> (Vec<&'static str>, String) {
    let mut versions = Vec::new();
    for cap in VERSION_RE.captures_iter(title) {
        if let Some(m) = cap.get(1) {
            let lower = m.as_str().to_lowercase();
            let v = if lower.contains("live") || lower.contains("vivo") {
                "live"
            } else if lower.contains("acoust")
                || lower.contains("acúst")
                || lower.contains("acust")
                || lower.contains("unplugged")
                || lower.contains("stripped")
            {
                "acoustic"
            } else if lower.contains("mix") {
                "remix"
            } else if lower.contains("demo") || lower.contains("early version") {
                "demo"
            } else if lower.contains("commentary")
                || lower.contains("interview")
                || lower.contains("track by track")
            {
                "commentary"
            } else if lower.contains("instrumental")
                || lower.contains("karaoke")
                || lower.contains("backing track")
            {
                "instrumental"
            } else {
                "other"
            };
            if !versions.contains(&v) {
                versions.push(v);
            }
        }
    }
    let base = VERSION_RE.replace_all(title, "").trim().to_string();
    (versions, base)
}

fn strip_punctuation(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn is_title_compatible(query_title: &str, candidate_title: &str) -> bool {
    let (query_versions, query_base) = extract_version_and_base(query_title);
    let (cand_versions, cand_base) = extract_version_and_base(candidate_title);

    // 1. Candidate must not introduce any version tag (e.g. "live", "acoustic")
    // that was not requested in the query title.
    for v in &cand_versions {
        if !query_versions.contains(v) {
            return false;
        }
    }

    // 2. Compare clean base titles (stripping remasters, feats, and normalizing)
    let norm_query = normalize(&query_base);
    let clean_query = clean_title_annotations(&norm_query).unwrap_or(norm_query);

    let norm_cand = normalize(&cand_base);
    let clean_cand = clean_title_annotations(&norm_cand).unwrap_or(norm_cand);

    if clean_query == clean_cand {
        return true;
    }

    // 3. Punctuation-insensitive fallback (e.g., "Don't" vs "Dont")
    let punct_query = strip_punctuation(&clean_query);
    let punct_cand = strip_punctuation(&clean_cand);
    if !punct_query.is_empty() && punct_query == punct_cand {
        return true;
    }

    false
}

/// Helper: selects the best candidate track ID from a track.search result list.
///
/// Filters out tracks without synchronized/plain lyrics or instrumentals, filters by duration tolerance (±2.0s),
/// checks title compatibility and version tag parity (avoiding live/acoustic/remix pollution for studio tracks),
/// and scores candidates by album match, exact title match, richsync presence, duration closeness, and rating.
pub(crate) fn select_best_candidate_track_id(
    track_list: &[Value],
    title: &str,
    album: &str,
    duration: Option<f64>,
) -> Option<i64> {
    let mut best_id = None;
    let mut highest_score = f64::NEG_INFINITY;

    let norm_title = normalize(title);

    for item in track_list {
        let Some(track) = item.get("track") else {
            continue;
        };
        let Some(track_id) = track.get("track_id").and_then(|v| v.as_i64()) else {
            continue;
        };
        let track_name = track
            .get("track_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let track_length = track
            .get("track_length")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let has_richsync = track
            .get("has_richsync")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let has_subtitles = track
            .get("has_subtitles")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let has_lyrics = track
            .get("has_lyrics")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let instrumental = track
            .get("instrumental")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let album_name = track
            .get("album_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Candidate must have some lyric or instrumental payload
        if has_richsync == 0 && has_subtitles == 0 && has_lyrics == 0 && instrumental == 0 {
            continue;
        }

        // Candidate title must be compatible with the requested track title
        if !is_title_compatible(title, track_name) {
            continue;
        }

        // Check duration tolerance if duration is known
        if let Some(target_dur) = duration
            && target_dur > 0.0
        {
            if track_length > 0.0 {
                let diff = (track_length - target_dur).abs();
                if diff > DURATION_TOLERANCE_SECS {
                    continue;
                }
            } else {
                // If caller provided an authoritative target duration, a candidate
                // with unknown length (0.0) cannot satisfy the duration tolerance window.
                continue;
            }
        }

        let mut score = 0.0;
        if has_richsync == 1 {
            score += 50.0;
        } else if has_subtitles == 1 {
            score += 25.0;
        }

        let norm_cand_title = normalize(track_name);
        if norm_cand_title == norm_title {
            score += 30.0;
        }

        if !album.is_empty() && !album_name.is_empty() {
            let norm_album = normalize(album);
            let norm_cand = normalize(album_name);
            if norm_cand == norm_album {
                score += 100.0;
            } else if norm_cand.contains(&norm_album) || norm_album.contains(&norm_cand) {
                score += 40.0;
            }
        }

        if let Some(target_dur) = duration
            && target_dur > 0.0
            && track_length > 0.0
        {
            let diff = (track_length - target_dur).abs();
            score += (DURATION_TOLERANCE_SECS - diff).max(0.0) * 10.0;
        }

        let rating = track
            .get("track_rating")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        score += rating.min(20.0);

        if score > highest_score {
            highest_score = score;
            best_id = Some(track_id);
        }
    }

    best_id
}

/// Fallback helper: calls track.search to find candidates with duration matching and retrieves lyrics for the best candidate.
pub(crate) async fn search_and_fetch_best_candidate(
    client: &Client,
    token: &str,
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
) -> Result<Option<FetchOutcome>, LyricsError> {
    let mut queries = vec![title.to_string()];
    if let Some(clean) = clean_title_annotations(title)
        && clean != title
    {
        queries.push(clean);
    }

    for query_title in queries {
        let search_url = format!(
            "https://apic-appmobile.musixmatch.com/ws/1.1/track.search?format=json&app_id=mac-ios-v2.0&page_size=15&q_artist={}&q_track={}&usertoken={}",
            urlencoding::encode(artist),
            urlencoding::encode(&query_title),
            urlencoding::encode(token),
        );

        let resp = client
            .get(&search_url)
            .header("User-Agent", DEFAULT_USER_AGENT)
            .header("X-User-Agent", DEFAULT_USER_AGENT)
            .header("Cookie", "x-mxm-token-guid=")
            .header("X-Cookie", "x-mxm-token-guid=")
            .header("x-mxm-app-version", "10.1.1")
            .header("Accept", "application/json")
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await?;

        let status = resp.status();
        if status.is_server_error() {
            return Err(LyricsError::Http(status.as_u16()));
        }

        let json: Option<Value> = resp.json().await.ok();
        if let Some(outcome) = evaluate_token_outcome(status, json.as_ref()) {
            return Ok(Some(outcome));
        }

        let Some(json) = json else { continue };
        let track_list = json
            .pointer("/message/body/track_list")
            .and_then(|v| v.as_array());

        let Some(track_list) = track_list else {
            continue;
        };
        if let Some(best_track_id) =
            select_best_candidate_track_id(track_list, title, album, duration)
        {
            let id_params = vec![
                ("track_id".to_string(), best_track_id.to_string()),
                ("usertoken".to_string(), token.to_string()),
            ];
            let outcome = try_macro_for_lyrics(client, &id_params).await?;
            if !matches!(outcome, FetchOutcome::TrackNotFound) {
                return Ok(Some(outcome));
            }
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluate_token_outcome_renewable() {
        let json: Value = serde_json::from_str(
            r#"{
            "message": {
                "header": {
                    "status_code": 401,
                    "hint": "renew"
                }
            }
        }"#,
        )
        .unwrap();

        let outcome = evaluate_token_outcome(reqwest::StatusCode::OK, Some(&json));
        assert_eq!(
            outcome,
            Some(FetchOutcome::TokenError {
                reason: "API Status 401 (hint: renew)".to_string(),
                is_renewable: true,
            })
        );
    }

    #[test]
    fn test_evaluate_token_outcome_throttled() {
        let json: Value = serde_json::from_str(
            r#"{
            "message": {
                "header": {
                    "status_code": 429,
                    "hint": "too_many_requests"
                }
            }
        }"#,
        )
        .unwrap();

        let outcome = evaluate_token_outcome(reqwest::StatusCode::OK, Some(&json));
        assert_eq!(
            outcome,
            Some(FetchOutcome::TokenError {
                reason: "API Status 429 (hint: too_many_requests)".to_string(),
                is_renewable: false,
            })
        );
    }

    #[test]
    fn test_evaluate_token_outcome_captcha_throttled() {
        let json: Value = serde_json::from_str(
            r#"{
            "message": {
                "header": {
                    "status_code": 401,
                    "hint": "captcha"
                }
            }
        }"#,
        )
        .unwrap();

        let outcome = evaluate_token_outcome(reqwest::StatusCode::OK, Some(&json));
        assert_eq!(
            outcome,
            Some(FetchOutcome::TokenError {
                reason: "API Status 401 (hint: captcha)".to_string(),
                is_renewable: false,
            })
        );
    }

    #[test]
    fn test_evaluate_token_outcome_http_fallback() {
        let outcome = evaluate_token_outcome(reqwest::StatusCode::FORBIDDEN, None);
        assert_eq!(
            outcome,
            Some(FetchOutcome::TokenError {
                reason: "HTTP 403 Forbidden".to_string(),
                is_renewable: false,
            })
        );
    }

    #[test]
    fn test_select_best_candidate_track_id() {
        let json_str = r#"[
            {
                "track": {
                    "track_id": 101,
                    "track_name": "Beauty And A Beat",
                    "album_name": "Beauty And A Beat",
                    "track_length": 294,
                    "has_richsync": 0,
                    "has_subtitles": 0,
                    "has_lyrics": 0,
                    "instrumental": 0
                }
            },
            {
                "track": {
                    "track_id": 102,
                    "track_name": "Beauty And A Beat",
                    "album_name": "Simplemente Acústico",
                    "track_length": 145,
                    "has_richsync": 1,
                    "has_subtitles": 1,
                    "has_lyrics": 1,
                    "instrumental": 0
                }
            },
            {
                "track": {
                    "track_id": 103,
                    "track_name": "Beauty And A Beat",
                    "album_name": "Believe",
                    "track_length": 228,
                    "has_richsync": 1,
                    "has_subtitles": 1,
                    "has_lyrics": 1,
                    "instrumental": 0,
                    "track_rating": 80
                }
            },
            {
                "track": {
                    "track_id": 104,
                    "track_name": "Beauty And A Beat",
                    "album_name": "Unknown",
                    "track_length": 227.5,
                    "has_richsync": 0,
                    "has_subtitles": 1,
                    "has_lyrics": 1,
                    "instrumental": 0,
                    "track_rating": 10
                }
            }
        ]"#;

        let track_list: Vec<Value> = serde_json::from_str(json_str).unwrap();

        // Should select track 103: duration matches 228.0 ± 2.0s, album matches "Believe", and has richsync
        let best_id = select_best_candidate_track_id(
            &track_list,
            "Beauty And A Beat",
            "Believe",
            Some(228.0),
        );
        assert_eq!(best_id, Some(103));

        // When duration is 145.0, should select track 102
        let best_145 = select_best_candidate_track_id(
            &track_list,
            "Beauty And A Beat",
            "Simplemente Acústico",
            Some(145.0),
        );
        assert_eq!(best_145, Some(102));

        // When duration is 300.0 (no candidate within 2.0s), should return None
        let none_id = select_best_candidate_track_id(
            &track_list,
            "Beauty And A Beat",
            "Believe",
            Some(300.0),
        );
        assert_eq!(none_id, None);
    }

    #[test]
    fn test_is_title_compatible() {
        assert!(is_title_compatible("Red Flag", "Red Flag"));
        assert!(is_title_compatible("Red Flag", "Red Flag [2009 Remaster]"));
        assert!(is_title_compatible("Heroes (feat. Brian Eno)", "Heroes"));
        assert!(is_title_compatible(
            "Hotel California (Live)",
            "Hotel California - Live On MTV 1994"
        ));

        // Version tags present in candidate but NOT in query title must be rejected
        assert!(!is_title_compatible(
            "Red Flag",
            "Red Flag (Live at Nova Rock Festival)"
        ));
        assert!(!is_title_compatible(
            "Red Flag",
            "Red Flag - Live At MTV Campus Invasion Germany"
        ));
        assert!(!is_title_compatible("Red Flag", "Red Flag (Live)"));
        assert!(!is_title_compatible("Creep", "Creep (Acoustic Version)"));
        assert!(!is_title_compatible("Song", "Song (Remix)"));
        assert!(!is_title_compatible("Song", "Song (Demo)"));

        // Different songs entirely
        assert!(!is_title_compatible("Red Flag", "The Red Flag"));
        assert!(!is_title_compatible("Red Flag", "Billy Talent on Red Flag"));
    }

    #[test]
    fn test_select_best_candidate_rejects_live_and_zero_duration() {
        let json_str = r#"[
            {
                "track": {
                    "track_id": 1,
                    "track_name": "Red Flag",
                    "album_name": "Red Flag",
                    "track_length": 200,
                    "has_richsync": 0,
                    "has_subtitles": 0,
                    "has_lyrics": 0,
                    "instrumental": 0
                }
            },
            {
                "track": {
                    "track_id": 2,
                    "track_name": "Red Flag (Live at Nova Rock Festival)",
                    "album_name": "666 Live",
                    "track_length": 0,
                    "has_richsync": 1,
                    "has_subtitles": 1,
                    "has_lyrics": 1,
                    "instrumental": 0
                }
            },
            {
                "track": {
                    "track_id": 3,
                    "track_name": "Red Flag - Live at the Horseshoe Tavern",
                    "album_name": "Billy Talent II",
                    "track_length": 222,
                    "has_richsync": 1,
                    "has_subtitles": 1,
                    "has_lyrics": 1,
                    "instrumental": 0
                }
            },
            {
                "track": {
                    "track_id": 4,
                    "track_name": "The Red Flag",
                    "album_name": "The Internationale",
                    "track_length": 198,
                    "has_richsync": 1,
                    "has_subtitles": 1,
                    "has_lyrics": 1,
                    "instrumental": 0
                }
            }
        ]"#;

        let track_list: Vec<Value> = serde_json::from_str(json_str).unwrap();

        // For studio "Red Flag" (length 198.173s), none of the candidates should be accepted:
        // Track 1 has no lyrics
        // Track 2 is Live and has length 0
        // Track 3 is Live and length is 222s (out of tolerance)
        // Track 4 is a different song ("The Red Flag")
        let result = select_best_candidate_track_id(
            &track_list,
            "Red Flag",
            "Billy Talent II",
            Some(198.173),
        );
        assert_eq!(result, None);
    }
}
