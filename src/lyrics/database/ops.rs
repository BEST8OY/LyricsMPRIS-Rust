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

/// Retrieves all associated track identifiers for a given lyrics id in insertion order.
pub(crate) async fn fetch_track_ids(pool: &SqlitePool, lyrics_id: i64) -> TrackMatchInfo {
    let mut isrcs = Vec::new();
    let mut spotify_ids = Vec::new();
    let mut itunes_ids = Vec::new();

    if let Ok(rows) = sqlx::query(
        "SELECT kind, value FROM track_identifiers WHERE lyrics_id = ? ORDER BY ordering ASC",
    )
    .bind(lyrics_id)
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

/// Helper function to check if query duration matches entry duration within tolerance.
fn duration_matches(query_duration: Option<f64>, entry_duration: Option<f64>) -> bool {
    match (query_duration, entry_duration) {
        (Some(q), Some(e)) => (q - e).abs() <= DURATION_TOLERANCE_SECS,
        _ => true,
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
            SELECT l.id, l.artist, l.title, l.duration, l.format, l.raw_lyrics
            FROM lyrics l
            JOIN track_identifiers ti ON l.id = ti.lyrics_id
            WHERE ti.kind = 'isrc' AND ti.value = ?
            LIMIT 1
            "#,
        )
        .bind(&id_norm)
        .fetch_optional(pool)
        .await
    {
        let entry_dur: Option<f64> = row.try_get("duration").ok();
        if duration_matches(duration, entry_dur)
            && let Some(result) = process_db_row(&row, pool).await
        {
            return Some(result);
        }
    }

    // 2. Exact match on track_aliases (artist / title / album)
    let artist_norm = normalize(artist);
    let title_norm = normalize(title);
    let album_norm = normalize(album);

    if let Ok(Some(row)) = sqlx::query(
        r#"
        SELECT l.id, l.artist, l.title, l.duration, l.format, l.raw_lyrics, ta.album
        FROM lyrics l
        JOIN track_aliases ta ON l.id = ta.lyrics_id
        WHERE ta.artist = ? AND ta.title = ? AND ta.album = ?
        LIMIT 1
        "#,
    )
    .bind(&artist_norm)
    .bind(&title_norm)
    .bind(&album_norm)
    .fetch_optional(pool)
    .await
    {
        let entry_dur: Option<f64> = row.try_get("duration").ok();
        if duration_matches(duration, entry_dur)
            && let Some(result) = process_db_row(&row, pool).await
        {
            return Some(result);
        }
    }

    // 3. Fallback: try lookup by Spotify ID
    if let Some(id_norm) = spotify_id.and_then(normalize_track_id)
        && let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT l.id, l.artist, l.title, l.duration, l.format, l.raw_lyrics
            FROM lyrics l
            JOIN track_identifiers ti ON l.id = ti.lyrics_id
            WHERE ti.kind = 'spotify' AND ti.value = ?
            LIMIT 1
            "#,
        )
        .bind(&id_norm)
        .fetch_optional(pool)
        .await
    {
        let entry_dur: Option<f64> = row.try_get("duration").ok();
        if duration_matches(duration, entry_dur)
            && let Some(result) = process_db_row(&row, pool).await
        {
            return Some(result);
        }
    }

    // 4. Fallback: try lookup by iTunes ID
    if let Some(id_norm) = itunes_id.and_then(normalize_track_id)
        && let Ok(Some(row)) = sqlx::query(
            r#"
            SELECT l.id, l.artist, l.title, l.duration, l.format, l.raw_lyrics
            FROM lyrics l
            JOIN track_identifiers ti ON l.id = ti.lyrics_id
            WHERE ti.kind = 'itunes' AND ti.value = ?
            LIMIT 1
            "#,
        )
        .bind(&id_norm)
        .fetch_optional(pool)
        .await
    {
        let entry_dur: Option<f64> = row.try_get("duration").ok();
        if duration_matches(duration, entry_dur)
            && let Some(result) = process_db_row(&row, pool).await
        {
            return Some(result);
        }
    }

    // 5. Fallback: try lookup by (artist, title) across any album in track_aliases
    if !artist_norm.is_empty()
        && !title_norm.is_empty()
        && let Ok(rows) = sqlx::query(
            r#"
            SELECT DISTINCT l.id, l.artist, l.title, l.duration, l.format, l.raw_lyrics
            FROM lyrics l
            JOIN track_aliases ta ON l.id = ta.lyrics_id
            WHERE ta.artist = ? AND ta.title = ?
            LIMIT 5
            "#,
        )
        .bind(&artist_norm)
        .bind(&title_norm)
        .fetch_all(pool)
        .await
    {
        for row in rows {
            let entry_dur: Option<f64> = row.try_get("duration").ok();
            if duration_matches(duration, entry_dur)
                && let Some(result) = process_db_row(&row, pool).await
            {
                // Register this album alias so subsequent lookups hit step 2 immediately
                let target_id: i64 = row.try_get("id").unwrap_or_default();
                if target_id > 0 && !album_norm.is_empty() {
                    let _ = sqlx::query(
                        "INSERT OR IGNORE INTO track_aliases (artist, title, album, lyrics_id) VALUES (?, ?, ?, ?)",
                    )
                    .bind(&artist_norm)
                    .bind(&title_norm)
                    .bind(&album_norm)
                    .bind(target_id)
                    .execute(pool)
                    .await;
                }
                return Some(result);
            }
        }
    }

    None
}

/// Helper: deletes a cached row by integer ID (cascades to track_identifiers and track_aliases via foreign key).
pub(crate) async fn delete_cached_row(pool: &SqlitePool, lyrics_id: i64) {
    let _ = sqlx::query("DELETE FROM lyrics WHERE id = ?")
        .bind(lyrics_id)
        .execute(pool)
        .await;
}

/// Processes a database row into a ProviderResult.
/// Handles decompression, format validation, and identifier hydration.
pub(crate) async fn process_db_row(
    row: &sqlx::sqlite::SqliteRow,
    pool: &SqlitePool,
) -> Option<ProviderResult> {
    let id: i64 = match row.try_get("id") {
        Ok(id) => id,
        Err(_) => return None,
    };
    let artist: String = row.try_get("artist").unwrap_or_default();
    let title: String = row.try_get("title").unwrap_or_default();
    let album: Option<String> = row.try_get("album").ok();

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

    // Step 1: Find existing lyrics_id by authoritative identifier or alias + duration matching
    let mut matched_lyrics_id: Option<i64> = None;

    // 1a. Check ISRCs
    for isrc in &normalized_isrcs {
        if let Ok(Some((id,))) = sqlx::query_as::<_, (i64,)>(
            "SELECT lyrics_id FROM track_identifiers WHERE kind = 'isrc' AND value = ? LIMIT 1",
        )
        .bind(isrc)
        .fetch_optional(&mut *tx)
        .await
        {
            matched_lyrics_id = Some(id);
            break;
        }
    }

    // 1b. Check Spotify IDs
    if matched_lyrics_id.is_none() {
        for sid in &normalized_spotify_ids {
            if let Ok(Some((id,))) = sqlx::query_as::<_, (i64,)>(
                "SELECT lyrics_id FROM track_identifiers WHERE kind = 'spotify' AND value = ? LIMIT 1",
            )
            .bind(sid)
            .fetch_optional(&mut *tx)
            .await
            {
                matched_lyrics_id = Some(id);
                break;
            }
        }
    }

    // 1c. Check iTunes IDs
    if matched_lyrics_id.is_none() {
        for tid in &normalized_itunes_ids {
            if let Ok(Some((id,))) = sqlx::query_as::<_, (i64,)>(
                "SELECT lyrics_id FROM track_identifiers WHERE kind = 'itunes' AND value = ? LIMIT 1",
            )
            .bind(tid)
            .fetch_optional(&mut *tx)
            .await
            {
                matched_lyrics_id = Some(id);
                break;
            }
        }
    }

    // 1d. Check exact alias (artist, title, album)
    if matched_lyrics_id.is_none()
        && let Ok(Some((id,))) = sqlx::query_as::<_, (i64,)>(
            "SELECT lyrics_id FROM track_aliases WHERE artist = ? AND title = ? AND album = ? LIMIT 1",
        )
        .bind(&artist_norm)
        .bind(&title_norm)
        .bind(&album_norm)
        .fetch_optional(&mut *tx)
        .await
    {
        matched_lyrics_id = Some(id);
    }

    // 1e. Check (artist, title) across any album with duration tolerance
    if matched_lyrics_id.is_none()
        && !artist_norm.is_empty()
        && !title_norm.is_empty()
        && let Ok(rows) = sqlx::query(
            r#"
            SELECT DISTINCT l.id, l.duration
            FROM lyrics l
            JOIN track_aliases ta ON l.id = ta.lyrics_id
            WHERE ta.artist = ? AND ta.title = ?
            LIMIT 10
            "#,
        )
        .bind(&artist_norm)
        .bind(&title_norm)
        .fetch_all(&mut *tx)
        .await
    {
        for row in rows {
            let entry_dur: Option<f64> = row.try_get("duration").ok();
            if duration_matches(duration, entry_dur) {
                let id: i64 = row.try_get("id").unwrap_or_default();
                if id > 0 {
                    matched_lyrics_id = Some(id);
                    break;
                }
            }
        }
    }

    // Step 2: Update existing or insert new canonical lyrics row
    // NOTE (Future Enhancement): Consider format-priority protection when updating existing entries.
    // Higher-fidelity formats (e.g., `Richsync` with word-by-word timestamps) should not be downgraded
    // to line-by-line formats (`Lrclib` or `Subtitles`) if a line-level provider matches an existing recording.
    let target_lyrics_id = if let Some(existing_id) = matched_lyrics_id {
        let update_res = sqlx::query(
            r#"
            UPDATE lyrics
            SET duration = COALESCE(?, duration),
                format = ?,
                raw_lyrics = ?
            WHERE id = ?
            "#,
        )
        .bind(duration)
        .bind(format.to_str())
        .bind(raw_lyrics_blob)
        .bind(existing_id)
        .execute(&mut *tx)
        .await;

        if let Err(e) = update_res {
            tracing::warn!(
                artist = %artist,
                title = %title,
                error = %e,
                "Failed to update canonical lyrics in database"
            );
            return;
        }
        existing_id
    } else {
        let insert_res: Result<(i64,), sqlx::Error> = sqlx::query_as(
            r#"
            INSERT INTO lyrics (artist, title, duration, format, raw_lyrics)
            VALUES (?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(&artist_norm)
        .bind(&title_norm)
        .bind(duration)
        .bind(format.to_str())
        .bind(raw_lyrics_blob)
        .fetch_one(&mut *tx)
        .await;

        match insert_res {
            Ok((id,)) => id,
            Err(e) => {
                tracing::warn!(
                    artist = %artist,
                    title = %title,
                    error = %e,
                    "Failed to insert canonical lyrics row in database"
                );
                return;
            }
        }
    };

    // Step 3: Upsert track_aliases mapping (artist, title, album) -> target_lyrics_id
    let alias_res = sqlx::query(
        r#"
        INSERT INTO track_aliases (artist, title, album, lyrics_id)
        VALUES (?, ?, ?, ?)
        ON CONFLICT(artist, title, album) DO UPDATE SET
            lyrics_id = excluded.lyrics_id
        "#,
    )
    .bind(&artist_norm)
    .bind(&title_norm)
    .bind(&album_norm)
    .bind(target_lyrics_id)
    .execute(&mut *tx)
    .await;

    if let Err(e) = alias_res {
        tracing::warn!(
            artist = %artist,
            title = %title,
            error = %e,
            "Failed to upsert track alias in database"
        );
    }

    // Step 4: Upsert track_identifiers with PRIMARY KEY (kind, value)
    for (idx, isrc) in normalized_isrcs.iter().enumerate() {
        let _ = sqlx::query(
            r#"
            INSERT INTO track_identifiers (kind, value, lyrics_id, ordering)
            VALUES ('isrc', ?, ?, ?)
            ON CONFLICT(kind, value) DO UPDATE SET
                lyrics_id = excluded.lyrics_id,
                ordering = excluded.ordering
            "#,
        )
        .bind(isrc)
        .bind(target_lyrics_id)
        .bind(idx as i64)
        .execute(&mut *tx)
        .await;
    }

    for (idx, sid) in normalized_spotify_ids.iter().enumerate() {
        let _ = sqlx::query(
            r#"
            INSERT INTO track_identifiers (kind, value, lyrics_id, ordering)
            VALUES ('spotify', ?, ?, ?)
            ON CONFLICT(kind, value) DO UPDATE SET
                lyrics_id = excluded.lyrics_id,
                ordering = excluded.ordering
            "#,
        )
        .bind(sid)
        .bind(target_lyrics_id)
        .bind(idx as i64)
        .execute(&mut *tx)
        .await;
    }

    for (idx, tid) in normalized_itunes_ids.iter().enumerate() {
        let _ = sqlx::query(
            r#"
            INSERT INTO track_identifiers (kind, value, lyrics_id, ordering)
            VALUES ('itunes', ?, ?, ?)
            ON CONFLICT(kind, value) DO UPDATE SET
                lyrics_id = excluded.lyrics_id,
                ordering = excluded.ordering
            "#,
        )
        .bind(tid)
        .bind(target_lyrics_id)
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
