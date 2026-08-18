//! CRUD operations and query logic for normalized lyrics caching.

use super::compress::{
    compress_raw_lyrics, decompress_raw_lyrics, normalize, normalize_isrc, normalize_track_id,
};
use super::schema::{DURATION_TOLERANCE_SECS, LyricsEntry, LyricsFormat};
use crate::lyrics::parse::{parse_richsync_body, parse_subtitle_body, parse_synced_lyrics};
use crate::lyrics::types::{LyricsError, ProviderResult, TrackMatchInfo};
use sqlx::Row;
use sqlx::sqlite::SqlitePool;

/// Parses stored lyrics based on their format.
///
/// # Returns
///
/// - `Ok((lines, Some(raw), track_ids))` on success with parsed lines, raw text, and identifiers
/// - `Err` if parsing fails
pub(crate) fn parse_stored_lyrics(entry: &LyricsEntry) -> ProviderResult {
    match entry.format {
        LyricsFormat::Lrclib => {
            let lines = parse_synced_lyrics(&entry.raw_lyrics);
            Ok((
                lines,
                Some(entry.raw_lyrics.clone()),
                entry.track_ids.clone(),
            ))
        }
        LyricsFormat::Richsync => match parse_richsync_body(&entry.raw_lyrics) {
            Some(lines) => Ok((
                lines,
                Some(entry.raw_lyrics.clone()),
                entry.track_ids.clone(),
            )),
            None => Err(LyricsError::Api(
                "Failed to parse richsync lyrics from database".to_string(),
            )),
        },
        LyricsFormat::Subtitles => match parse_subtitle_body(&entry.raw_lyrics) {
            Some(lines) => Ok((
                lines,
                Some(entry.raw_lyrics.clone()),
                entry.track_ids.clone(),
            )),
            None => Err(LyricsError::Api(
                "Failed to parse subtitle lyrics from database".to_string(),
            )),
        },
    }
}

/// Retrieves all associated track identifiers for a given track id in insertion order.
pub(crate) async fn fetch_track_ids(pool: &SqlitePool, track_id: i64) -> TrackMatchInfo {
    let mut isrcs = Vec::new();
    let mut spotify_ids = Vec::new();
    let mut itunes_ids = Vec::new();

    if let Ok(rows) = sqlx::query(
        "SELECT kind, value FROM track_identifiers WHERE track_id = ? ORDER BY ordering ASC",
    )
    .bind(track_id)
    .fetch_all(pool)
    .await
    {
        for row in rows {
            let kind: String = row.try_get("kind").unwrap_or_default();
            let value: String = row.try_get("value").unwrap_or_default();
            match kind.as_str() {
                "isrc" => isrcs.push(value),
                "spotify" => spotify_ids.push(value),
                "itunes" => itunes_ids.push(value),
                _ => {}
            }
        }
    }

    TrackMatchInfo {
        track_isrcs: isrcs,
        track_spotify_ids: spotify_ids,
        track_itunes_ids: itunes_ids,
    }
}

/// Inner implementation of fetching lyrics with an explicit database pool reference.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_from_database_inner(
    pool: &SqlitePool,
    artist: &str,
    title: &str,
    album: &str,
    duration: Option<f64>,
    isrc: Option<&str>,
    spotify_id: Option<&str>,
    itunes_id: Option<&str>,
) -> Option<ProviderResult> {
    // 1. Try lookup by ISRC (most reliable unique identifier)
    if let Some(id_norm) = isrc.and_then(normalize_isrc)
        && let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT l.id, l.artist, l.title, l.album, l.duration, l.format, l.raw_lyrics
            FROM lyrics l
            JOIN track_identifiers ti ON l.id = ti.track_id
            WHERE ti.kind = 'isrc' AND ti.value = ?
            LIMIT 1
            "#,
        )
        .bind(&id_norm)
        .fetch_optional(pool)
        .await
        && let Some(result) = process_db_row(&row, duration, pool).await
    {
        return Some(result);
    }

    // 2. Fallback: try lookup by composite key (artist / title / album)
    let artist_norm = normalize(artist);
    let title_norm = normalize(title);
    let album_norm = normalize(album);

    if let Ok(Some(row)) = sqlx::query(
        r#"
        SELECT id, artist, title, album, duration, format, raw_lyrics
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
        && let Some(result) = process_db_row(&row, duration, pool).await
    {
        return Some(result);
    }

    // 3. Fallback: try lookup by Spotify ID
    if let Some(id_norm) = spotify_id.and_then(normalize_track_id)
        && let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT l.id, l.artist, l.title, l.album, l.duration, l.format, l.raw_lyrics
            FROM lyrics l
            JOIN track_identifiers ti ON l.id = ti.track_id
            WHERE ti.kind = 'spotify' AND ti.value = ?
            LIMIT 1
            "#,
        )
        .bind(&id_norm)
        .fetch_optional(pool)
        .await
        && let Some(result) = process_db_row(&row, duration, pool).await
    {
        return Some(result);
    }

    // 4. Fallback: try lookup by iTunes ID
    if let Some(id_norm) = itunes_id.and_then(normalize_track_id)
        && let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT l.id, l.artist, l.title, l.album, l.duration, l.format, l.raw_lyrics
            FROM lyrics l
            JOIN track_identifiers ti ON l.id = ti.track_id
            WHERE ti.kind = 'itunes' AND ti.value = ?
            LIMIT 1
            "#,
        )
        .bind(&id_norm)
        .fetch_optional(pool)
        .await
        && let Some(result) = process_db_row(&row, duration, pool).await
    {
        return Some(result);
    }

    None
}

/// Helper: deletes a cached row by integer ID (cascades to track_identifiers via foreign key).
pub(crate) async fn delete_cached_row(pool: &SqlitePool, track_id: i64) {
    let _ = sqlx::query("DELETE FROM lyrics WHERE id = ?")
        .bind(track_id)
        .execute(pool)
        .await;
}

/// Processes a database row into a ProviderResult.
/// Handles decompression, format validation, duration matching, and identifier hydration.
pub(crate) async fn process_db_row(
    row: &sqlx::sqlite::SqliteRow,
    duration: Option<f64>,
    pool: &SqlitePool,
) -> Option<ProviderResult> {
    let id: i64 = match row.try_get("id") {
        Ok(id) => id,
        Err(_) => return None,
    };
    let artist: String = row.try_get("artist").unwrap_or_default();
    let title: String = row.try_get("title").unwrap_or_default();
    let album: String = row.try_get("album").unwrap_or_default();

    let raw_lyrics_blob: Vec<u8> = match row.try_get("raw_lyrics") {
        Ok(blob) => blob,
        Err(_) => {
            delete_cached_row(pool, id).await;
            return None;
        }
    };

    let raw_lyrics = match decompress_raw_lyrics(raw_lyrics_blob) {
        Some(text) => text,
        None => {
            tracing::warn!(
                artist = %artist,
                title = %title,
                "Failed to decompress cached lyrics blob; deleting cache entry"
            );
            delete_cached_row(pool, id).await;
            return None;
        }
    };

    let format_str: String = match row.try_get("format") {
        Ok(fmt) => fmt,
        Err(_) => {
            delete_cached_row(pool, id).await;
            return None;
        }
    };

    let format = match LyricsFormat::from_str(&format_str) {
        Some(fmt) => fmt,
        None => {
            tracing::warn!(
                artist = %artist,
                title = %title,
                "Invalid lyrics format in database; deleting cache entry"
            );
            delete_cached_row(pool, id).await;
            return None;
        }
    };

    let entry_duration: Option<f64> = row.try_get("duration").ok();

    // Validate duration match if both are present.
    if let (Some(query_duration), Some(entry_dur)) = (duration, entry_duration)
        && (query_duration - entry_dur).abs() > DURATION_TOLERANCE_SECS
    {
        delete_cached_row(pool, id).await;
        return None;
    }

    let track_ids = fetch_track_ids(pool, id).await;

    let entry = LyricsEntry {
        id,
        artist,
        title,
        album,
        duration: entry_duration,
        format,
        raw_lyrics,
        track_ids,
    };

    match parse_stored_lyrics(&entry) {
        Ok(ok) => Some(Ok(ok)),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to parse cached lyrics; deleting cache entry"
            );
            delete_cached_row(pool, id).await;
            None
        }
    }
}

/// Inner implementation of storing lyrics with an explicit database pool reference.
#[allow(clippy::too_many_arguments)]
pub async fn store_in_database_inner(
    pool: &SqlitePool,
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
    let artist_norm = normalize(artist);
    let title_norm = normalize(title);
    let album_norm = normalize(album);

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

    let mut normalized_isrcs = Vec::new();
    for isrc in isrcs {
        if let Some(norm) = normalize_isrc(isrc)
            && !normalized_isrcs.contains(&norm)
        {
            normalized_isrcs.push(norm);
        }
    }

    let mut normalized_spotify_ids = Vec::new();
    for sid in spotify_ids {
        if let Some(norm) = normalize_track_id(sid)
            && !normalized_spotify_ids.contains(&norm)
        {
            normalized_spotify_ids.push(norm);
        }
    }

    let mut normalized_itunes_ids = Vec::new();
    for tid in itunes_ids {
        if let Some(norm) = normalize_track_id(tid)
            && !normalized_itunes_ids.contains(&norm)
        {
            normalized_itunes_ids.push(norm);
        }
    }

    let Ok(mut tx) = pool.begin().await else {
        tracing::warn!(
            artist = %artist,
            title = %title,
            "Failed to begin database transaction for storing lyrics"
        );
        return;
    };

    let row: Result<(i64,), sqlx::Error> = sqlx::query_as(
        r#"
        INSERT INTO lyrics (artist, title, album, duration, format, raw_lyrics)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(artist, title, album) DO UPDATE SET
            duration = excluded.duration,
            format = excluded.format,
            raw_lyrics = excluded.raw_lyrics
        RETURNING id
        "#,
    )
    .bind(&artist_norm)
    .bind(&title_norm)
    .bind(&album_norm)
    .bind(duration)
    .bind(format.to_str())
    .bind(raw_lyrics_blob)
    .fetch_one(&mut *tx)
    .await;

    let track_id = match row {
        Ok((id,)) => id,
        Err(e) => {
            tracing::warn!(
                artist = %artist,
                title = %title,
                error = %e,
                "Failed to insert/update lyrics row in database"
            );
            return;
        }
    };

    // Delete existing ID mappings for this track and insert fresh ones
    let _ = sqlx::query("DELETE FROM track_identifiers WHERE track_id = ?")
        .bind(track_id)
        .execute(&mut *tx)
        .await;

    for (idx, isrc) in normalized_isrcs.iter().enumerate() {
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO track_identifiers (kind, value, track_id, ordering) VALUES ('isrc', ?, ?, ?)",
        )
        .bind(isrc)
        .bind(track_id)
        .bind(idx as i64)
        .execute(&mut *tx)
        .await;
    }

    for (idx, sid) in normalized_spotify_ids.iter().enumerate() {
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO track_identifiers (kind, value, track_id, ordering) VALUES ('spotify', ?, ?, ?)",
        )
        .bind(sid)
        .bind(track_id)
        .bind(idx as i64)
        .execute(&mut *tx)
        .await;
    }

    for (idx, tid) in normalized_itunes_ids.iter().enumerate() {
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO track_identifiers (kind, value, track_id, ordering) VALUES ('itunes', ?, ?, ?)",
        )
        .bind(tid)
        .bind(track_id)
        .bind(idx as i64)
        .execute(&mut *tx)
        .await;
    }

    if let Err(e) = tx.commit().await {
        tracing::warn!(
            artist = %artist,
            title = %title,
            error = %e,
            "Failed to commit database transaction for storing lyrics"
        );
    }
}
