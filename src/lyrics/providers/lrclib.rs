use serde::Deserialize;

use crate::lyrics::parse::parse_synced_lyrics;
use crate::lyrics::types::{LyricsError, ProviderResult, http_client};

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct LrcLibResp {
    syncedLyrics: Option<String>,
}

/// Fetch lyrics from lrclib for a given artist and title. Optionally include track duration
/// (seconds) to improve matching accuracy.
pub async fn fetch_lyrics_from_lrclib(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
) -> ProviderResult {
    let client = http_client();
    // Build query parameters and URL; encode values to be safe.
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("artist_name={}", urlencoding::encode(artist)));
    parts.push(format!("track_name={}", urlencoding::encode(title)));
    // Include album_name when provided to improve matching.
    if !album.is_empty() {
        parts.push(format!("album_name={}", urlencoding::encode(album)));
    }
    if let Some(d) = duration {
        // lrclib expects duration in seconds (integer). Round to nearest second.
        let secs = d.round() as i64;
        parts.push(format!("duration={}", secs));
    }
    let url = format!("https://lrclib.net/api/get?{}", parts.join("&"));
    let resp = client
        .get(&url)
        .header("User-Agent", "LyricsMPRIS/1.0")
        .send()
        .await?;
    if resp.status().as_u16() == 404 {
        return Ok((Vec::new(), None));
    }
    if !resp.status().is_success() {
        return Err(LyricsError::Api(format!(
            "lrclib: unexpected status {}",
            resp.status()
        )));
    }
    let api: LrcLibResp = resp.json().await?;
    let synced = api.syncedLyrics.unwrap_or_default();
    if synced.is_empty() {
        Ok((Vec::new(), None))
    } else {
        let parsed = parse_synced_lyrics(&synced);
        Ok((parsed, Some(synced)))
    }
}
