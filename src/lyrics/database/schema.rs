//! Database schema definition, types, and connection setup.

use crate::lyrics::types::TrackMatchInfo;
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
#[allow(dead_code)]
pub struct LyricsEntry {
    pub id: i64,
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub format: LyricsFormat,
    pub raw_lyrics: String,
    pub track_ids: TrackMatchInfo,
}

/// Creates the database schema and indexes if they don't exist.
pub(crate) async fn create_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("PRAGMA foreign_keys = ON;")
        .execute(pool)
        .await?;

    // Canonical lyrics table (one row per unique recording / lyrics payload)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lyrics (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            duration REAL,
            format TEXT NOT NULL,
            raw_lyrics BLOB NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Multi-album alias table mapping (artist, title, album) -> canonical lyrics_id
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS track_aliases (
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            album TEXT NOT NULL,
            lyrics_id INTEGER NOT NULL REFERENCES lyrics(id) ON DELETE CASCADE,
            PRIMARY KEY (artist, title, album)
        ) WITHOUT ROWID
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_track_aliases_lyrics_id
        ON track_aliases(lyrics_id)
        "#,
    )
    .execute(pool)
    .await?;

    // Unified identifier table mapping (kind, value) -> unique canonical lyrics_id
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS track_identifiers (
            kind TEXT NOT NULL,
            value TEXT NOT NULL,
            lyrics_id INTEGER NOT NULL REFERENCES lyrics(id) ON DELETE CASCADE,
            ordering INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (kind, value)
        ) WITHOUT ROWID
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_track_identifiers_lyrics_id
        ON track_identifiers(lyrics_id)
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
        .foreign_keys(true)
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
