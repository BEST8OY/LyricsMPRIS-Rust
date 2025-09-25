//! Minimal track metadata struct and metadata querying for MPRIS.

use crate::mpris::connection::{MprisError, TIMEOUT, get_dbus_conn};
use dbus::nonblock::Proxy;
use dbus::nonblock::stdintf::org_freedesktop_dbus::Properties;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub length: Option<f64>,
    pub spotify_id: Option<String>,
}

/// Helper to extract a string that might be a single value or the first in an array.
/// The MPRIS spec says artist/album are arrays of strings, but some players send a single string.
fn extract_optional_string(
    variant: &dbus::arg::Variant<Box<dyn dbus::arg::RefArg + 'static>>,
) -> Option<String> {
    use dbus::arg::ArgType;
    match variant.0.arg_type() {
        ArgType::Array => {
            if let Some(mut iter) = variant.0.as_iter() {
                iter.next().and_then(|v| v.as_str()).map(str::to_string)
            } else {
                None
            }
        }
        ArgType::String => variant.0.as_str().map(str::to_string),
        _ => None,
    }
}

/// Extract metadata fields from a D-Bus property map.
pub fn extract_metadata(map: &dbus::arg::PropMap) -> TrackMetadata {
    let title = map
        .get("xesam:title")
        .and_then(|v| v.0.as_str())
        .map(str::to_string)
        .unwrap_or_default();
    let artist = map
        .get("xesam:artist")
        .and_then(extract_optional_string)
        .unwrap_or_default();
    let album = map
        .get("xesam:album")
        .and_then(extract_optional_string)
        .unwrap_or_default();
    let length = map.get("mpris:length").and_then(|v| {
        // DBus may provide the length as a signed or unsigned integer depending on the player.
        v.0.as_i64().map(|i| i as f64 / 1_000_000.0)
            .or_else(|| v.0.as_u64().map(|u| u as f64 / 1_000_000.0))
    });
    // Extract mpris:trackid which is often an object path like
    // "/com/spotify/track/<id>" for Spotify players. We normalize to just
    // the Spotify track id if present.
    let spotify_id = map.get("mpris:trackid").and_then(|v| v.0.as_str()).and_then(|s| {
        if let Some(idx) = s.rfind('/') {
            let candidate = &s[idx + 1..];
            if !candidate.is_empty() {
                return Some(candidate.to_string());
            }
        }
        // support spotify URI form as fallback
        if let Some(idx) = s.find("spotify:track:") {
            let candidate = &s[idx + "spotify:track:".len()..];
            if !candidate.is_empty() {
                return Some(candidate.to_string());
            }
        }
        None
    });
    TrackMetadata { title, artist, album, length, spotify_id }
}

/// Query metadata for a specific MPRIS player service.
pub async fn get_metadata(service: &str) -> Result<TrackMetadata, MprisError> {
    if service.is_empty() {
        return Ok(TrackMetadata::default());
    }
    let conn = get_dbus_conn().await?;
    let proxy = Proxy::new(service, "/org/mpris/MediaPlayer2", TIMEOUT, conn);
    let metadata: Option<dbus::arg::PropMap> =
        Properties::get(&proxy, "org.mpris.MediaPlayer2.Player", "Metadata")
            .await
            .ok();
    Ok(metadata
        .map(|map| extract_metadata(&map))
        .unwrap_or_default())
}
