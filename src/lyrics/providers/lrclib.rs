use serde::Deserialize;

use crate::lyrics::parse::parse_synced_lyrics;
use crate::lyrics::types::{http_client, LyricsError, ProviderResult};

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct LrcLibResponse {
    syncedLyrics: Option<String>,
}

/// Fetch synced lyrics from lrclib.net API.
///
/// The lrclib API provides high-quality community-sourced time-synced lyrics.
/// Matching is improved by including album and duration when available.
pub async fn fetch_lyrics_from_lrclib(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
) -> ProviderResult {
    let url = build_lrclib_url(artist, title, album, duration);
    
    let resp = http_client()
        .get(&url)
        .header("User-Agent", "LyricsMPRIS/1.0")
        .send()
        .await?;

    // 404 means no lyrics found - not an error
    if resp.status().as_u16() == 404 {
        return Ok((Vec::new(), None));
    }

    if !resp.status().is_success() {
        return Err(LyricsError::Api(format!(
            "lrclib: HTTP {}",
            resp.status()
        )));
    }

    let response: LrcLibResponse = resp.json().await?;
    
    match response.syncedLyrics {
        Some(synced) if !synced.is_empty() => {
            let parsed = parse_synced_lyrics(&synced);
            Ok((parsed, Some(synced)))
        }
        _ => Ok((Vec::new(), None)),
    }
}

/// Build lrclib API URL with query parameters.
fn build_lrclib_url(artist: &str, title: &str, album: &str, duration: Option<f64>) -> String {
    let mut params = vec![
        format!("artist_name={}", urlencoding::encode(artist)),
        format!("track_name={}", urlencoding::encode(title)),
    ];

    if !album.is_empty() {
        params.push(format!("album_name={}", urlencoding::encode(album)));
    }

    if let Some(d) = duration {
        // API expects duration in seconds (integer)
        params.push(format!("duration={}", d.round() as i64));
    }

    format!("https://lrclib.net/api/get?{}", params.join("&"))
}
