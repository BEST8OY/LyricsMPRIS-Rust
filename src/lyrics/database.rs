//! Local lyrics database module.
//!
//! This module provides a persistent JSON-based cache for lyrics to reduce
//! API calls and enable offline playback. It stores raw lyrics data and
//! parses it on retrieval.
//!
//! # Storage Format
//!
//! - **LRC format** (from LRCLIB): Stored as raw text with `[MM:SS.CC]` timestamps
//! - **Richsync** (from Musixmatch): Stored as unparsed JSON (word-level timing)
//! - **Subtitles** (from Musixmatch): Stored as unparsed JSON (line-level timing)
//! - **Metadata**: Artist, title, album for efficient lookups
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
//! │ Database Check  │───── Hit ──────▶ Parse & Return
//! └────────┬────────┘
//!          │ Miss
//!          ▼
//! ┌─────────────────┐
//! │ Provider Fetch  │
//! └────────┬────────┘
//!          │
//!          ▼
//! ┌─────────────────┐
//! │ Store in DB     │
//! └─────────────────┘
//! ```

use crate::lyrics::parse::{parse_richsync_body, parse_subtitle_body, parse_synced_lyrics};
use crate::lyrics::types::{LyricsError, ProviderResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ============================================================================
// Database Types
// ============================================================================

/// Format of stored lyrics for correct parsing on retrieval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LyricsFormat {
    /// LRC timestamp format (from LRCLIB provider): `[MM:SS.CC]lyrics`
    Lrclib,
    /// Musixmatch richsync format with word-level timestamps (JSON)
    Richsync,
    /// Musixmatch subtitle format with line-level timestamps (JSON)
    Subtitles,
}

/// Database entry for a single track's lyrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LyricsEntry {
    /// Track artist (normalized for matching)
    pub artist: String,
    
    /// Track title (normalized for matching)
    pub title: String,
    
    /// Track album (normalized for matching)
    pub album: String,
    
    /// Track duration in seconds (optional, for better matching)
    pub duration: Option<f64>,
    
    /// Format of the stored lyrics
    pub format: LyricsFormat,
    
    /// Raw lyrics text (unparsed for richsync, LRCLIB text otherwise)
    pub raw_lyrics: String,
}

/// In-memory database structure.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LyricsDatabase {
    /// Map of normalized key -> lyrics entry
    entries: HashMap<String, LyricsEntry>,
}

impl LyricsDatabase {
    /// Creates a new empty database.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Normalizes a string for case-insensitive matching.
    ///
    /// Converts to lowercase and trims whitespace.
    fn normalize(s: &str) -> String {
        s.trim().to_lowercase()
    }

    /// Generates a cache key from track metadata.
    ///
    /// Format: `artist|title|album` (all normalized)
    fn cache_key(artist: &str, title: &str, album: &str) -> String {
        format!(
            "{}|{}|{}",
            Self::normalize(artist),
            Self::normalize(title),
            Self::normalize(album)
        )
    }

    /// Looks up lyrics in the database.
    ///
    /// Returns `Some(entry)` if found, `None` otherwise.
    pub fn get(&self, artist: &str, title: &str, album: &str) -> Option<&LyricsEntry> {
        let key = Self::cache_key(artist, title, album);
        self.entries.get(&key)
    }

    /// Stores lyrics in the database.
    ///
    /// Overwrites existing entries with the same key.
    pub fn insert(
        &mut self,
        artist: &str,
        title: &str,
        album: &str,
        duration: Option<f64>,
        format: LyricsFormat,
        raw_lyrics: String,
    ) {
        let key = Self::cache_key(artist, title, album);
        let entry = LyricsEntry {
            artist: artist.to_string(),
            title: title.to_string(),
            album: album.to_string(),
            duration,
            format,
            raw_lyrics,
        };
        self.entries.insert(key, entry);
    }

    /// Returns the number of entries in the database.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

// ============================================================================
// File I/O
// ============================================================================

/// Loads the database from a JSON file.
///
/// Returns a new empty database if the file doesn't exist or is invalid.
pub async fn load_database(path: &Path) -> LyricsDatabase {
    match load_database_inner(path).await {
        Ok(db) => {
            if db.len() > 0 {
                tracing::info!(
                    path = %path.display(),
                    entries = db.len(),
                    "Loaded lyrics database"
                );
            }
            db
        }
        Err(e) if e.to_string().contains("No such file") => {
            // First run - file doesn't exist yet
            tracing::info!(
                path = %path.display(),
                "Creating new lyrics database"
            );
            LyricsDatabase::new()
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to load database, using empty database"
            );
            LyricsDatabase::new()
        }
    }
}

/// Inner implementation that returns errors for logging.
async fn load_database_inner(path: &Path) -> Result<LyricsDatabase, Box<dyn std::error::Error>> {
    let mut file = fs::File::open(path).await?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    let db: LyricsDatabase = serde_json::from_str(&contents)?;
    Ok(db)
}

/// Saves the database to a JSON file.
///
/// Creates parent directories if they don't exist.
pub async fn save_database(db: &LyricsDatabase, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Create parent directory if it doesn't exist
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let json = serde_json::to_string_pretty(db)?;
    let mut file = fs::File::create(path).await?;
    file.write_all(json.as_bytes()).await?;
    file.flush().await?;
    Ok(())
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
                None => Err(LyricsError::Api(
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
                None => Err(LyricsError::Api(
                    "Failed to parse subtitle lyrics from database".to_string()
                )),
            }
        }
    }
}

// ============================================================================
// Public API
// ============================================================================

/// Global database state wrapped in a mutex for thread-safe access.
static DATABASE: tokio::sync::Mutex<Option<(LyricsDatabase, PathBuf)>> = tokio::sync::Mutex::const_new(None);

/// Initializes the database from the specified path.
///
/// This should be called once at application startup.
/// Logging is handled by `load_database`.
pub async fn initialize(path: PathBuf) {
    let db = load_database(&path).await;
    *DATABASE.lock().await = Some((db, path));
}

/// Attempts to fetch lyrics from the database.
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
    let guard = DATABASE.lock().await;
    let (db, _path) = guard.as_ref()?;
    
    let entry = db.get(artist, title, album)?;
    
    // Optional: Validate duration match if both are present
    if let (Some(query_duration), Some(entry_duration)) = (duration, entry.duration) {
        // Allow 5% tolerance for duration mismatch
        let tolerance = query_duration * 0.05;
        if (query_duration - entry_duration).abs() > tolerance {
            return None;
        }
    }
    
    // Parse and return
    Some(parse_stored_lyrics(entry))
}

/// Stores lyrics in the database and persists to disk.
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
    let mut guard = DATABASE.lock().await;
    let Some((db, path)) = guard.as_mut() else {
        return;
    };
    
    db.insert(artist, title, album, duration, format, raw_lyrics);
    
    // Persist to disk asynchronously (don't block on errors)
    if let Err(e) = save_database(db, path).await {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "Failed to save database"
        );
    }
}
