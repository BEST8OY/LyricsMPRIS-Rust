//! SQLite caching engine for synchronized lyrics.
//!
//! Provides indexed local storage for lyrics fetched from external providers
//! with zstd compression, atomic transactions, multi-ID indexing (ISRC,
//! Spotify ID, iTunes ID), and automatic duration mismatch invalidation.

pub mod compress;
pub mod ops;
pub mod schema;
#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub use ops::{fetch_from_database_inner, store_in_database_inner};
#[allow(unused_imports)]
pub use schema::{DURATION_TOLERANCE_SECS, LyricsEntry, LyricsFormat};

use crate::lyrics::types::ProviderResult;
use schema::open_database;
use sqlx::sqlite::SqlitePool;
use std::path::PathBuf;

/// Global SQLite connection pool.
/// Pool maintains a small number of connections, reusing them efficiently.
static DB_POOL: tokio::sync::OnceCell<SqlitePool> = tokio::sync::OnceCell::const_new();

/// Initializes the SQLite database.
///
/// This should be called once at application startup.
/// Creates the database file and schema if they don't exist.
pub async fn initialize(path: PathBuf) {
    match open_database(&path).await {
        Ok(pool) => {
            tracing::info!(
                path = %path.display(),
                "SQLite database initialized"
            );
            let _ = DB_POOL.set(pool);
        }
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "Failed to initialize SQLite database"
            );
        }
    }
}

/// Attempts to fetch lyrics from the database cache.
///
/// Uses indexed SQL query for fast lookup with minimal memory usage.
/// Falls back to lookup by track identifiers (ISRC, Spotify ID, iTunes ID)
/// when primary lookups fail.
///
/// # Returns
///
/// - `Some(result)` if lyrics are found in the database
/// - `None` if not found (should proceed to external providers)
pub async fn fetch_from_database(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    isrc: Option<&str>,
    spotify_id: Option<&str>,
    itunes_id: Option<&str>,
) -> Option<ProviderResult> {
    let pool = DB_POOL.get()?;
    ops::fetch_from_database_inner(
        pool, artist, title, album, duration, isrc, spotify_id, itunes_id,
    )
    .await
}

/// Stores lyrics in the database cache.
///
/// Automatically compresses the payload using zstd and registers associated
/// track identifiers in junction indexing tables within an atomic transaction.
#[allow(clippy::too_many_arguments)]
pub async fn store_in_database(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    format: LyricsFormat,
    raw_lyrics: String,
    isrcs: &[String],
    spotify_ids: &[String],
    itunes_ids: &[String],
) {
    let Some(pool) = DB_POOL.get() else {
        return;
    };
    ops::store_in_database_inner(
        pool,
        artist,
        title,
        album,
        duration,
        format,
        raw_lyrics,
        isrcs,
        spotify_ids,
        itunes_ids,
    )
    .await;
}
