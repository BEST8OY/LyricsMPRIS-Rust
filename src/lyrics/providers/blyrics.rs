//! BetterLyrics (`bLyrics`) TTML provider (`BlyricsProvider.java`).

use crate::lyrics::parsers::parse_ttml;
use crate::lyrics::providers::{BoxFuture, FetchedCandidate, LyricsProvider, ProviderContext};
use crate::lyrics::types::{LyricsError, TrackInfo, TrackMatchInfo};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://lyrics-api.boidu.dev/getLyrics";

pub struct BlyricsProvider;

fn base_url(ctx: &ProviderContext) -> String {
    crate::lyrics::providers::endpoint_url(
        ctx.settings.endpoints.blyrics.as_deref(),
        DEFAULT_BASE_URL,
    )
}

impl LyricsProvider for BlyricsProvider {
    fn id(&self) -> &'static str {
        "blyrics"
    }

    fn display_name(&self) -> &'static str {
        "bLyrics"
    }

    fn fetch_candidates<'a>(
        &'a self,
        track: &'a TrackInfo,
        _ids: &'a TrackMatchInfo,
        ctx: &'a ProviderContext,
    ) -> BoxFuture<'a, Result<Vec<FetchedCandidate>, LyricsError>> {
        Box::pin(async move {
            if track.title.is_empty() || track.artist.is_empty() {
                return Ok(Vec::new());
            }

            let mut url = format!(
                "{}?s={}&a={}",
                base_url(ctx),
                urlencoding::encode(&track.title),
                urlencoding::encode(&track.artist)
            );
            if track.duration_secs > 0 {
                url.push_str(&format!("&d={}", track.duration_secs));
            }
            if !track.album.is_empty() {
                url.push_str("&al=");
                url.push_str(&urlencoding::encode(&track.album));
            }

            let resp = ctx.client().get(&url).send().await?;
            if !resp.status().is_success() {
                return Ok(Vec::new());
            }

            let root: Value = resp.json().await?;
            let Some(ttml) = root
                .get("ttml")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Ok(Vec::new());
            };

            let Some(lyrics) = parse_ttml(ttml, self.display_name()) else {
                return Ok(Vec::new());
            };
            Ok(FetchedCandidate::blind(lyrics).into_iter().collect())
        })
    }
}
