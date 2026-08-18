//! JSON response extractors and identifier parsing for Musixmatch.

use crate::lyrics::types::TrackMatchInfo;
use serde_json::Value;

/// Extracts the root level status code from a Musixmatch response JSON.
pub(crate) fn get_root_status_code(json: &Value) -> Option<i64> {
    json.pointer("/message/header/status_code")
        .and_then(|v| v.as_i64())
}

pub(crate) fn extract_string_or_int(value: &Value) -> Option<String> {
    let s = value
        .as_str()
        .map(String::from)
        .or_else(|| value.as_i64().map(|i| i.to_string()))?;
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

pub(crate) fn extract_all_strings_from_fields(
    track: &Value,
    single_field: &str,
    array_field: &str,
) -> Vec<String> {
    let mut results = Vec::new();
    let mut add_item = |val: &Value| {
        if let Some(s) = extract_string_or_int(val) {
            let trimmed = s.trim().to_string();
            if !trimmed.is_empty()
                && !results
                    .iter()
                    .any(|existing: &String| existing.eq_ignore_ascii_case(&trimmed))
            {
                results.push(trimmed);
            }
        }
    };

    if let Some(val) = track.get(single_field) {
        add_item(val);
    }
    if let Some(arr) = track.get(array_field).and_then(|v| v.as_array()) {
        for item in arr {
            add_item(item);
        }
    }
    results
}

pub(crate) fn extract_all_isrcs_from_json(track: &Value) -> Vec<String> {
    let mut isrcs = Vec::new();

    let mut add_isrc = |val: &Value| {
        if let Some(s) = extract_string_or_int(val) {
            let trimmed = s.trim().to_string();
            if !trimmed.is_empty()
                && !isrcs
                    .iter()
                    .any(|existing: &String| existing.eq_ignore_ascii_case(&trimmed))
            {
                isrcs.push(trimmed);
            }
        }
    };

    if let Some(track_isrc) = track.get("track_isrc") {
        add_isrc(track_isrc);
    }

    if let Some(commontrack_isrcs) = track.get("commontrack_isrcs").and_then(|v| v.as_array()) {
        for item in commontrack_isrcs {
            if let Some(arr) = item.as_array() {
                for inner in arr {
                    add_isrc(inner);
                }
            } else {
                add_isrc(item);
            }
        }
    }

    isrcs
}

/// Extract track identifiers (isrcs, spotify_ids, itunes_ids) from a musixmatch track object.
pub(crate) fn extract_track_ids_from_json(track: &Value) -> TrackMatchInfo {
    let isrcs = extract_all_isrcs_from_json(track);
    let spotify_ids =
        extract_all_strings_from_fields(track, "track_spotify_id", "commontrack_spotify_ids");
    let itunes_ids =
        extract_all_strings_from_fields(track, "track_itunes_id", "commontrack_itunes_ids");

    TrackMatchInfo {
        track_isrcs: isrcs,
        track_spotify_ids: spotify_ids,
        track_itunes_ids: itunes_ids,
    }
}

/// Try to extract track identifiers from the macro response JSON.
/// Musixmatch macro responses may include `matcher.track.get` with full track metadata.
pub(crate) fn extract_track_ids_from_macro(macro_calls: &Value) -> TrackMatchInfo {
    if let Some(track) = macro_calls.pointer("/matcher.track.get/message/body/track") {
        return extract_track_ids_from_json(track);
    }
    if let Some(track) = macro_calls.pointer("/track") {
        return extract_track_ids_from_json(track);
    }
    TrackMatchInfo::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_track_ids_from_json() {
        let json: Value = serde_json::from_str(
            r#"{
            "track_isrc": "USUM72345678",
            "track_spotify_id": "6rqhFg4Kj6gIwR5v",
            "track_itunes_id": "123456789"
        }"#,
        )
        .unwrap();

        let ids = extract_track_ids_from_json(&json);
        assert_eq!(ids.track_isrcs, vec!["USUM72345678".to_string()]);
        assert_eq!(ids.track_spotify_ids, vec!["6rqhFg4Kj6gIwR5v".to_string()]);
        assert_eq!(ids.track_itunes_ids, vec!["123456789".to_string()]);
    }

    #[test]
    fn test_extract_track_ids_from_json_array_fields() {
        let json: Value = serde_json::from_str(
            r#"{
            "track_isrc": "EEUL32203751",
            "commontrack_isrcs": [["EEUL32203751", "GBKPL2177541"]],
            "track_spotify_id": "5FVd6KXrgO9B3JPmC8OPst",
            "commontrack_spotify_ids": ["5FVd6KXrgO9B3JPmC8OPst", "2UzMpPKPhbcC8RbsmuURAZ"],
            "commontrack_itunes_ids": [1442699400, 776001036]
        }"#,
        )
        .unwrap();

        let ids = extract_track_ids_from_json(&json);
        assert_eq!(
            ids.track_isrcs,
            vec!["EEUL32203751".to_string(), "GBKPL2177541".to_string()]
        );
        assert_eq!(
            ids.track_spotify_ids,
            vec![
                "5FVd6KXrgO9B3JPmC8OPst".to_string(),
                "2UzMpPKPhbcC8RbsmuURAZ".to_string()
            ]
        );
        assert_eq!(
            ids.track_itunes_ids,
            vec!["1442699400".to_string(), "776001036".to_string()]
        );
    }

    #[test]
    fn test_extract_track_ids_missing() {
        let json: Value = serde_json::from_str(
            r#"{
            "track_name": "Test Song"
        }"#,
        )
        .unwrap();

        let ids = extract_track_ids_from_json(&json);
        assert!(ids.track_isrcs.is_empty());
        assert!(ids.track_spotify_ids.is_empty());
        assert!(ids.track_itunes_ids.is_empty());
    }
}
