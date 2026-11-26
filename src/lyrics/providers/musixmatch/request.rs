//! HTTP request execution and response handling for Musixmatch API.

use std::sync::LazyLock;

use super::extract::{
    MatchedTrackMeta, extract_track_ids_from_macro, extract_track_meta_from_json,
    extract_track_meta_from_macro, get_root_hint, get_root_status_code,
};
use super::token::{API_BASE, APP_ID, open_api_get, request_id, throttle_request};
use crate::lyrics::providers::MAX_CANDIDATES;
use crate::lyrics::types::{LyricLine, LyricsError, TrackInfo, TrackMatchInfo};
use crate::text_utils::{DURATION_TOLERANCE_SECS, clean_title_annotations, normalize};
use reqwest::Client;
use serde_json::Value;

/// Outcome of a single token's fetch attempt.
#[derive(Debug, PartialEq)]
pub(crate) enum FetchOutcome {
    /// Successfully found lyrics: (parsed lines, raw response content, track identifiers, track metadata)
    Success(
        Vec<LyricLine>,
        Option<String>,
        TrackMatchInfo,
        Option<MatchedTrackMeta>,
    ),
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

/// Try to call `macro.subtitles.get` at `api_base` and extract richsync or subtitle_body.
pub(crate) async fn try_macro_for_lyrics(
    client: &Client,
    api_base: &str,
    params: &[(String, String)],
) -> Result<FetchOutcome, LyricsError> {
    let req_id = request_id();
    let macro_base = format!(
        "{api_base}macro.subtitles.get?namespace=lyrics_richsynched&optional_calls=track.richsync&subtitle_format=lrc&f_subtitle_length_max_deviation=40&format=json&app_id={APP_ID}&t={req_id}&"
    );
    let macro_url = macro_base
        + &params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

    throttle_request().await;
    let macro_resp = open_api_get(client, &macro_url).send().await?;

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
        let track_meta = extract_track_meta_from_macro(calls);

        // Check instrumental status first
        let is_instrumental = calls
            .pointer("/matcher.track.get/message/body/track/instrumental")
            .map(|v| v.as_i64() == Some(1) || v.as_bool() == Some(true))
            .unwrap_or(false);

        if is_instrumental {
            let line = LyricLine::new(0.0, "♪ Instrumental ♪");
            return Ok(FetchOutcome::Success(
                vec![line],
                None,
                track_ids,
                track_meta,
            ));
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
                track_meta,
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
                track_meta,
            ));
        }
    }

    Ok(FetchOutcome::TrackNotFound)
}

fn build_macro_params(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    token: &str,
) -> Vec<(String, String)> {
    let mut params = vec![
        ("q_artist".to_string(), artist.to_string()),
        ("q_artists".to_string(), artist.to_string()),
        ("q_track".to_string(), title.to_string()),
        ("usertoken".to_string(), token.to_string()),
    ];
    if !album.is_empty() {
        params.push(("q_album".to_string(), album.to_string()));
    }
    if let Some(d) = duration
        && d > 0.0
    {
        params.push(("q_duration".to_string(), (d.round() as i64).to_string()));
    }
    params
}

/// Validates direct-macro `Success` metadata against `track` so mismatched guesses fall
/// through to `track.search` (`TokenError` and `TrackNotFound` pass through unchanged).
fn filter_outcome_for_track(
    outcome: FetchOutcome,
    track: &TrackInfo,
    custom_regex: Option<&regex::Regex>,
) -> FetchOutcome {
    let FetchOutcome::Success(ref lines, _, _, ref meta) = outcome else {
        return outcome;
    };
    if lines.is_empty() || super::metadata_match_score(meta.as_ref(), track, custom_regex).is_none()
    {
        return FetchOutcome::TrackNotFound;
    }
    outcome
}

/// Fetch lyrics with a single token using the configured Musixmatch `API_BASE`.
pub(crate) async fn fetch_lyrics_with_token(
    client: &Client,
    token: &str,
    track: &TrackInfo,
    ids: &TrackMatchInfo,
    custom_regex: Option<&regex::Regex>,
) -> Result<FetchOutcome, LyricsError> {
    fetch_lyrics_with_token_at(client, API_BASE, token, track, ids, custom_regex).await
}

pub(crate) async fn fetch_lyrics_with_token_at(
    client: &Client,
    api_base: &str,
    token: &str,
    track: &TrackInfo,
    ids: &TrackMatchInfo,
    custom_regex: Option<&regex::Regex>,
) -> Result<FetchOutcome, LyricsError> {
    let duration = (track.duration_secs > 0).then_some(track.duration_secs as f64);
    let track_spotify_id = ids.track_spotify_ids.first().map(String::as_str);
    let track_isrc = ids.track_isrcs.first().map(String::as_str);

    // Step 1: Single-shot macro query combining metadata and any known external IDs.
    let mut params = build_macro_params(&track.artist, &track.title, &track.album, duration, token);
    let has_ids = track_spotify_id.is_some() || track_isrc.is_some();
    if let Some(sid) = track_spotify_id {
        params.push(("track_spotify_id".to_string(), sid.to_string()));
    }
    if let Some(isrc) = track_isrc {
        params.push(("track_isrc".to_string(), isrc.to_string()));
    }

    let outcome = filter_outcome_for_track(
        try_macro_for_lyrics(client, api_base, &params).await?,
        track,
        custom_regex,
    );

    // Step 2: Retry pure metadata (without IDs) if IDs were sent and Call 1 missed/mismatched.
    let outcome = if has_ids && matches!(outcome, FetchOutcome::TrackNotFound) {
        let fallback_params =
            build_macro_params(&track.artist, &track.title, &track.album, duration, token);
        filter_outcome_for_track(
            try_macro_for_lyrics(client, api_base, &fallback_params).await?,
            track,
            custom_regex,
        )
    } else {
        outcome
    };

    if !matches!(outcome, FetchOutcome::TrackNotFound) {
        return Ok(outcome);
    }

    // Fallback to track.search to find candidates with duration matching and scoring
    if let Some(candidate_outcome) =
        search_and_fetch_best_candidate(client, api_base, token, track, duration, custom_regex)
            .await?
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

/// Selects ranked `(track_id, meta)` candidates from a `track.search` result list.
pub(crate) fn ranked_candidate_tracks(
    track_list: &[Value],
    title: &str,
    album: &str,
    duration: Option<f64>,
) -> Vec<(i64, MatchedTrackMeta)> {
    let mut ranked: Vec<(f64, i64, MatchedTrackMeta)> = Vec::new();
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

        ranked.push((score, track_id, extract_track_meta_from_json(track)));
    }

    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    ranked.into_iter().map(|(_, id, meta)| (id, meta)).collect()
}

/// Helper: selects the best candidate track ID from a track.search result list.
///
/// Filters out tracks without synchronized/plain lyrics or instrumentals, filters by duration tolerance (±2.0s),
/// checks title compatibility and version tag parity (avoiding live/acoustic/remix pollution for studio tracks),
/// and scores candidates by album match, exact title match, richsync presence, duration closeness, and rating.
#[cfg(test)]
pub(crate) fn select_best_candidate_track_id(
    track_list: &[Value],
    title: &str,
    album: &str,
    duration: Option<f64>,
) -> Option<i64> {
    ranked_candidate_tracks(track_list, title, album, duration)
        .into_iter()
        .next()
        .map(|(id, _)| id)
}

/// Fallback helper: calls track.search to find candidates with duration matching and retrieves lyrics for the best candidate.
pub(crate) async fn search_and_fetch_best_candidate(
    client: &Client,
    api_base: &str,
    token: &str,
    track: &TrackInfo,
    duration: Option<f64>,
    custom_regex: Option<&regex::Regex>,
) -> Result<Option<FetchOutcome>, LyricsError> {
    let req_id = request_id();
    let mut search_url = format!(
        "{api_base}track.search?page_size=10&page=1&s_track_rating=desc&q_artist={}&q_track={}&usertoken={}&format=json&app_id={APP_ID}&t={req_id}",
        urlencoding::encode(&track.artist),
        urlencoding::encode(&track.title),
        urlencoding::encode(token),
    );
    if let Some(d) = duration
        && d > 0.0
    {
        search_url.push_str(&format!("&q_duration={}", d.round() as i64));
    }

    throttle_request().await;
    let resp = open_api_get(client, &search_url).send().await?;

    let status = resp.status();
    if status.is_server_error() {
        return Err(LyricsError::Http(status.as_u16()));
    }

    let json: Option<Value> = resp.json().await.ok();
    if let Some(outcome) = evaluate_token_outcome(status, json.as_ref()) {
        return Ok(Some(outcome));
    }

    let Some(track_list) = json
        .as_ref()
        .and_then(|j| j.pointer("/message/body/track_list"))
        .and_then(|v| v.as_array())
    else {
        return Ok(None);
    };

    for (best_track_id, cand_meta) in
        ranked_candidate_tracks(track_list, &track.title, &track.album, duration)
            .into_iter()
            .take(MAX_CANDIDATES)
    {
        let id_params = vec![
            ("track_id".to_string(), best_track_id.to_string()),
            ("usertoken".to_string(), token.to_string()),
        ];
        let outcome = try_macro_for_lyrics(client, api_base, &id_params).await?;
        match outcome {
            FetchOutcome::Success(lines, raw, ids, macro_meta) => {
                let meta = macro_meta.or_else(|| {
                    (!cand_meta.title.is_empty() || !cand_meta.artist.is_empty())
                        .then_some(cand_meta)
                });
                let candidate = FetchOutcome::Success(lines, raw, ids, meta);
                let checked = filter_outcome_for_track(candidate, track, custom_regex);
                if !matches!(checked, FetchOutcome::TrackNotFound) {
                    return Ok(Some(checked));
                }
            }
            FetchOutcome::TokenError { .. } => return Ok(Some(outcome)),
            FetchOutcome::TrackNotFound => continue,
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

    #[tokio::test]
    async fn test_direct_macro_rejection_falls_through_to_track_search() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first_line = req.lines().next().unwrap_or("");

                    let body = if first_line.contains("macro.subtitles.get")
                        && first_line.contains("track_id=202")
                    {
                        serde_json::json!({
                            "message": {
                                "header": { "status_code": 200 },
                                "body": {
                                    "macro_calls": {
                                        "matcher.track.get": {
                                            "message": {
                                                "header": { "status_code": 200 },
                                                "body": {
                                                    "track": {
                                                        "track_name": "Red Flag",
                                                        "artist_name": "Billy Talent",
                                                        "album_name": "Billy Talent II",
                                                        "track_length": 198
                                                    }
                                                }
                                            }
                                        },
                                        "track.subtitles.get": {
                                            "message": {
                                                "header": { "status_code": 200 },
                                                "body": {
                                                    "subtitle_list": [{
                                                        "subtitle": {
                                                            "subtitle_body": "[00:05.00] Studio lyric line"
                                                        }
                                                    }]
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        })
                    } else if first_line.contains("macro.subtitles.get") {
                        // Direct macro returns wrong artist/version; must be rejected cleanly.
                        serde_json::json!({
                            "message": {
                                "header": { "status_code": 200 },
                                "body": {
                                    "macro_calls": {
                                        "matcher.track.get": {
                                            "message": {
                                                "header": { "status_code": 200 },
                                                "body": {
                                                    "track": {
                                                        "track_name": "Red Flag (Live)",
                                                        "artist_name": "Wrong Cover Band",
                                                        "album_name": "Live",
                                                        "track_length": 250
                                                    }
                                                }
                                            }
                                        },
                                        "track.subtitles.get": {
                                            "message": {
                                                "header": { "status_code": 200 },
                                                "body": {
                                                    "subtitle_list": [{
                                                        "subtitle": {
                                                            "subtitle_body": "[00:01.00] Wrong cover line"
                                                        }
                                                    }]
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        })
                    } else {
                        serde_json::json!({
                            "message": {
                                "header": { "status_code": 200 },
                                "body": {
                                    "track_list": [{
                                        "track": {
                                            "track_id": 202,
                                            "track_name": "Red Flag",
                                            "artist_name": "Billy Talent",
                                            "album_name": "Billy Talent II",
                                            "track_length": 198,
                                            "has_richsync": 0,
                                            "has_subtitles": 1,
                                            "has_lyrics": 1,
                                            "instrumental": 0
                                        }
                                    }]
                                }
                            }
                        })
                    };

                    let body_str = body.to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body_str.len(),
                        body_str
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });

        let api_base = format!("http://{addr}/ws/1.1/");
        let track = TrackInfo::new("Red Flag", "Billy Talent", "Billy Talent II", 198);
        let ids = TrackMatchInfo::default();
        let outcome = fetch_lyrics_with_token_at(
            crate::lyrics::types::http_client(),
            &api_base,
            "test_token",
            &track,
            &ids,
            None,
        )
        .await
        .unwrap();

        match outcome {
            FetchOutcome::Success(lines, _, _, meta) => {
                assert_eq!(lines[0].text, "Studio lyric line");
                assert_eq!(meta.unwrap().artist, "Billy Talent");
            }
            other => panic!("Expected fallback track.search Success, got {other:?}"),
        }

        server.abort();
    }
}
