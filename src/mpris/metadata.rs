//! Track metadata parsing and querying for MPRIS.

use crate::mpris::connection::{MprisError, get_dbus_conn};
use crate::mpris::proxies::MediaPlayer2PlayerProxy;
use lofty::file::TaggedFileExt;
use lofty::prelude::ItemKey;
use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::task::block_in_place;
use zvariant::OwnedValue;

static ISRC_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable or disable ISRC-based metadata lookup.
pub fn set_isrc_enabled(enabled: bool) {
    ISRC_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Returns true if ISRC lookup is enabled.
fn isrc_enabled() -> bool {
    ISRC_ENABLED.load(Ordering::Relaxed)
}

/// Track metadata from MPRIS player
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub length: Option<f64>,
    pub spotify_id: Option<String>,
    pub isrc: Option<String>,
    pub itunes_id: Option<String>,
}

/// Try to read ISRC from a local audio file path or URL using lofty.
///
/// Returns the ISRC code string if found, or None.
fn get_isrc_from_file(raw_path: &str) -> Option<String> {
    let path_str = raw_path.strip_prefix("file://").unwrap_or(raw_path);
    let decoded = urlencoding::decode(path_str).ok()?;
    let path_buf = PathBuf::from(decoded.as_ref());

    let read_tags = || lofty::read_from_path(path_buf).ok();
    let tagged_file = match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            block_in_place(read_tags)?
        }
        _ => read_tags()?,
    };

    for tag in tagged_file.tags() {
        for item in tag.items() {
            if matches!(item.key(), ItemKey::Isrc)
                && let Some(value) = item.value().text()
            {
                let trimmed = value.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }
    None
}

fn normalize_spotify_id(id: &str) -> Option<String> {
    let id = id.trim();
    if id.len() != 22 {
        return None;
    }
    Some(id.to_string())
}

fn normalize_spotify_url(url: &str) -> Option<String> {
    let prefix = "https://open.spotify.com/track/";
    if let Some(idx) = url.find(prefix) {
        let id_part = &url[idx + prefix.len()..];
        let id_part = id_part.split('?').next().unwrap_or("");
        return normalize_spotify_id(id_part);
    }
    None
}

fn normalize_spotify_trackid(trackid: &str) -> Option<String> {
    if let Some(idx) = trackid.find("spotify:track:")
        && let Some(norm) = normalize_spotify_id(&trackid[idx + "spotify:track:".len()..])
    {
        return Some(norm);
    }
    if let Some(idx) = trackid.find("/com/spotify/track/")
        && let Some(norm) = normalize_spotify_id(&trackid[idx + "/com/spotify/track/".len()..])
    {
        return Some(norm);
    }
    if trackid.contains("spotify")
        && let Some(id) = trackid.rsplit('/').next().filter(|id| !id.is_empty())
        && let Some(norm) = normalize_spotify_id(id)
    {
        return Some(norm);
    }
    None
}

/// Try to get ISRC from the local audio file.
///
/// Returns the ISRC code string if found in the file's metadata, or None.
fn isrc_from_metadata(url: Option<&str>) -> Option<String> {
    let file_url = url?;
    if file_url.starts_with("file://") || file_url.starts_with('/') {
        return get_isrc_from_file(file_url);
    }
    None
}

/// Check if an MPRIS `mpris:trackid` represents an explicit canonical "NoTrack" sentinel
/// defined in the MPRIS specification (e.g. `/org/mpris/MediaPlayer2/TrackList/NoTrack`,
/// `/org/mpris/MediaPlayer2/NoTrack`, `NoTrack`).
pub fn is_explicit_no_track(trackid: &str) -> bool {
    let trimmed = trackid.trim();
    trimmed.ends_with("/NoTrack") || trimmed.eq_ignore_ascii_case("NoTrack")
}

/// Check if an MPRIS `mpris:trackid` represents the canonical "NoTrack" sentinel
/// defined in the MPRIS specification (e.g. `/org/mpris/MediaPlayer2/TrackList/NoTrack`,
/// `/org/mpris/MediaPlayer2/NoTrack`, `NoTrack`, or `/`).
pub fn is_no_track(trackid: &str) -> bool {
    let trimmed = trackid.trim();
    trimmed.is_empty() || trimmed == "/" || is_explicit_no_track(trimmed)
}

/// Extract the first non-empty trimmed string from either a string variant or an array of strings.
/// Borrows `val.deref()` directly to avoid heap clones of `OwnedValue`.
pub fn extract_string_or_first_item(val: &OwnedValue) -> Option<String> {
    match val.deref() {
        zvariant::Value::Str(s) => {
            let trimmed = s.as_str().trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        zvariant::Value::ObjectPath(p) => {
            let trimmed = p.as_str().trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        zvariant::Value::Array(arr) => {
            for elem in arr.iter() {
                if let Ok(s) = <&str>::try_from(elem) {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Extract track length in seconds from either i64 or u64 microseconds.
pub fn extract_length_seconds(val: &OwnedValue) -> Option<f64> {
    if let Ok(microsecs) = i64::try_from(val) {
        return Some(microsecs as f64 / 1_000_000.0);
    }
    if let Ok(microsecs) = u64::try_from(val) {
        return Some(microsecs as f64 / 1_000_000.0);
    }
    None
}

/// Extract metadata from a raw D-Bus property map
///
/// This is used for signal handlers where we receive raw variant maps.
pub fn extract_metadata(map: &HashMap<String, OwnedValue>) -> TrackMetadata {
    let trackid = map
        .get("mpris:trackid")
        .and_then(extract_string_or_first_item);
    let title = map
        .get("xesam:title")
        .and_then(extract_string_or_first_item)
        .unwrap_or_default();

    // Check for explicit "NoTrack" sentinel or empty sentinel with empty title
    if let Some(ref id) = trackid {
        let is_explicit = is_explicit_no_track(id);
        let is_empty_sentinel = is_no_track(id) && title.is_empty();
        if is_explicit || is_empty_sentinel {
            return TrackMetadata::default();
        }
    }

    let artist = map
        .get("xesam:artist")
        .and_then(extract_string_or_first_item)
        .unwrap_or_default();

    let album = map
        .get("xesam:album")
        .and_then(extract_string_or_first_item)
        .unwrap_or_default();

    let length = map.get("mpris:length").and_then(extract_length_seconds);

    let url = map.get("xesam:url").and_then(extract_string_or_first_item);

    // Extract Spotify ID — try xesam:url first (https://open.spotify.com/track/ID),
    // then fall back to mpris:trackid (/com/spotify/track/ID or spotify:track:ID).
    let spotify_id = url
        .as_deref()
        .and_then(normalize_spotify_url)
        .or_else(|| trackid.as_deref().and_then(normalize_spotify_trackid));

    let isrc = if isrc_enabled() {
        isrc_from_metadata(url.as_deref())
    } else {
        None
    };

    TrackMetadata {
        title,
        artist,
        album,
        length,
        spotify_id,
        isrc,
        itunes_id: None,
    }
}

/// Query metadata for a specific MPRIS player service
pub async fn get_metadata(service: &str) -> Result<TrackMetadata, MprisError> {
    if service.is_empty() {
        return Ok(TrackMetadata::default());
    }

    let conn = get_dbus_conn().await?;

    let proxy = MediaPlayer2PlayerProxy::builder(&conn)
        .destination(service)?
        .build()
        .await?;

    match proxy.metadata().await {
        Ok(metadata_map) => Ok(extract_metadata(&metadata_map)),
        Err(_) => Ok(TrackMetadata::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_conversion() {
        let mut map = HashMap::new();
        map.insert(
            "xesam:title".to_string(),
            OwnedValue::try_from(zvariant::Value::from("Test Song")).unwrap(),
        );
        map.insert(
            "xesam:artist".to_string(),
            OwnedValue::try_from(zvariant::Value::from(vec!["Artist 1", "Artist 2"])).unwrap(),
        );
        map.insert(
            "xesam:album".to_string(),
            OwnedValue::try_from(zvariant::Value::from("Test Album")).unwrap(),
        );
        map.insert(
            "mpris:length".to_string(),
            OwnedValue::try_from(zvariant::Value::from(180_000_000i64)).unwrap(),
        );

        let track = extract_metadata(&map);
        assert_eq!(track.title, "Test Song");
        assert_eq!(track.artist, "Artist 1");
        assert_eq!(track.album, "Test Album");
        assert_eq!(track.length, Some(180.0));
    }

    #[test]
    fn test_notrack_sentinel() {
        assert!(is_explicit_no_track(
            "/org/mpris/MediaPlayer2/TrackList/NoTrack"
        ));
        assert!(is_explicit_no_track("/org/mpris/MediaPlayer2/NoTrack"));
        assert!(is_explicit_no_track("/NoTrack"));
        assert!(is_explicit_no_track("NoTrack"));
        assert!(!is_explicit_no_track("/"));
        assert!(!is_explicit_no_track(""));
        assert!(!is_explicit_no_track("/org/mpris/MediaPlayer2/track/123"));

        assert!(is_no_track("/org/mpris/MediaPlayer2/TrackList/NoTrack"));
        assert!(is_no_track("/org/mpris/MediaPlayer2/NoTrack"));
        assert!(is_no_track("/NoTrack"));
        assert!(is_no_track("NoTrack"));
        assert!(is_no_track("/"));
        assert!(!is_no_track("/org/mpris/MediaPlayer2/track/123"));

        // Raw variant map with stale title and explicit NoTrack sentinel
        let mut map = HashMap::new();
        map.insert(
            "mpris:trackid".to_string(),
            OwnedValue::try_from(zvariant::Value::from(
                "/org/mpris/MediaPlayer2/TrackList/NoTrack",
            ))
            .unwrap(),
        );
        map.insert(
            "xesam:title".to_string(),
            OwnedValue::try_from(zvariant::Value::from("Stale Lingering Title")).unwrap(),
        );
        map.insert(
            "xesam:artist".to_string(),
            OwnedValue::try_from(zvariant::Value::from(vec!["Stale Artist"])).unwrap(),
        );
        let extracted = extract_metadata(&map);
        assert_eq!(extracted, TrackMetadata::default());
    }

    #[test]
    fn test_spotify_url_and_trackid_normalization() {
        assert_eq!(
            normalize_spotify_url("https://open.spotify.com/track/5FVd6KXrgO9B3JPmC8OPst?si=123"),
            Some("5FVd6KXrgO9B3JPmC8OPst".to_string())
        );
        assert_eq!(
            normalize_spotify_trackid("spotify:track:5FVd6KXrgO9B3JPmC8OPst"),
            Some("5FVd6KXrgO9B3JPmC8OPst".to_string())
        );
        assert_eq!(
            normalize_spotify_trackid("/com/spotify/track/5FVd6KXrgO9B3JPmC8OPst"),
            Some("5FVd6KXrgO9B3JPmC8OPst".to_string())
        );
        assert_eq!(normalize_spotify_id("too_short"), None);
    }

    #[test]
    fn test_isrc_from_metadata_nonexistent_file() {
        struct IsrcGuard(bool);
        impl Drop for IsrcGuard {
            fn drop(&mut self) {
                set_isrc_enabled(self.0);
            }
        }
        let _guard = IsrcGuard(isrc_enabled());

        set_isrc_enabled(true);
        assert_eq!(
            isrc_from_metadata(Some("file:///nonexistent/path/to/song.mp3")),
            None
        );
        assert_eq!(
            isrc_from_metadata(Some("https://example.com/song.mp3")),
            None
        );
    }

    #[test]
    fn test_extract_metadata_with_array_and_str_variants() {
        let mut map = HashMap::new();
        map.insert(
            "xesam:title".to_string(),
            OwnedValue::try_from(zvariant::Value::from("Test Title")).unwrap(),
        );
        map.insert(
            "xesam:artist".to_string(),
            OwnedValue::try_from(zvariant::Value::from(vec!["Artist A", "Artist B"])).unwrap(),
        );
        map.insert(
            "xesam:album".to_string(),
            OwnedValue::try_from(zvariant::Value::from("Album Name")).unwrap(),
        );
        map.insert(
            "mpris:length".to_string(),
            OwnedValue::try_from(zvariant::Value::from(210_000_000i64)).unwrap(),
        );

        let track = extract_metadata(&map);
        assert_eq!(track.title, "Test Title");
        assert_eq!(track.artist, "Artist A");
        assert_eq!(track.album, "Album Name");
        assert_eq!(track.length, Some(210.0));
    }

    #[test]
    fn test_non_spotify_trackid_rejected() {
        // A 22-character trailing segment on a non-Spotify path must NOT be parsed as Spotify ID
        assert_eq!(
            normalize_spotify_trackid("/org/mpris/MediaPlayer2/Track/1234567890123456789012"),
            None
        );
        assert_eq!(
            normalize_spotify_trackid("/org/videolan/vlc/track/1234567890123456789012"),
            None
        );
    }
}
