//! Track metadata parsing and querying for MPRIS.

use crate::mpris::connection::{MprisError, get_dbus_conn};
use lofty::file::TaggedFileExt;
use lofty::prelude::ItemKey;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use zbus::{proxy, zvariant};
use zvariant::{OwnedValue, Type};

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

/// Internal metadata structure matching MPRIS specification
///
/// Uses zvariant's DeserializeDict to properly handle D-Bus dictionary types.
#[derive(Debug, Type)]
#[zvariant(signature = "a{sv}")]
struct MprisMetadata {
    #[zvariant(rename = "xesam:title")]
    title: Option<String>,
    #[zvariant(rename = "xesam:artist")]
    artist: Option<Vec<String>>,
    #[zvariant(rename = "xesam:album")]
    album: Option<Vec<String>>,
    #[zvariant(rename = "mpris:length")]
    length: Option<i64>,
    #[zvariant(rename = "mpris:trackid")]
    trackid: Option<String>,
    #[zvariant(rename = "xesam:url")]
    url: Option<String>,
}

/// Try to read ISRC from a local audio file using lofty.
///
/// Returns the ISRC code string if found, or None.
fn get_isrc_from_file(file_url: &str) -> Option<String> {
    let path = file_url.strip_prefix("file://")?;
    let decoded = urlencoding::decode(path).ok()?;
    let path_str = decoded.as_ref();

    let tagged_file = lofty::read_from_path(path_str).ok()?;
    for tag in tagged_file.tags() {
        for item in tag.items() {
            if matches!(item.key(), ItemKey::Isrc) {
                if let Some(value) = item.value().text() {
                    let trimmed = value.trim().to_string();
                    if !trimmed.is_empty() {
                        return Some(trimmed);
                    }
                }
            }
        }
    }
    None
}

/// Try to get ISRC from the local audio file.
///
/// Returns the ISRC code string if found in the file's metadata, or None.
fn isrc_from_metadata(url: Option<&str>) -> Option<String> {
    let file_url = url?;
    if !file_url.starts_with("file://") {
        return None;
    }
    get_isrc_from_file(file_url)
}

impl From<MprisMetadata> for TrackMetadata {
    fn from(md: MprisMetadata) -> Self {
        let title = md.title.unwrap_or_default();
        let artist = md
            .artist
            .and_then(|artists| artists.into_iter().next())
            .unwrap_or_default();
        let album = md
            .album
            .and_then(|albums| albums.into_iter().next())
            .unwrap_or_default();

        // Convert microseconds to seconds
        let length = md.length.map(|microsecs| microsecs as f64 / 1_000_000.0);

        // Extract Spotify ID — try xesam:url first (https://open.spotify.com/track/ID),
        // then fall back to mpris:trackid (/com/spotify/track/ID or spotify:track:ID).
        let spotify_id = md
            .url
            .as_ref()
            .and_then(|u| {
                if let Some(idx) = u.find("https://open.spotify.com/track/") {
                    let id = &u[idx + "https://open.spotify.com/track/".len()..];
                    if !id.is_empty() {
                        return Some(id.to_string());
                    }
                }
                None
            })
            .or_else(|| {
                md.trackid.as_ref().and_then(|trackid| {
                    trackid
                        .rsplit('/')
                        .next()
                        .filter(|id| !id.is_empty())
                        .map(|id| id.to_string())
                        .or_else(|| {
                            trackid
                                .find("spotify:track:")
                                .map(|idx| trackid[idx + "spotify:track:".len()..].to_string())
                        })
                })
            });

        let isrc = if isrc_enabled() {
            isrc_from_metadata(md.url.as_deref())
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
}

/// Extract metadata from a raw D-Bus property map
///
/// This is used for signal handlers where we receive raw variant maps.
pub fn extract_metadata(map: &HashMap<String, OwnedValue>) -> TrackMetadata {
    // Helper to extract string from variant
    let get_string = |key: &str| -> Option<String> {
        map.get(key)
            .and_then(|v| <&str>::try_from(v).ok().map(String::from))
    };

    // Helper to extract string array from variant
    let get_string_array = |key: &str| -> Option<Vec<String>> {
        map.get(key).and_then(|v| {
            // Try to deserialize directly from OwnedValue as array
            zvariant::Array::try_from(v.clone()).ok().and_then(|arr| {
                arr.iter()
                    .map(|elem| <&str>::try_from(elem).ok().map(String::from))
                    .collect::<Option<Vec<String>>>()
            })
        })
    };

    // Helper to extract integer from variant
    let get_i64 = |key: &str| -> Option<i64> {
        map.get(key).and_then(|v| {
            // Try both i64 and u64
            i64::try_from(v)
                .ok()
                .or_else(|| u64::try_from(v).ok().map(|u| u as i64))
        })
    };

    let title = get_string("xesam:title").unwrap_or_default();

    // Artist: try array first, fallback to string
    let artist = get_string_array("xesam:artist")
        .and_then(|arr| arr.into_iter().next())
        .or_else(|| get_string("xesam:artist"))
        .unwrap_or_default();

    // Album: try array first, fallback to string
    let album = get_string_array("xesam:album")
        .and_then(|arr| arr.into_iter().next())
        .or_else(|| get_string("xesam:album"))
        .unwrap_or_default();

    let length = get_i64("mpris:length").map(|microsecs| microsecs as f64 / 1_000_000.0);

    let url = get_string("xesam:url");

    // Extract Spotify ID — try xesam:url first (https://open.spotify.com/track/ID),
    // then fall back to mpris:trackid (/com/spotify/track/ID or spotify:track:ID).
    let spotify_id = url
        .as_ref()
        .and_then(|u| {
            if let Some(idx) = u.find("https://open.spotify.com/track/") {
                let id = &u[idx + "https://open.spotify.com/track/".len()..];
                if !id.is_empty() {
                    return Some(id.to_string());
                }
            }
            None
        })
        .or_else(|| {
            get_string("mpris:trackid").and_then(|trackid| {
                trackid
                    .rsplit('/')
                    .next()
                    .filter(|id| !id.is_empty())
                    .map(|id| id.to_string())
                    .or_else(|| {
                        trackid
                            .find("spotify:track:")
                            .map(|idx| trackid[idx + "spotify:track:".len()..].to_string())
                    })
            })
        });

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

/// MPRIS MediaPlayer2.Player interface proxy
#[proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait MediaPlayer2Player {
    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
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
        let md = MprisMetadata {
            title: Some("Test Song".to_string()),
            artist: Some(vec!["Artist 1".to_string(), "Artist 2".to_string()]),
            album: Some(vec!["Test Album".to_string()]),
            length: Some(180_000_000), // 180 seconds in microseconds
            trackid: None,
            url: None,
        };

        let track: TrackMetadata = md.into();
        assert_eq!(track.title, "Test Song");
        assert_eq!(track.artist, "Artist 1");
        assert_eq!(track.album, "Test Album");
        assert_eq!(track.length, Some(180.0));
    }
}
