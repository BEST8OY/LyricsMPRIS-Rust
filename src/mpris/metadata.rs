//! Minimal track metadata struct and metadata querying for MPRIS.

use crate::mpris::connection::{MprisError, get_dbus_conn};
use zbus::Proxy;
use zvariant::OwnedValue;

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
/// Extract metadata fields from a D-Bus property map.
pub fn extract_metadata(map: &std::collections::HashMap<String, OwnedValue>) -> TrackMetadata {
    // Reuse the serde-path helper to keep parsing logic centralized.
    let serde_md = map_to_serde_metadata(map);
    from_serde(serde_md)
}

// We keep extract_metadata as the canonical parser from a HashMap<String, OwnedValue>.

#[derive(Debug, zvariant::DeserializeDict)]
struct SerdeMetadata {
    #[zvariant(rename = "xesam:title")]
    title: Option<String>,
    #[zvariant(rename = "xesam:artist")]
    artist: Option<Vec<String>>,
    #[zvariant(rename = "xesam:album")]
    album: Option<Vec<String>>,
    #[zvariant(rename = "mpris:length")]
    mpris_length: Option<i64>,
    #[zvariant(rename = "mpris:trackid")]
    trackid: Option<String>,
}

fn from_serde(md: SerdeMetadata) -> TrackMetadata {
    let title = md.title.unwrap_or_default();
    let artist = md.artist.and_then(|v| v.into_iter().next()).unwrap_or_default();
    let album = md.album.and_then(|v| v.into_iter().next()).unwrap_or_default();
    let length = md.mpris_length.map(|i| i as f64 / 1_000_000.0);
    let spotify_id = md.trackid.and_then(|s| {
        if let Some(idx) = s.rfind('/') {
            let candidate = &s[idx + 1..];
            if !candidate.is_empty() {
                return Some(candidate.to_string());
            }
        }
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

/// Build a `SerdeMetadata` from a raw a{sv} `HashMap`.
fn map_to_serde_metadata(map: &std::collections::HashMap<String, OwnedValue>) -> SerdeMetadata {
    let title = map.get("xesam:title").and_then(|v| std::convert::TryInto::<String>::try_into(v.clone()).ok());
    let artist = map.get("xesam:artist").and_then(|v| std::convert::TryInto::<Vec<String>>::try_into(v.clone()).ok());
    let album = map.get("xesam:album").and_then(|v| std::convert::TryInto::<Vec<String>>::try_into(v.clone()).ok());
    let mpris_length = map.get("mpris:length").and_then(|v| {
        if let Ok(i) = std::convert::TryInto::<i64>::try_into(v.clone()) {
            return Some(i);
        }
        if let Ok(u) = std::convert::TryInto::<u64>::try_into(v.clone()) {
            return Some(u as i64);
        }
        None
    });
    let trackid = map.get("mpris:trackid").and_then(|v| std::convert::TryInto::<String>::try_into(v.clone()).ok());
    SerdeMetadata { title, artist, album, mpris_length, trackid }
}

/// Query metadata for a specific MPRIS player service.
pub async fn get_metadata(service: &str) -> Result<TrackMetadata, MprisError> {
    if service.is_empty() {
        return Ok(TrackMetadata::default());
    }
    let conn = get_dbus_conn().await?;
    // Use targeted Properties.Get to avoid triggering GetAll
    let props_proxy = Proxy::new(&conn, service, "/org/mpris/MediaPlayer2", "org.freedesktop.DBus.Properties").await?;
    if let Ok(reply) = props_proxy.call_method("Get", &("org.mpris.MediaPlayer2.Player", "Metadata")).await {
        if let Ok(val) = reply.body().deserialize::<OwnedValue>() {
            // Preferred path: convert the OwnedValue into a HashMap and then
            // build the SerdeMetadata from that HashMap.
            if let Ok(map) = std::convert::TryInto::<std::collections::HashMap<String, OwnedValue>>::try_into(val.clone()) {
                let title = map.get("xesam:title").and_then(|v| std::convert::TryInto::<String>::try_into(v.clone()).ok());
                let artist = map.get("xesam:artist").and_then(|v| std::convert::TryInto::<Vec<String>>::try_into(v.clone()).ok());
                let album = map.get("xesam:album").and_then(|v| std::convert::TryInto::<Vec<String>>::try_into(v.clone()).ok());
                let mpris_length = map.get("mpris:length").and_then(|v| {
                    if let Ok(i) = std::convert::TryInto::<i64>::try_into(v.clone()) {
                        return Some(i);
                    }
                    if let Ok(u) = std::convert::TryInto::<u64>::try_into(v.clone()) {
                        return Some(u as i64);
                    }
                    None
                });
                let trackid = map.get("mpris:trackid").and_then(|v| std::convert::TryInto::<String>::try_into(v.clone()).ok());

                let serde_md = SerdeMetadata {
                    title,
                    artist,
                    album,
                    mpris_length,
                    trackid,
                };
                return Ok(from_serde(serde_md));
            }
            // If we couldn't turn it into a map, fall back to the generic parser
            if let Ok(map) = std::convert::TryInto::<std::collections::HashMap<String, OwnedValue>>::try_into(val) {
                return Ok(extract_metadata(&map));
            }
        }
    }
    // Fallback: no metadata or deserialization failed
    Ok(TrackMetadata::default())
}
