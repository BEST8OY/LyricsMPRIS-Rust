//! Local lyrics database module.
//!
//! This module provides persistent SQLite-based storage for lyrics to reduce
//! API calls and enable offline playback. Uses SQLite for efficient indexed
//! lookups with minimal memory usage.
//!
//! # Storage Format
//!
//! - **SQLite database** with indexed lookups by artist/title/album
//! - **LRC format** (from LRCLIB): Stored as raw text with `[MM:SS.CC]` timestamps
//! - **Richsync** (from Musixmatch): Stored as unparsed JSON (word-level timing)
//! - **Subtitles** (from Musixmatch): Stored as unparsed JSON (line-level timing)
//!
//! # Memory Usage
//!
//! - **Minimal memory**: SQLite only loads requested rows
//! - **Indexed queries**: Fast lookups without loading entire database
//! - **Connection pool**: Reuses connections efficiently
//! - **No cache needed**: SQLite's internal cache handles frequently-accessed data
//!
//! # Schema
//!
//! ```sql
//! CREATE TABLE lyrics (
//!     artist TEXT NOT NULL,
//!     title TEXT NOT NULL,
//!     album TEXT NOT NULL,
//!     duration REAL,
//!     format TEXT NOT NULL,
//!     raw_lyrics BLOB NOT NULL,
//!     isrc TEXT,
//!     spotify_id TEXT,
//!     itunes_id TEXT
//! );
//! CREATE INDEX idx_lookup ON lyrics(artist, title, album);
//! CREATE INDEX IF NOT EXISTS idx_isrc ON lyrics(isrc);
//! CREATE INDEX IF NOT EXISTS idx_spotify_id ON lyrics(spotify_id);
//! CREATE INDEX IF NOT EXISTS idx_itunes_id ON lyrics(itunes_id);
//! ```
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐
//! │ Fetch Request   │
//! └────────┬────────┘
//!          │
//!          ▼
//! ┌─────────────────┐
//! │ SQL SELECT      │───── Hit ──────▶ Parse & Return
//! │ (indexed)       │
//! └────────┬────────┘
//!          │ Miss
//!          ▼
//! ┌─────────────────┐
//! │ Provider Fetch  │
//! └────────┬────────┘
//!          │
//!          ▼
//! ┌─────────────────┐
//! │ SQL INSERT      │
//! │ (UPSERT)        │
//! └─────────────────┘
//! ```

use crate::lyrics::parse::{parse_richsync_body, parse_subtitle_body, parse_synced_lyrics};
use crate::lyrics::types::{LyricsError, ProviderResult, TrackMatchInfo};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::io::Cursor;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

// ============================================================================
// Database Types
// ============================================================================

/// Format of stored lyrics for correct parsing on retrieval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LyricsFormat {
    /// LRC timestamp format (from LRCLIB provider): `[MM:SS.CC]lyrics`
    Lrclib,
    /// Musixmatch richsync format with word-level timestamps (JSON)
    Richsync,
    /// Musixmatch subtitle format with line-level timestamps (JSON)
    Subtitles,
}

impl LyricsFormat {
    fn to_str(&self) -> &'static str {
        match self {
            Self::Lrclib => "lrclib",
            Self::Richsync => "richsync",
            Self::Subtitles => "subtitles",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "lrclib" => Some(Self::Lrclib),
            "richsync" => Some(Self::Richsync),
            "subtitles" => Some(Self::Subtitles),
            _ => None,
        }
    }
}

/// Database entry for a single track's lyrics (from SQL query).
#[derive(Debug, Clone)]
pub struct LyricsEntry {
    pub duration: Option<f64>,
    pub format: LyricsFormat,
    pub raw_lyrics: String,
    pub isrc: Option<String>,
    pub spotify_id: Option<String>,
    pub itunes_id: Option<String>,
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Normalizes a string for case-insensitive matching.
fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

fn normalize_isrc(s: &str) -> Option<String> {
    let trimmed = s.trim().to_uppercase();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn normalize_track_id(s: &str) -> Option<String> {
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn compress_raw_lyrics(raw: &str) -> Result<Vec<u8>, std::io::Error> {
    // Level 3 is zstd's default and a good balance for small payloads.
    zstd::stream::encode_all(Cursor::new(raw.as_bytes()), 3)
}

fn decompress_raw_lyrics(raw: Vec<u8>) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }

    let decoded = zstd::stream::decode_all(Cursor::new(&raw)).ok()?;
    String::from_utf8(decoded).ok()
}

// ============================================================================
// SQLite Connection & Schema
// ============================================================================

/// Creates the database schema if it doesn't exist.
async fn create_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lyrics (
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            album TEXT NOT NULL,
            duration REAL,
            format TEXT NOT NULL,
            raw_lyrics BLOB NOT NULL,
            isrc TEXT,
            spotify_id TEXT,
            itunes_id TEXT,
            PRIMARY KEY (artist, title, album)
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Assume current schema; do not perform runtime migrations for legacy compatibility

    // Create index for fast lookups by artist/title/album
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_lookup
        ON lyrics(artist, title, album)
        "#,
    )
    .execute(pool)
    .await?;

    // Create indexes for track identifier lookups
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_isrc ON lyrics(isrc)
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_spotify_id ON lyrics(spotify_id)
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_itunes_id ON lyrics(itunes_id)
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Opens or creates a SQLite database connection pool.
async fn open_database(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    // Create parent directory if needed
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // Configure SQLite connection
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal); // Write-Ahead Logging for better concurrency

    // Create connection pool (max 5 connections)
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    // Initialize schema
    create_schema(&pool).await?;

    Ok(pool)
}

// ============================================================================
// Parsing Utilities
// ============================================================================

/// Parses stored lyrics based on their format.
///
/// # Returns
///
/// - `Ok((lines, Some(raw)))` on success with parsed lines and original raw text
/// - `Err` if parsing fails
fn parse_stored_lyrics(entry: &LyricsEntry) -> ProviderResult {
    let ids = TrackMatchInfo {
        track_isrc: entry.isrc.clone(),
        track_spotify_id: entry.spotify_id.clone(),
        track_itunes_id: entry.itunes_id.clone(),
    };
    match entry.format {
        LyricsFormat::Lrclib => {
            let lines = parse_synced_lyrics(&entry.raw_lyrics);
            Ok((lines, Some(entry.raw_lyrics.clone()), ids))
        }
        LyricsFormat::Richsync => {
            // Parse the raw JSON body
            match parse_richsync_body(&entry.raw_lyrics) {
                Some(lines) => {
                    // Return the original JSON as raw
                    Ok((lines, Some(entry.raw_lyrics.clone()), ids))
                }
                _ => Err(LyricsError::Api(
                    "Failed to parse richsync lyrics from database".to_string(),
                )),
            }
        }
        LyricsFormat::Subtitles => {
            // Parse the raw JSON body
            match parse_subtitle_body(&entry.raw_lyrics) {
                Some(lines) => {
                    // Return the original JSON as raw
                    Ok((lines, Some(entry.raw_lyrics.clone()), ids))
                }
                _ => Err(LyricsError::Api(
                    "Failed to parse subtitle lyrics from database".to_string(),
                )),
            }
        }
    }
}

// ============================================================================
// Public API
// ============================================================================

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

/// Attempts to fetch lyrics from the database.
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

    // Normalize search terms for case-insensitive matching
    let artist_norm = normalize(artist);
    let title_norm = normalize(title);
    let album_norm = normalize(album);

    // Try lookup by ISRC first (most reliable unique identifier)
    if let Some(id_norm) = isrc.and_then(normalize_isrc)
        && let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT duration, format, raw_lyrics, isrc, spotify_id, itunes_id
            FROM lyrics
            WHERE isrc = ?
            LIMIT 1
            "#,
        )
        .bind(&id_norm)
        .fetch_optional(pool)
        .await
        && let Some(result) =
            process_db_row(&row, duration, &artist_norm, &title_norm, &album_norm, pool).await
    {
        return Some(result);
    }

    // Fallback: try lookup by artist/title/album
    if let Ok(Some(row)) = sqlx::query(
        r#"
        SELECT duration, format, raw_lyrics, isrc, spotify_id, itunes_id
        FROM lyrics
        WHERE artist = ? AND title = ? AND album = ?
        LIMIT 1
        "#,
    )
    .bind(&artist_norm)
    .bind(&title_norm)
    .bind(&album_norm)
    .fetch_optional(pool)
    .await
        && let Some(result) =
            process_db_row(&row, duration, &artist_norm, &title_norm, &album_norm, pool).await
    {
        return Some(result);
    }

    // Fallback: try lookup by Spotify ID
    if let Some(id_norm) = spotify_id.and_then(normalize_track_id)
        && let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT duration, format, raw_lyrics, isrc, spotify_id, itunes_id
            FROM lyrics
            WHERE spotify_id = ?
            LIMIT 1
            "#,
        )
        .bind(&id_norm)
        .fetch_optional(pool)
        .await
        && let Some(result) =
            process_db_row(&row, duration, &artist_norm, &title_norm, &album_norm, pool).await
    {
        return Some(result);
    }

    // Fallback: try lookup by iTunes ID
    if let Some(id_norm) = itunes_id.and_then(normalize_track_id)
        && let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT duration, format, raw_lyrics, isrc, spotify_id, itunes_id
            FROM lyrics
            WHERE itunes_id = ?
            LIMIT 1
            "#,
        )
        .bind(&id_norm)
        .fetch_optional(pool)
        .await
        && let Some(result) =
            process_db_row(&row, duration, &artist_norm, &title_norm, &album_norm, pool).await
    {
        return Some(result);
    }

    None
}

/// Helper: deletes a cached row from the database using row values or fallback values.
async fn delete_cached_row(
    pool: &SqlitePool,
    row: &sqlx::sqlite::SqliteRow,
    artist_norm: &str,
    title_norm: &str,
    album_norm: &str,
) {
    let row_isrc: Option<String> = row.try_get("isrc").ok().flatten();
    let row_spotify_id: Option<String> = row.try_get("spotify_id").ok().flatten();
    let row_itunes_id: Option<String> = row.try_get("itunes_id").ok().flatten();

    if let Some(isrc) = row_isrc.as_deref().and_then(normalize_isrc) {
        let _ = sqlx::query("DELETE FROM lyrics WHERE isrc = ?")
            .bind(isrc)
            .execute(pool)
            .await;
    } else if let Some(spotify_id) = row_spotify_id.as_deref().and_then(normalize_track_id) {
        let _ = sqlx::query("DELETE FROM lyrics WHERE spotify_id = ?")
            .bind(spotify_id)
            .execute(pool)
            .await;
    } else if let Some(itunes_id) = row_itunes_id.as_deref().and_then(normalize_track_id) {
        let _ = sqlx::query("DELETE FROM lyrics WHERE itunes_id = ?")
            .bind(itunes_id)
            .execute(pool)
            .await;
    } else {
        let _ = sqlx::query("DELETE FROM lyrics WHERE artist = ? AND title = ? AND album = ?")
            .bind(artist_norm)
            .bind(title_norm)
            .bind(album_norm)
            .execute(pool)
            .await;
    }
}

/// Processes a database row into a ProviderResult.
/// Handles decompression, format validation, and duration matching.
async fn process_db_row(
    row: &sqlx::sqlite::SqliteRow,
    duration: Option<f64>,
    artist_norm: &str,
    title_norm: &str,
    album_norm: &str,
    pool: &SqlitePool,
) -> Option<ProviderResult> {
    let raw_lyrics_blob: Vec<u8> = match row.try_get("raw_lyrics") {
        Ok(blob) => blob,
        Err(_) => {
            delete_cached_row(pool, row, artist_norm, title_norm, album_norm).await;
            return None;
        }
    };

    let raw_lyrics = match decompress_raw_lyrics(raw_lyrics_blob) {
        Some(text) => text,
        None => {
            tracing::warn!(
                artist = %artist_norm,
                title = %title_norm,
                "Failed to decompress cached lyrics blob; deleting cache entry"
            );
            delete_cached_row(pool, row, artist_norm, title_norm, album_norm).await;
            return None;
        }
    };

    let format_str: String = match row.try_get("format") {
        Ok(fmt) => fmt,
        Err(_) => {
            delete_cached_row(pool, row, artist_norm, title_norm, album_norm).await;
            return None;
        }
    };

    let format = match LyricsFormat::from_str(&format_str) {
        Some(fmt) => fmt,
        None => {
            tracing::warn!(
                artist = %artist_norm,
                title = %title_norm,
                "Invalid lyrics format in database; deleting cache entry"
            );
            delete_cached_row(pool, row, artist_norm, title_norm, album_norm).await;
            return None;
        }
    };

    let entry = LyricsEntry {
        duration: row.try_get("duration").ok(),
        format,
        raw_lyrics,
        isrc: row.try_get("isrc").ok().flatten(),
        spotify_id: row.try_get("spotify_id").ok().flatten(),
        itunes_id: row.try_get("itunes_id").ok().flatten(),
    };

    // Optional: validate duration match if both are present.
    if let (Some(query_duration), Some(entry_duration)) = (duration, entry.duration) {
        let tolerance = query_duration * 0.05;
        if (query_duration - entry_duration).abs() > tolerance {
            delete_cached_row(pool, row, artist_norm, title_norm, album_norm).await;
            return None;
        }
    }

    match parse_stored_lyrics(&entry) {
        Ok(ok) => Some(Ok(ok)),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to parse cached lyrics; deleting cache entry"
            );
            delete_cached_row(pool, row, artist_norm, title_norm, album_norm).await;
            None
        }
    }
}

/// Stores lyrics in the database.
///
/// Minimal memory usage - only the new entry is in memory briefly.
///
/// This should be called after successfully fetching lyrics from a provider.
#[allow(clippy::too_many_arguments)]
pub async fn store_in_database(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    format: LyricsFormat,
    raw_lyrics: String,
    isrc: Option<&str>,
    spotify_id: Option<&str>,
    itunes_id: Option<&str>,
) {
    let Some(pool) = DB_POOL.get() else {
        return;
    };

    // Normalize for consistent storage
    let artist_norm = normalize(artist);
    let title_norm = normalize(title);
    let album_norm = normalize(album);

    // Insert or replace existing entry
    let raw_lyrics_blob = match compress_raw_lyrics(&raw_lyrics) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(
                artist = %artist,
                title = %title,
                error = %e,
                "Failed to zstd-compress lyrics; skipping database cache"
            );
            return;
        }
    };
    let normalized_isrc = isrc.and_then(normalize_isrc);
    let normalized_spotify_id = spotify_id.and_then(normalize_track_id);
    let normalized_itunes_id = itunes_id.and_then(normalize_track_id);

    let result = sqlx::query(
        r#"
        INSERT OR REPLACE INTO lyrics (artist, title, album, duration, format, raw_lyrics, isrc, spotify_id, itunes_id)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&artist_norm)
    .bind(&title_norm)
    .bind(&album_norm)
    .bind(duration)
    .bind(format.to_str())
    .bind(raw_lyrics_blob)
    .bind(normalized_isrc.as_deref())
    .bind(normalized_spotify_id.as_deref())
    .bind(normalized_itunes_id.as_deref())
    .execute(pool)
    .await;

    if let Err(e) = result {
        tracing::warn!(
            artist = %artist,
            title = %title,
            error = %e,
            "Failed to store lyrics in database"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_database_crud_and_lookups() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();

        create_schema(&pool).await.unwrap();

        let lrc_content = "[00:10.00]Test lyric line 1\n[00:20.00]Test lyric line 2";
        let blob = compress_raw_lyrics(lrc_content).unwrap();

        sqlx::query(
            r#"
            INSERT INTO lyrics (artist, title, album, duration, format, raw_lyrics, isrc, spotify_id, itunes_id)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind("test artist")
        .bind("test title")
        .bind("test album")
        .bind(180.0)
        .bind("lrclib")
        .bind(&blob)
        .bind("USUM71234567")
        .bind("5FVd6KXrgO9B3JPmC8OPst")
        .bind("123456789")
        .execute(&pool)
        .await
        .unwrap();

        // Query by artist/title/album
        let row = sqlx::query(
            "SELECT duration, format, raw_lyrics, isrc, spotify_id, itunes_id FROM lyrics WHERE artist = ? AND title = ? AND album = ?"
        )
        .bind("test artist")
        .bind("test title")
        .bind("test album")
        .fetch_one(&pool)
        .await
        .unwrap();

        let res = process_db_row(
            &row,
            Some(180.0),
            "test artist",
            "test title",
            "test album",
            &pool,
        )
        .await;
        assert!(res.is_some());
        let (lines, raw, ids) = res.unwrap().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(raw, Some(lrc_content.to_string()));
        assert_eq!(ids.track_isrc, Some("USUM71234567".to_string()));
        assert_eq!(
            ids.track_spotify_id,
            Some("5FVd6KXrgO9B3JPmC8OPst".to_string())
        );
        assert_eq!(ids.track_itunes_id, Some("123456789".to_string()));

        // Query by ISRC
        let isrc_row = sqlx::query(
            "SELECT duration, format, raw_lyrics, isrc, spotify_id, itunes_id FROM lyrics WHERE isrc = ?"
        )
        .bind("USUM71234567")
        .fetch_one(&pool)
        .await
        .unwrap();
        let isrc_res = process_db_row(
            &isrc_row,
            Some(180.0),
            "diff artist",
            "diff title",
            "diff album",
            &pool,
        )
        .await;
        assert!(isrc_res.is_some());

        // Test duration tolerance rejection
        let row_mismatch = sqlx::query(
            "SELECT duration, format, raw_lyrics, isrc, spotify_id, itunes_id FROM lyrics WHERE artist = ? AND title = ? AND album = ?"
        )
        .bind("test artist")
        .bind("test title")
        .bind("test album")
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert!(row_mismatch.is_some());
        let res_mismatch = process_db_row(
            &row_mismatch.unwrap(),
            Some(300.0),
            "test artist",
            "test title",
            "test album",
            &pool,
        )
        .await;
        assert!(res_mismatch.is_none());

        // Confirm deleted due to duration mismatch
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0);
    }
}
