//! CRUD operations and query logic for lyrics caching.

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
/// - `Ok((lines, Some(raw)))` on success with parsed lines and original raw text
/// - `Err` if parsing fails
pub(crate) fn parse_stored_lyrics(entry: &LyricsEntry) -> ProviderResult {
    let ids = TrackMatchInfo {
        track_isrcs: entry.isrc.clone().into_iter().collect(),
        track_spotify_ids: entry.spotify_id.clone().into_iter().collect(),
        track_itunes_ids: entry.itunes_id.clone().into_iter().collect(),
    };
    match entry.format {
        LyricsFormat::Lrclib => {
            let lines = parse_synced_lyrics(&entry.raw_lyrics);
            Ok((lines, Some(entry.raw_lyrics.clone()), ids))
        }
        LyricsFormat::Richsync => match parse_richsync_body(&entry.raw_lyrics) {
            Some(lines) => Ok((lines, Some(entry.raw_lyrics.clone()), ids)),
            None => Err(LyricsError::Api(
                "Failed to parse richsync lyrics from database".to_string(),
            )),
        },
        LyricsFormat::Subtitles => match parse_subtitle_body(&entry.raw_lyrics) {
            Some(lines) => Ok((lines, Some(entry.raw_lyrics.clone()), ids)),
            None => Err(LyricsError::Api(
                "Failed to parse subtitle lyrics from database".to_string(),
            )),
        },
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
    // Normalize search terms for case-insensitive matching
    let artist_norm = normalize(artist);
    let title_norm = normalize(title);
    let album_norm = normalize(album);

    // Try lookup by ISRC first (most reliable unique identifier)
    if let Some(id_norm) = isrc.and_then(normalize_isrc) {
        if let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT l.duration, l.format, l.raw_lyrics, l.isrc, l.spotify_id, l.itunes_id
            FROM lyrics l
            JOIN track_isrcs ti ON l.artist = ti.artist AND l.title = ti.title AND l.album = ti.album
            WHERE ti.isrc = ?
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

        // Direct fallback on lyrics.isrc
        if let Ok(Some(row)) = sqlx::query(
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
    if let Some(id_norm) = spotify_id.and_then(normalize_track_id) {
        if let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT l.duration, l.format, l.raw_lyrics, l.isrc, l.spotify_id, l.itunes_id
            FROM lyrics l
            JOIN track_spotify_ids ts ON l.artist = ts.artist AND l.title = ts.title AND l.album = ts.album
            WHERE ts.spotify_id = ?
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

        // Direct fallback on lyrics.spotify_id
        if let Ok(Some(row)) = sqlx::query(
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
    }

    // Fallback: try lookup by iTunes ID
    if let Some(id_norm) = itunes_id.and_then(normalize_track_id) {
        if let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT l.duration, l.format, l.raw_lyrics, l.isrc, l.spotify_id, l.itunes_id
            FROM lyrics l
            JOIN track_itunes_ids tit ON l.artist = tit.artist AND l.title = tit.title AND l.album = tit.album
            WHERE tit.itunes_id = ?
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

        // Direct fallback on lyrics.itunes_id
        if let Ok(Some(row)) = sqlx::query(
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
    }

    None
}

/// Helper: deletes a cached row and its associated ISRC, Spotify, and iTunes mappings from the database.
pub(crate) async fn delete_cached_row(
    pool: &SqlitePool,
    artist_norm: &str,
    title_norm: &str,
    album_norm: &str,
) {
    let Ok(mut tx) = pool.begin().await else {
        return;
    };

    let _ = sqlx::query("DELETE FROM track_isrcs WHERE artist = ? AND title = ? AND album = ?")
        .bind(artist_norm)
        .bind(title_norm)
        .bind(album_norm)
        .execute(&mut *tx)
        .await;

    let _ =
        sqlx::query("DELETE FROM track_spotify_ids WHERE artist = ? AND title = ? AND album = ?")
            .bind(artist_norm)
            .bind(title_norm)
            .bind(album_norm)
            .execute(&mut *tx)
            .await;

    let _ =
        sqlx::query("DELETE FROM track_itunes_ids WHERE artist = ? AND title = ? AND album = ?")
            .bind(artist_norm)
            .bind(title_norm)
            .bind(album_norm)
            .execute(&mut *tx)
            .await;

    let _ = sqlx::query("DELETE FROM lyrics WHERE artist = ? AND title = ? AND album = ?")
        .bind(artist_norm)
        .bind(title_norm)
        .bind(album_norm)
        .execute(&mut *tx)
        .await;

    let _ = tx.commit().await;
}

/// Processes a database row into a ProviderResult.
/// Handles decompression, format validation, and duration matching.
pub(crate) async fn process_db_row(
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
            delete_cached_row(pool, artist_norm, title_norm, album_norm).await;
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
            delete_cached_row(pool, artist_norm, title_norm, album_norm).await;
            return None;
        }
    };

    let format_str: String = match row.try_get("format") {
        Ok(fmt) => fmt,
        Err(_) => {
            delete_cached_row(pool, artist_norm, title_norm, album_norm).await;
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
            delete_cached_row(pool, artist_norm, title_norm, album_norm).await;
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

    // Validate duration match if both are present.
    if let (Some(query_duration), Some(entry_duration)) = (duration, entry.duration)
        && (query_duration - entry_duration).abs() > DURATION_TOLERANCE_SECS
    {
        delete_cached_row(pool, artist_norm, title_norm, album_norm).await;
        return None;
    }

    match parse_stored_lyrics(&entry) {
        Ok(ok) => Some(Ok(ok)),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to parse cached lyrics; deleting cache entry"
            );
            delete_cached_row(pool, artist_norm, title_norm, album_norm).await;
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

    let mut normalized_isrcs = Vec::new();
    for isrc in isrcs {
        if let Some(norm) = normalize_isrc(isrc)
            && !normalized_isrcs.contains(&norm)
        {
            normalized_isrcs.push(norm);
        }
    }
    let primary_isrc = normalized_isrcs.first().cloned();

    let mut normalized_spotify_ids = Vec::new();
    for sid in spotify_ids {
        if let Some(norm) = normalize_track_id(sid)
            && !normalized_spotify_ids.contains(&norm)
        {
            normalized_spotify_ids.push(norm);
        }
    }
    let primary_spotify = normalized_spotify_ids.first().cloned();

    let mut normalized_itunes_ids = Vec::new();
    for tid in itunes_ids {
        if let Some(norm) = normalize_track_id(tid)
            && !normalized_itunes_ids.contains(&norm)
        {
            normalized_itunes_ids.push(norm);
        }
    }
    let primary_itunes = normalized_itunes_ids.first().cloned();

    let Ok(mut tx) = pool.begin().await else {
        tracing::warn!(
            artist = %artist,
            title = %title,
            "Failed to begin database transaction for storing lyrics"
        );
        return;
    };

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
    .bind(primary_isrc.as_deref())
    .bind(primary_spotify.as_deref())
    .bind(primary_itunes.as_deref())
    .execute(&mut *tx)
    .await;

    if let Err(e) = result {
        tracing::warn!(
            artist = %artist,
            title = %title,
            error = %e,
            "Failed to store lyrics in database"
        );
        return;
    }

    // Delete existing ID associations for this track and insert updated ones
    let _ = sqlx::query("DELETE FROM track_isrcs WHERE artist = ? AND title = ? AND album = ?")
        .bind(&artist_norm)
        .bind(&title_norm)
        .bind(&album_norm)
        .execute(&mut *tx)
        .await;

    for isrc in &normalized_isrcs {
        let _ = sqlx::query(
            r#"
            INSERT OR IGNORE INTO track_isrcs (artist, title, album, isrc)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(&artist_norm)
        .bind(&title_norm)
        .bind(&album_norm)
        .bind(isrc)
        .execute(&mut *tx)
        .await;
    }

    let _ =
        sqlx::query("DELETE FROM track_spotify_ids WHERE artist = ? AND title = ? AND album = ?")
            .bind(&artist_norm)
            .bind(&title_norm)
            .bind(&album_norm)
            .execute(&mut *tx)
            .await;

    for sid in &normalized_spotify_ids {
        let _ = sqlx::query(
            r#"
            INSERT OR IGNORE INTO track_spotify_ids (artist, title, album, spotify_id)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(&artist_norm)
        .bind(&title_norm)
        .bind(&album_norm)
        .bind(sid)
        .execute(&mut *tx)
        .await;
    }

    let _ =
        sqlx::query("DELETE FROM track_itunes_ids WHERE artist = ? AND title = ? AND album = ?")
            .bind(&artist_norm)
            .bind(&title_norm)
            .bind(&album_norm)
            .execute(&mut *tx)
            .await;

    for tid in &normalized_itunes_ids {
        let _ = sqlx::query(
            r#"
            INSERT OR IGNORE INTO track_itunes_ids (artist, title, album, itunes_id)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(&artist_norm)
        .bind(&title_norm)
        .bind(&album_norm)
        .bind(tid)
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
