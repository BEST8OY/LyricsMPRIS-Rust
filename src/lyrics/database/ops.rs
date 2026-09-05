//! CRUD operations and query logic for normalized lyrics caching.

use super::compress::{
    compress_raw_lyrics, decompress_raw_lyrics, normalize, normalize_isrc, normalize_track_id,
    strip_remaster_suffix,
};
use super::schema::{DURATION_TOLERANCE_SECS, LyricsFormat};
use crate::lyrics::parse::{parse_richsync_body, parse_subtitle_body, parse_synced_lyrics};
use crate::lyrics::types::{LyricsError, ProviderResult, TrackMatchInfo};
use sqlx::Connection;
use sqlx::Row;
use sqlx::sqlite::SqlitePool;

/// Parses stored lyrics based on their format with zero string copies.
///
/// # Returns
///
/// - `Ok((lines, Some(raw), track_ids))` on success with parsed lines, raw text, and identifiers
/// - `Err` if parsing fails
pub(crate) fn parse_stored_lyrics(
    format: LyricsFormat,
    raw_lyrics: String,
    track_ids: TrackMatchInfo,
) -> ProviderResult {
    let lines = match format {
        LyricsFormat::Lrclib => Ok(parse_synced_lyrics(&raw_lyrics)),
        LyricsFormat::Richsync => parse_richsync_body(&raw_lyrics).ok_or_else(|| {
            LyricsError::Api("Failed to parse richsync lyrics from database".to_string())
        }),
        LyricsFormat::Subtitles => parse_subtitle_body(&raw_lyrics).ok_or_else(|| {
            LyricsError::Api("Failed to parse subtitle lyrics from database".to_string())
        }),
    }?;

    let mut parsed_lines = lines;
    crate::lyrics::romanize::apply_romanization(&mut parsed_lines);
    Ok((parsed_lines, Some(raw_lyrics), track_ids))
}

/// Retrieves all associated track identifiers for a given lyrics id in insertion order.
pub(crate) async fn fetch_track_ids(pool: &SqlitePool, lyrics_id: i64) -> TrackMatchInfo {
    let mut isrcs = Vec::new();
    let mut spotify_ids = Vec::new();
    let mut itunes_ids = Vec::new();

    if let Ok(rows) = sqlx::query_as::<_, (String, String)>(
        "SELECT kind, value FROM track_identifiers WHERE lyrics_id = ? ORDER BY ordering ASC",
    )
    .bind(lyrics_id)
    .fetch_all(pool)
    .await
    {
        for (kind, value) in rows {
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
        (Some(q), Some(e)) if q > 0.0 && e > 0.0 && q.is_finite() && e.is_finite() => {
            (q - e).abs() <= DURATION_TOLERANCE_SECS
        }
        _ => true,
    }
}

/// Helper: lookup a single candidate row by identifier (ISRC, Spotify, iTunes) with duration tolerance.
async fn lookup_by_identifier(
    pool: &SqlitePool,
    kind: &str,
    id_norm: &str,
    duration: Option<f64>,
) -> Option<(ProviderResult, LyricsFormat)> {
    if let Ok(Some(row)) = sqlx::query(
        r#"
        SELECT l.id, l.artist, l.title, l.duration, l.format, l.raw_lyrics
        FROM lyrics l
        JOIN track_identifiers ti ON l.id = ti.lyrics_id
        WHERE ti.kind = ? AND ti.value = ?
        LIMIT 1
        "#,
    )
    .bind(kind)
    .bind(id_norm)
    .fetch_optional(pool)
    .await
    {
        let entry_dur: Option<f64> = row.try_get("duration").ok();
        if duration_matches(duration, entry_dur) {
            return process_db_row(&row, pool).await;
        }
    }
    None
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
) -> Option<(ProviderResult, LyricsFormat)> {
    // 1. Try lookup by ISRC (most reliable unique identifier)
    if let Some(id_norm) = isrc.and_then(normalize_isrc)
        && let Some(result) = lookup_by_identifier(pool, "isrc", &id_norm, duration).await
    {
        return Some(result);
    }

    // 2. Exact match on track_aliases (artist / title / album)
    let artist_norm = normalize(artist);
    let title_norm = normalize(title);
    let album_norm = normalize(album);

    if let Ok(Some(row)) = sqlx::query(
        r#"
        SELECT l.id, l.artist, l.title, l.duration, l.format, l.raw_lyrics
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
        && let Some(result) = lookup_by_identifier(pool, "spotify", &id_norm, duration).await
    {
        return Some(result);
    }

    // 4. Fallback: try lookup by iTunes ID
    if let Some(id_norm) = itunes_id.and_then(normalize_track_id)
        && let Some(result) = lookup_by_identifier(pool, "itunes", &id_norm, duration).await
    {
        return Some(result);
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
                // Register/update this album alias so subsequent lookups hit step 2 immediately
                let target_id: i64 = row.try_get("id").unwrap_or_default();
                if target_id > 0 && !album_norm.is_empty() {
                    let _ = sqlx::query(
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
                    .bind(target_id)
                    .execute(pool)
                    .await;
                }
                return Some(result);
            }
        }
    }

    // 6. Fallback: Remaster annotation tolerance
    if !artist_norm.is_empty() && !title_norm.is_empty() {
        let clean_query_title = strip_remaster_suffix(&title_norm);

        // Case A: Query has remaster suffix (e.g. "Heroes - 2015 Remaster") -> look for clean title in DB
        if let Some(clean_title) = clean_query_title.as_ref() {
            // 6a. Exact match on clean title + exact album
            if let Ok(Some(row)) = sqlx::query(
                r#"
                SELECT l.id, l.artist, l.title, l.duration, l.format, l.raw_lyrics
                FROM lyrics l
                JOIN track_aliases ta ON l.id = ta.lyrics_id
                WHERE ta.artist = ? AND ta.title = ? AND ta.album = ?
                LIMIT 1
                "#,
            )
            .bind(&artist_norm)
            .bind(clean_title)
            .bind(&album_norm)
            .fetch_optional(pool)
            .await
            {
                let entry_dur: Option<f64> = row.try_get("duration").ok();
                if duration_matches(duration, entry_dur)
                    && let Some(result) = process_db_row(&row, pool).await
                {
                    // Self-healing: register the player's exact remaster query alias
                    let target_id: i64 = row.try_get("id").unwrap_or_default();
                    if target_id > 0 {
                        let _ = sqlx::query(
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
                        .bind(target_id)
                        .execute(pool)
                        .await;
                    }
                    return Some(result);
                }
            }

            // 6b. Clean title across any album
            if let Ok(rows) = sqlx::query(
                r#"
                SELECT DISTINCT l.id, l.artist, l.title, l.duration, l.format, l.raw_lyrics
                FROM lyrics l
                JOIN track_aliases ta ON l.id = ta.lyrics_id
                WHERE ta.artist = ? AND ta.title = ?
                LIMIT 5
                "#,
            )
            .bind(&artist_norm)
            .bind(clean_title)
            .fetch_all(pool)
            .await
            {
                for row in rows {
                    let entry_dur: Option<f64> = row.try_get("duration").ok();
                    if duration_matches(duration, entry_dur)
                        && let Some(result) = process_db_row(&row, pool).await
                    {
                        // Self-healing: register the player's exact remaster query alias
                        let target_id: i64 = row.try_get("id").unwrap_or_default();
                        if target_id > 0 {
                            let _ = sqlx::query(
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
                            .bind(target_id)
                            .execute(pool)
                            .await;
                        }
                        return Some(result);
                    }
                }
            }
        }

        // Case B: DB has remaster suffix (or alternate remaster), while query is clean (or has a different remaster)
        let clean_target = clean_query_title.as_deref().unwrap_or(&title_norm);
        let escaped_clean = clean_target
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let prefix_pattern = format!("{escaped_clean}%");

        if let Ok(rows) = sqlx::query(
            r#"
            SELECT DISTINCT ta.title AS alias_title, l.id, l.artist, l.title, l.duration, l.format, l.raw_lyrics
            FROM lyrics l
            JOIN track_aliases ta ON l.id = ta.lyrics_id
            WHERE ta.artist = ? AND ta.title LIKE ? ESCAPE '\'
            LIMIT 10
            "#,
        )
        .bind(&artist_norm)
        .bind(&prefix_pattern)
        .fetch_all(pool)
        .await
        {
            for row in rows {
                let alias_title: String = row.try_get("alias_title").unwrap_or_default();
                if strip_remaster_suffix(&alias_title).as_deref() == Some(clean_target) {
                    let entry_dur: Option<f64> = row.try_get("duration").ok();
                    if duration_matches(duration, entry_dur)
                        && let Some(result) = process_db_row(&row, pool).await
                    {
                        // Self-healing: register query alias so subsequent lookups hit step 2 immediately
                        let target_id: i64 = row.try_get("id").unwrap_or_default();
                        if target_id > 0 {
                            let _ = sqlx::query(
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
                            .bind(target_id)
                            .execute(pool)
                            .await;
                        }
                        return Some(result);
                    }
                }
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

/// Processes a database row into a ProviderResult and its LyricsFormat.
/// Handles decompression, format validation, and identifier hydration.
pub(crate) async fn process_db_row(
    row: &sqlx::sqlite::SqliteRow,
    pool: &SqlitePool,
) -> Option<(ProviderResult, LyricsFormat)> {
    let id: i64 = match row.try_get("id") {
        Ok(id) => id,
        Err(_) => return None,
    };
    let artist: String = row.try_get("artist").unwrap_or_default();
    let title: String = row.try_get("title").unwrap_or_default();

    let raw_lyrics_blob: Vec<u8> = match row.try_get("raw_lyrics") {
        Ok(blob) => blob,
        Err(_) => {
            delete_cached_row(pool, id).await;
            return None;
        }
    };

    let raw_lyrics = match decompress_raw_lyrics(&raw_lyrics_blob) {
        Some(text) if !text.trim().is_empty() => text,
        _ => {
            tracing::warn!(
                artist = %artist,
                title = %title,
                "Failed to decompress cached lyrics blob or blob is empty; deleting cache entry"
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
                format = %format_str,
                "Unknown lyrics format in database; skipping cache entry"
            );
            return None;
        }
    };

    let track_ids = fetch_track_ids(pool, id).await;

    match parse_stored_lyrics(format, raw_lyrics, track_ids) {
        Ok((lines, raw, ids)) if !lines.is_empty() => Some((Ok((lines, raw, ids)), format)),
        Ok(_) => {
            tracing::warn!(
                artist = %artist,
                title = %title,
                "Cached lyrics parsed into empty lines; deleting cache entry"
            );
            delete_cached_row(pool, id).await;
            None
        }
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

/// Helper: find existing lyrics id and format for matching identifier in active transaction.
async fn find_matched_identifier_lyrics(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    kind: &str,
    values: &[String],
    duration: Option<f64>,
) -> Option<(i64, Option<LyricsFormat>)> {
    for val in values {
        if let Ok(Some((id, entry_dur, fmt_str))) = sqlx::query_as::<_, (i64, Option<f64>, String)>(
            r#"
                SELECT ti.lyrics_id, l.duration, l.format
                FROM track_identifiers ti
                JOIN lyrics l ON l.id = ti.lyrics_id
                WHERE ti.kind = ? AND ti.value = ?
                LIMIT 1
                "#,
        )
        .bind(kind)
        .bind(val)
        .fetch_optional(&mut **tx)
        .await
            && duration_matches(duration, entry_dur)
        {
            let parsed_fmt = LyricsFormat::from_str(&fmt_str);
            return Some((id, parsed_fmt));
        }
    }
    None
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
    if raw_lyrics.trim().is_empty() {
        tracing::debug!(
            artist = %artist,
            title = %title,
            "Skipping database cache for empty lyrics payload"
        );
        return;
    }

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

    let Ok(mut conn) = pool.acquire().await else {
        tracing::warn!(
            artist = %artist,
            title = %title,
            "Failed to acquire database connection for storing lyrics"
        );
        return;
    };

    let Ok(mut tx) = conn.begin_with("BEGIN IMMEDIATE").await else {
        tracing::warn!(
            artist = %artist,
            title = %title,
            "Failed to begin database transaction for storing lyrics"
        );
        return;
    };

    // Step 1: Find existing lyrics_id by authoritative identifier or alias + duration matching
    let mut matched_entry: Option<(i64, Option<LyricsFormat>)> =
        find_matched_identifier_lyrics(&mut tx, "isrc", &normalized_isrcs, duration).await;

    if matched_entry.is_none() {
        matched_entry =
            find_matched_identifier_lyrics(&mut tx, "spotify", &normalized_spotify_ids, duration)
                .await;
    }

    if matched_entry.is_none() {
        matched_entry =
            find_matched_identifier_lyrics(&mut tx, "itunes", &normalized_itunes_ids, duration)
                .await;
    }

    // 1d. Check exact alias (artist, title, album)
    if matched_entry.is_none()
        && let Ok(Some((id, entry_dur, fmt_str))) = sqlx::query_as::<_, (i64, Option<f64>, String)>(
            r#"
            SELECT ta.lyrics_id, l.duration, l.format
            FROM track_aliases ta
            JOIN lyrics l ON l.id = ta.lyrics_id
            WHERE ta.artist = ? AND ta.title = ? AND ta.album = ?
            LIMIT 1
            "#,
        )
        .bind(&artist_norm)
        .bind(&title_norm)
        .bind(&album_norm)
        .fetch_optional(&mut *tx)
        .await
        && duration_matches(duration, entry_dur)
    {
        matched_entry = Some((id, LyricsFormat::from_str(&fmt_str)));
    }

    // 1e. Check (artist, title) across any album with duration tolerance
    if matched_entry.is_none()
        && !artist_norm.is_empty()
        && !title_norm.is_empty()
        && let Ok(rows) = sqlx::query(
            r#"
            SELECT DISTINCT l.id, l.duration, l.format
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
                    let fmt_str: String = row.try_get("format").unwrap_or_default();
                    matched_entry = Some((id, LyricsFormat::from_str(&fmt_str)));
                    break;
                }
            }
        }
    }

    // 1f. Check clean title or remaster alias in track_aliases with duration tolerance
    if matched_entry.is_none() && !artist_norm.is_empty() && !title_norm.is_empty() {
        let clean_query_title = strip_remaster_suffix(&title_norm);

        // If incoming title has remaster suffix, check if clean title already exists
        if let Some(clean_title) = clean_query_title.as_ref()
            && let Ok(rows) = sqlx::query(
                r#"
                SELECT DISTINCT l.id, l.duration, l.format
                FROM lyrics l
                JOIN track_aliases ta ON l.id = ta.lyrics_id
                WHERE ta.artist = ? AND ta.title = ?
                LIMIT 5
                "#,
            )
            .bind(&artist_norm)
            .bind(clean_title)
            .fetch_all(&mut *tx)
            .await
        {
            for row in rows {
                let entry_dur: Option<f64> = row.try_get("duration").ok();
                if duration_matches(duration, entry_dur) {
                    let id: i64 = row.try_get("id").unwrap_or_default();
                    if id > 0 {
                        let fmt_str: String = row.try_get("format").unwrap_or_default();
                        matched_entry = Some((id, LyricsFormat::from_str(&fmt_str)));
                        break;
                    }
                }
            }
        }

        // If incoming title is clean or alternate remaster, check existing remaster aliases
        if matched_entry.is_none() {
            let clean_target = clean_query_title.as_deref().unwrap_or(&title_norm);
            let escaped_clean = clean_target
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let pattern = format!("{escaped_clean}%");

            if let Ok(rows) = sqlx::query(
                r#"
                SELECT DISTINCT ta.title AS alias_title, l.id, l.duration, l.format
                FROM lyrics l
                JOIN track_aliases ta ON l.id = ta.lyrics_id
                WHERE ta.artist = ? AND ta.title LIKE ? ESCAPE '\'
                LIMIT 10
                "#,
            )
            .bind(&artist_norm)
            .bind(&pattern)
            .fetch_all(&mut *tx)
            .await
            {
                for row in rows {
                    let alias_title: String = row.try_get("alias_title").unwrap_or_default();
                    if strip_remaster_suffix(&alias_title).as_deref() == Some(clean_target) {
                        let entry_dur: Option<f64> = row.try_get("duration").ok();
                        if duration_matches(duration, entry_dur) {
                            let id: i64 = row.try_get("id").unwrap_or_default();
                            if id > 0 {
                                let fmt_str: String = row.try_get("format").unwrap_or_default();
                                matched_entry = Some((id, LyricsFormat::from_str(&fmt_str)));
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    // Step 2: Update existing or insert new canonical lyrics row
    let target_lyrics_id = if let Some((existing_id, existing_fmt)) = matched_entry {
        if let Some(existing_fmt) = existing_fmt
            && format.fidelity_rank() < existing_fmt.fidelity_rank()
        {
            tracing::debug!(
                existing_id,
                existing_format = ?existing_fmt,
                incoming_format = ?format,
                "Skipping canonical lyrics update: existing format has higher fidelity"
            );
        } else {
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

    if let Some(clean_title) = strip_remaster_suffix(&title_norm) {
        let clean_alias_res = sqlx::query(
            r#"
            INSERT INTO track_aliases (artist, title, album, lyrics_id)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(artist, title, album) DO UPDATE SET
                lyrics_id = excluded.lyrics_id
            "#,
        )
        .bind(&artist_norm)
        .bind(&clean_title)
        .bind(&album_norm)
        .bind(target_lyrics_id)
        .execute(&mut *tx)
        .await;

        if let Err(e) = clean_alias_res {
            tracing::warn!(
                artist = %artist,
                title = %clean_title,
                error = %e,
                "Failed to upsert clean track alias in database"
            );
        }
    }

    // Step 4: Upsert track_identifiers with PRIMARY KEY (kind, value)
    for (kind, ids) in [
        ("isrc", &normalized_isrcs),
        ("spotify", &normalized_spotify_ids),
        ("itunes", &normalized_itunes_ids),
    ] {
        for (idx, val) in ids.iter().enumerate() {
            let res = sqlx::query(
                r#"
                INSERT INTO track_identifiers (kind, value, lyrics_id, ordering)
                VALUES (?, ?, ?, ?)
                ON CONFLICT(kind, value) DO UPDATE SET
                    lyrics_id = excluded.lyrics_id,
                    ordering = excluded.ordering
                "#,
            )
            .bind(kind)
            .bind(val)
            .bind(target_lyrics_id)
            .bind(idx as i64)
            .execute(&mut *tx)
            .await;

            if let Err(e) = res {
                tracing::warn!(
                    kind,
                    value = %val,
                    error = %e,
                    "Failed to upsert track identifier in database"
                );
            }
        }
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
