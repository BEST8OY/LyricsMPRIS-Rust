use serde::Deserialize;

use crate::lyrics::parse::parse_synced_lyrics;
use crate::lyrics::types::{LyricsError, ProviderResult, http_client};

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct LrcLibResp {
    syncedLyrics: Option<String>,
}

/// Fetch lyrics from lrclib for a given artist and title.
pub async fn fetch_lyrics_from_lrclib(artist: &str, title: &str) -> ProviderResult {
    let client = http_client();
    let url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        urlencoding::encode(artist),
        urlencoding::encode(title)
    );
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
