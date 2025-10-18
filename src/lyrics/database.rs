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
//!     id INTEGER PRIMARY KEY,
//!     artist TEXT NOT NULL,
//!     title TEXT NOT NULL,
//!     album TEXT NOT NULL,
//!     duration REAL,
//!     format TEXT NOT NULL,
//!     raw_lyrics TEXT NOT NULL
//! );
//! CREATE INDEX idx_lookup ON lyrics(artist, title, album);
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
use crate::lyrics::types::{LyricsError, ProviderResult};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
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
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Normalizes a string for case-insensitive matching.
fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

// ============================================================================
// SQLite Connection & Schema
// ============================================================================

/// Creates the database schema if it doesn't exist.
async fn create_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lyrics (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            album TEXT NOT NULL,
            duration REAL,
            format TEXT NOT NULL,
            raw_lyrics TEXT NOT NULL
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

    Ok(())
}

/// Opens or creates a SQLite database connection pool.
async fn open_database(path: &PathBuf) -> Result<SqlitePool, sqlx::Error> {
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
    match entry.format {
        LyricsFormat::Lrclib => {
            let lines = parse_synced_lyrics(&entry.raw_lyrics);
            Ok((lines, Some(entry.raw_lyrics.clone())))
        }
        LyricsFormat::Richsync => {
            // Parse the raw JSON body
            match parse_richsync_body(&entry.raw_lyrics) {
                Some(lines) => {
                    // Return the original JSON as raw
                    Ok((lines, Some(entry.raw_lyrics.clone())))
                }
                _ => Err(LyricsError::Api(
                    "Failed to parse richsync lyrics from database".to_string()
                )),
            }
        }
        LyricsFormat::Subtitles => {
            // Parse the raw JSON body
            match parse_subtitle_body(&entry.raw_lyrics) {
                Some(lines) => {
                    // Return the original JSON as raw
                    Ok((lines, Some(entry.raw_lyrics.clone())))
                }
                _ => Err(LyricsError::Api(
                    "Failed to parse subtitle lyrics from database".to_string()
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
) -> Option<ProviderResult> {
    let pool = DB_POOL.get()?;
    
    // Normalize search terms for case-insensitive matching
    let artist_norm = normalize(artist);
    let title_norm = normalize(title);
    let album_norm = normalize(album);
    
    // Query database with indexed lookup
    let row = sqlx::query(
        r#"
        SELECT duration, format, raw_lyrics
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
    .ok()??;
    
    // Extract fields from row
    let entry = LyricsEntry {
        duration: row.get("duration"),
        format: LyricsFormat::from_str(row.get("format"))?,
        raw_lyrics: row.get("raw_lyrics"),
    };
    
    // Optional: Validate duration match if both are present
    if let (Some(query_duration), Some(entry_duration)) = (duration, entry.duration) {
        // Allow 5% tolerance for duration mismatch
        let tolerance = query_duration * 0.05;
        if (query_duration - entry_duration).abs() > tolerance {
            return None;
        }
    }
    
    // Parse and return
    Some(parse_stored_lyrics(&entry))
}

/// Stores lyrics in the database.
///
/// Uses SQL DELETE + INSERT to replace existing entries.
/// Minimal memory usage - only the new entry is in memory briefly.
///
/// This should be called after successfully fetching lyrics from a provider.
pub async fn store_in_database(
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    format: LyricsFormat,
    raw_lyrics: String,
) {
    let Some(pool) = DB_POOL.get() else {
        return;
    };
    
    // Normalize for consistent storage
    let artist_norm = normalize(artist);
    let title_norm = normalize(title);
    let album_norm = normalize(album);
    
    // Delete existing entry if it exists
    let _ = sqlx::query(
        r#"
        DELETE FROM lyrics
        WHERE artist = ? AND title = ? AND album = ?
        "#,
    )
    .bind(&artist_norm)
    .bind(&title_norm)
    .bind(&album_norm)
    .execute(pool)
    .await;
    
    // Insert new entry
    let result = sqlx::query(
        r#"
        INSERT INTO lyrics (artist, title, album, duration, format, raw_lyrics)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&artist_norm)
    .bind(&title_norm)
    .bind(&album_norm)
    .bind(duration)
    .bind(format.to_str())
    .bind(&raw_lyrics)
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