//! Database schema definition, types, and connection setup.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

/// Maximum allowable duration difference (in seconds) between the queried track
/// and the cached entry before invalidating and purging the cache row.
pub const DURATION_TOLERANCE_SECS: f64 = 2.0;

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
    #[must_use]
    pub fn to_str(&self) -> &'static str {
        match self {
            Self::Lrclib => "lrclib",
            Self::Richsync => "richsync",
            Self::Subtitles => "subtitles",
        }
    }

    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
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

/// Creates the database schema and indexes if they don't exist.
pub(crate) async fn create_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
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

    // Create table for multi-ISRC indexing
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS track_isrcs (
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            album TEXT NOT NULL,
            isrc TEXT NOT NULL,
            PRIMARY KEY (artist, title, album, isrc)
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_track_isrcs_isrc ON track_isrcs(isrc)
        "#,
    )
    .execute(pool)
    .await?;

    // Backfill existing ISRCs from lyrics into track_isrcs table if any exist
    sqlx::query(
        r#"
        INSERT OR IGNORE INTO track_isrcs (artist, title, album, isrc)
        SELECT artist, title, album, isrc FROM lyrics WHERE isrc IS NOT NULL AND isrc != ''
        "#,
    )
    .execute(pool)
    .await?;

    // Create table for multi-Spotify ID indexing
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS track_spotify_ids (
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            album TEXT NOT NULL,
            spotify_id TEXT NOT NULL,
            PRIMARY KEY (artist, title, album, spotify_id)
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_track_spotify_ids_spotify_id ON track_spotify_ids(spotify_id)
        "#,
    )
    .execute(pool)
    .await?;

    // Backfill existing spotify_ids from lyrics into track_spotify_ids table if any exist
    sqlx::query(
        r#"
        INSERT OR IGNORE INTO track_spotify_ids (artist, title, album, spotify_id)
        SELECT artist, title, album, spotify_id FROM lyrics WHERE spotify_id IS NOT NULL AND spotify_id != ''
        "#,
    )
    .execute(pool)
    .await?;

    // Create table for multi-iTunes ID indexing
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS track_itunes_ids (
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            album TEXT NOT NULL,
            itunes_id TEXT NOT NULL,
            PRIMARY KEY (artist, title, album, itunes_id)
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_track_itunes_ids_itunes_id ON track_itunes_ids(itunes_id)
        "#,
    )
    .execute(pool)
    .await?;

    // Backfill existing itunes_ids from lyrics into track_itunes_ids table if any exist
    sqlx::query(
        r#"
        INSERT OR IGNORE INTO track_itunes_ids (artist, title, album, itunes_id)
        SELECT artist, title, album, itunes_id FROM lyrics WHERE itunes_id IS NOT NULL AND itunes_id != ''
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
pub(crate) async fn open_database(path: &Path) -> Result<SqlitePool, sqlx::Error> {
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
