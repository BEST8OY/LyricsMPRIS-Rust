//! Track metadata parsing and querying for MPRIS.

use crate::mpris::connection::{get_dbus_conn, MprisError};
use std::collections::HashMap;
use zbus::{proxy, zvariant};
use zvariant::{OwnedValue, Type};

/// Track metadata from MPRIS player
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub length: Option<f64>,
    pub spotify_id: Option<String>,
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
        
        // Extract Spotify ID from track ID
        let spotify_id = md.trackid.and_then(|trackid| {
            // Try extracting from path like "/org/mpris/MediaPlayer2/Track/spotify/track/ID"
            if let Some(id) = trackid.rsplit('/').next() {
                if !id.is_empty() && id.len() == 22 {
                    return Some(id.to_string());
                }
            }
            
            // Try extracting from spotify:track:ID format
            if let Some(idx) = trackid.find("spotify:track:") {
                let id = &trackid[idx + "spotify:track:".len()..];
                if !id.is_empty() {
                    return Some(id.to_string());
                }
            }
            
            None
        });

        TrackMetadata {
            title,
            artist,
            album,
            length,
            spotify_id,
        }
    }
}

/// Extract metadata from a raw D-Bus property map
/// 
/// This is used for signal handlers where we receive raw variant maps.
pub fn extract_metadata(map: &HashMap<String, OwnedValue>) -> TrackMetadata {
    // Helper to extract string from variant
    let get_string = |key: &str| -> Option<String> {
        map.get(key).and_then(|v| {
            <&str>::try_from(v).ok().map(String::from)
        })
    };

    // Helper to extract string array from variant
    let get_string_array = |key: &str| -> Option<Vec<String>> {
        map.get(key).and_then(|v| {
            // Try to deserialize directly from OwnedValue
            zvariant::Array::try_from(v.clone())
                .ok()
                .and_then(|arr| {
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
            i64::try_from(v).ok().or_else(|| {
                u64::try_from(v).ok().map(|u| u as i64)
            })
        })
    };

    let title = get_string("xesam:title").unwrap_or_default();
    let artist = get_string_array("xesam:artist")
        .and_then(|arr| arr.into_iter().next())
        .unwrap_or_default();
    let album = get_string_array("xesam:album")
        .and_then(|arr| arr.into_iter().next())
        .unwrap_or_default();
    let length = get_i64("mpris:length").map(|microsecs| microsecs as f64 / 1_000_000.0);

    let spotify_id = get_string("mpris:trackid").and_then(|trackid| {
        // Try extracting from path
        if let Some(id) = trackid.rsplit('/').next() {
            if !id.is_empty() && id.len() == 22 {
                return Some(id.to_string());
            }
        }
        
        // Try spotify:track: format
        if let Some(idx) = trackid.find("spotify:track:") {
            let id = &trackid[idx + "spotify:track:".len()..];
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
        
        None
    });

    TrackMetadata {
        title,
        artist,
        album,
        length,
        spotify_id,
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
        };

        let track: TrackMetadata = md.into();
        assert_eq!(track.title, "Test Song");
        assert_eq!(track.artist, "Artist 1");
        assert_eq!(track.album, "Test Album");
        assert_eq!(track.length, Some(180.0));
    }
}
