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
use zbus::{OwnedVariant, Variant};

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
    pub youtube_video_id: Option<String>,
    pub is_local: bool,
}

/// Returns true if an MPRIS `xesam:url` points to a local file (`file://` or `/`).
pub fn is_local_media_url(url: &str) -> bool {
    let trimmed = url.trim();
    trimmed
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file://"))
        || trimmed.starts_with('/')
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
    if is_local_media_url(file_url) {
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
/// Borrows `val.deref()` directly to avoid heap clones of `OwnedVariant`.
pub fn extract_string_or_first_item(val: &OwnedVariant) -> Option<String> {
    match val.deref() {
        Variant::Str(s) => {
            let trimmed = s.as_str().trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Variant::ObjectPath(p) => {
            let trimmed = p.as_str().trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Variant::Array(arr) => {
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
pub fn extract_length_seconds(val: &OwnedVariant) -> Option<f64> {
    if let Ok(microsecs) = i64::try_from(val) {
        return (microsecs >= 0).then_some(microsecs as f64 / 1_000_000.0);
    }
    if let Ok(microsecs) = u64::try_from(val) {
        return Some(microsecs as f64 / 1_000_000.0);
    }
    None
}

/// Extract metadata from a raw D-Bus property map
///
/// This is used for signal handlers where we receive raw variant maps.
pub fn extract_metadata(map: &HashMap<String, OwnedVariant>) -> TrackMetadata {
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

    let youtube_video_id = extract_youtube_video_id(url.as_deref(), trackid.as_deref());

    let is_local = url.as_deref().is_some_and(is_local_media_url);

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
        youtube_video_id,
        is_local,
    }
}

fn is_valid_yt_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Extracts an 11-character YouTube `videoId` from `xesam:url` or YouTube-specific `mpris:trackid`.
pub fn extract_youtube_video_id(url: Option<&str>, trackid: Option<&str>) -> Option<String> {
    if let Some(raw_url) = url {
        let u = raw_url.trim();
        let lower = u.to_ascii_lowercase();
        if lower.contains("youtu.be/") {
            if let Some(rest) = u
                .split_once("youtu.be/")
                .map(|(_, r)| r.split(['?', '#', '&', '/']).next().unwrap_or(""))
                && is_valid_yt_id(rest)
            {
                return Some(rest.to_string());
            }
        } else if lower.contains("youtube.com/") || lower.contains("youtube-nocookie.com/") {
            if let Some((_, query)) = u.split_once('?') {
                for pair in query.split('&') {
                    if let Some(("v", val)) = pair.split_once('=') {
                        let id = val.split('#').next().unwrap_or("");
                        if is_valid_yt_id(id) {
                            return Some(id.to_string());
                        }
                    }
                }
            }
            for marker in ["/shorts/", "/embed/", "/live/", "/v/"] {
                if let Some((_, rest)) = u.split_once(marker) {
                    let id = rest.split(['?', '#', '&', '/']).next().unwrap_or("");
                    if is_valid_yt_id(id) {
                        return Some(id.to_string());
                    }
                }
            }
        }
    }

    if let Some(tid) = trackid {
        let t = tid.trim();
        if let Some((_, rest)) = t.split_once("youtube:video:") {
            let id = rest.split(['?', '#', '&', '/']).next().unwrap_or("");
            if is_valid_yt_id(id) {
                return Some(id.to_string());
            }
        }
        if t.to_ascii_lowercase().contains("youtube")
            && let Some((_, query)) = t.split_once("v=")
        {
            let id = query.split(['&', '#', '/']).next().unwrap_or("");
            if is_valid_yt_id(id) {
                return Some(id.to_string());
            }
        }
    }

    None
}

/// Query metadata for a specific MPRIS player service
pub async fn get_metadata(service: &str) -> Result<TrackMetadata, MprisError> {
    if service.is_empty() {
        return Ok(TrackMetadata::default());
    }

    let conn = get_dbus_conn().await?;

    let proxy = MediaPlayer2PlayerProxy::builder(&conn)
        .destination(service)
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
            OwnedVariant::try_from(Variant::from("Test Song")).unwrap(),
        );
        map.insert(
            "xesam:artist".to_string(),
            OwnedVariant::try_from(Variant::from(vec!["Artist 1", "Artist 2"])).unwrap(),
        );
        map.insert(
            "xesam:album".to_string(),
            OwnedVariant::try_from(Variant::from("Test Album")).unwrap(),
        );
        map.insert(
            "mpris:length".to_string(),
            OwnedVariant::try_from(Variant::from(180_000_000i64)).unwrap(),
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
            OwnedVariant::try_from(Variant::from("/org/mpris/MediaPlayer2/TrackList/NoTrack"))
                .unwrap(),
        );
        map.insert(
            "xesam:title".to_string(),
            OwnedVariant::try_from(Variant::from("Stale Lingering Title")).unwrap(),
        );
        map.insert(
            "xesam:artist".to_string(),
            OwnedVariant::try_from(Variant::from(vec!["Stale Artist"])).unwrap(),
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
            OwnedVariant::try_from(Variant::from("Test Title")).unwrap(),
        );
        map.insert(
            "xesam:artist".to_string(),
            OwnedVariant::try_from(Variant::from(vec!["Artist A", "Artist B"])).unwrap(),
        );
        map.insert(
            "xesam:album".to_string(),
            OwnedVariant::try_from(Variant::from("Album Name")).unwrap(),
        );
        map.insert(
            "mpris:length".to_string(),
            OwnedVariant::try_from(Variant::from(210_000_000i64)).unwrap(),
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

    #[test]
    fn test_extract_metadata_local_vs_remote_url() {
        assert!(is_local_media_url("file:///home/user/Music/song.flac"));
        assert!(is_local_media_url("FILE:///tmp/test.mp3"));
        assert!(is_local_media_url("/home/user/Music/song.ogg"));
        assert!(!is_local_media_url("https://open.spotify.com/track/123"));
        assert!(!is_local_media_url("http://stream.example.com/live"));
        assert!(!is_local_media_url(""));

        let mut local_map = HashMap::new();
        local_map.insert(
            "xesam:title".to_string(),
            OwnedVariant::try_from(Variant::from("Local Song")).unwrap(),
        );
        local_map.insert(
            "xesam:url".to_string(),
            OwnedVariant::try_from(Variant::from("file:///music/local.ogg")).unwrap(),
        );
        assert!(extract_metadata(&local_map).is_local);

        let mut remote_map = HashMap::new();
        remote_map.insert(
            "xesam:title".to_string(),
            OwnedVariant::try_from(Variant::from("Stream Song")).unwrap(),
        );
        remote_map.insert(
            "xesam:url".to_string(),
            OwnedVariant::try_from(Variant::from("https://example.com/stream.opus")).unwrap(),
        );
        assert!(!extract_metadata(&remote_map).is_local);
    }

    #[test]
    fn test_extract_youtube_video_id() {
        assert_eq!(
            extract_youtube_video_id(
                Some("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RD"),
                None
            ),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            extract_youtube_video_id(Some("https://youtu.be/dQw4w9WgXcQ?t=10"), None),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            extract_youtube_video_id(Some("https://www.youtube.com/shorts/abcDEF12_34"), None),
            Some("abcDEF12_34".to_string())
        );
        assert_eq!(
            extract_youtube_video_id(None, Some("youtube:video:dQw4w9WgXcQ")),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            extract_youtube_video_id(Some("https://open.spotify.com/track/123"), None),
            None
        );
    }
}
