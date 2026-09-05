//! Integration and unit tests for database CRUD operations, alias deduplication, and identifier indexing.

use super::compress::{normalize, strip_remaster_suffix};
use super::ops::{delete_cached_row, fetch_from_database_inner, store_in_database_inner};
use super::schema::{LyricsFormat, create_schema};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

pub(crate) async fn create_test_pool() -> SqlitePool {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    create_schema(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn test_database_crud_and_lookups() {
    let pool = create_test_pool().await;

    let lrc_content = "[00:10.00]Test lyric line 1\n[00:20.00]Test lyric line 2";
    let isrcs = vec!["USUM71234567".to_string(), "ALTISRC99999".to_string()];
    let spotify_ids = vec![
        "5FVd6KXrgO9B3JPmC8OPst".to_string(),
        "2UzMpPKPhbcC8RbsmuURAZ".to_string(),
    ];
    let itunes_ids = vec!["123456789".to_string(), "987654321".to_string()];

    // 1. Store via inner API using test pool
    store_in_database_inner(
        &pool,
        "test artist",
        "test title",
        "test album",
        Some(180.0),
        LyricsFormat::Lrclib,
        lrc_content.to_string(),
        &isrcs,
        &spotify_ids,
        &itunes_ids,
    )
    .await;

    // 2. Fetch by composite key (artist / title / album)
    let res_key = fetch_from_database_inner(
        &pool,
        "test artist",
        "test title",
        "test album",
        Some(180.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_key.is_some());
    let (res_prov, format) = res_key.unwrap();
    let (lines, raw, ids) = res_prov.unwrap();
    assert_eq!(format, LyricsFormat::Lrclib);
    assert_eq!(lines.len(), 2);
    assert_eq!(raw, Some(lrc_content.to_string()));
    assert_eq!(ids.track_isrcs, isrcs);
    assert_eq!(ids.track_spotify_ids, spotify_ids);
    assert_eq!(ids.track_itunes_ids, itunes_ids);

    // 3. Fetch by primary ISRC (with unknown artist/title/album)
    let res_isrc = fetch_from_database_inner(
        &pool,
        "different artist",
        "different title",
        "different album",
        Some(180.0),
        Some("USUM71234567"),
        None,
        None,
    )
    .await;
    assert!(res_isrc.is_some());

    // 4. Fetch by secondary ISRC (with unknown artist/title/album)
    let res_multi_isrc = fetch_from_database_inner(
        &pool,
        "different artist",
        "different title",
        "different album",
        Some(180.0),
        Some("ALTISRC99999"),
        None,
        None,
    )
    .await;
    assert!(res_multi_isrc.is_some());

    // 5. Fetch by primary Spotify ID
    let res_spotify = fetch_from_database_inner(
        &pool,
        "different artist",
        "different title",
        "different album",
        Some(180.0),
        None,
        Some("5FVd6KXrgO9B3JPmC8OPst"),
        None,
    )
    .await;
    assert!(res_spotify.is_some());

    // 6. Fetch by secondary Spotify ID
    let res_secondary_spotify = fetch_from_database_inner(
        &pool,
        "different artist",
        "different title",
        "different album",
        Some(180.0),
        None,
        Some("2UzMpPKPhbcC8RbsmuURAZ"),
        None,
    )
    .await;
    assert!(res_secondary_spotify.is_some());

    // 7. Fetch by primary iTunes ID
    let res_itunes = fetch_from_database_inner(
        &pool,
        "different artist",
        "different title",
        "different album",
        Some(180.0),
        None,
        None,
        Some("123456789"),
    )
    .await;
    assert!(res_itunes.is_some());

    // 8. Fetch by secondary iTunes ID
    let res_secondary_itunes = fetch_from_database_inner(
        &pool,
        "different artist",
        "different title",
        "different album",
        Some(180.0),
        None,
        None,
        Some("987654321"),
    )
    .await;
    assert!(res_secondary_itunes.is_some());

    // 9. Fetch with duration within tolerance (diff 1.5s <= 2.0s)
    let res_within_tolerance = fetch_from_database_inner(
        &pool,
        "test artist",
        "test title",
        "test album",
        Some(181.5),
        None,
        None,
        None,
    )
    .await;
    assert!(res_within_tolerance.is_some());

    // 10. Test duration tolerance rejection (>2.0s difference) returns None
    let res_mismatch = fetch_from_database_inner(
        &pool,
        "test artist",
        "test title",
        "test album",
        Some(185.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_mismatch.is_none());

    // 11. Confirm row in lyrics AND associated aliases/identifiers were NOT purged by duration mismatch
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1);

    let alias_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track_aliases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alias_count.0, 1);

    let identifier_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track_identifiers")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(identifier_count.0, 6);
}

#[tokio::test]
async fn test_database_multi_album_deduplication() {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    create_schema(&pool).await.unwrap();

    let lrc_content = "[00:10.00]Song lyric\n[00:20.00]Another line";
    let isrcs = vec!["USRC12345678".to_string()];
    let spotify_ids = vec!["spotify_track_1".to_string()];

    // 1. Store song from Album 1 ("Original Album")
    store_in_database_inner(
        &pool,
        "Artist Name",
        "Song Title",
        "Original Album",
        Some(200.0),
        LyricsFormat::Lrclib,
        lrc_content.to_string(),
        &isrcs,
        &spotify_ids,
        &[],
    )
    .await;

    // 2. Store same song from Album 2 ("Greatest Hits")
    store_in_database_inner(
        &pool,
        "Artist Name",
        "Song Title",
        "Greatest Hits",
        Some(200.0),
        LyricsFormat::Lrclib,
        lrc_content.to_string(),
        &isrcs,
        &spotify_ids,
        &[],
    )
    .await;

    // 3. Store same song from Album 3 ("Singles Collection") with no ISRC, matching on artist/title + duration
    store_in_database_inner(
        &pool,
        "Artist Name",
        "Song Title",
        "Singles Collection",
        Some(200.5),
        LyricsFormat::Lrclib,
        lrc_content.to_string(),
        &[],
        &[],
        &[],
    )
    .await;

    // Assert only 1 lyrics row was created (deduplicated)
    let lyrics_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lyrics_count.0, 1);

    // Assert 3 track_aliases rows exist
    let alias_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track_aliases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alias_count.0, 3);

    // Assert track_identifiers does not duplicate
    let identifier_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track_identifiers")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(identifier_count.0, 2);

    // 4. Fetch under Album 2 without ISRC -> direct alias hit
    let res_album2 = fetch_from_database_inner(
        &pool,
        "Artist Name",
        "Song Title",
        "Greatest Hits",
        Some(200.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_album2.is_some());

    // 5. Fetch under an un-cached album ("Deluxe Edition") -> hits fallback (artist, title)
    let res_fallback = fetch_from_database_inner(
        &pool,
        "Artist Name",
        "Song Title",
        "Deluxe Edition",
        Some(200.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_fallback.is_some());

    // Fallback automatically registered "Deluxe Edition" into aliases
    let alias_count_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track_aliases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alias_count_after.0, 4);
}

#[tokio::test]
async fn test_database_duration_differentiation() {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    create_schema(&pool).await.unwrap();

    // 1. Studio version: 180s
    store_in_database_inner(
        &pool,
        "Band",
        "Track",
        "Studio Album",
        Some(180.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Studio line".to_string(),
        &["STUDIO12345".to_string()],
        &[],
        &[],
    )
    .await;

    // 2. Live version: 300s (diff 120s > 2.0s, distinct ISRC)
    store_in_database_inner(
        &pool,
        "Band",
        "Track",
        "Live in Concert",
        Some(300.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Live line".to_string(),
        &["LIVE1234567".to_string()],
        &[],
        &[],
    )
    .await;

    // Assert 2 distinct lyrics rows were created
    let lyrics_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lyrics_count.0, 2);

    // Fetching studio duration returns studio lyrics
    let res_studio = fetch_from_database_inner(
        &pool,
        "Band",
        "Track",
        "Studio Album",
        Some(180.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_studio.is_some());
    let (res, _) = res_studio.unwrap();
    let (lines, _, _) = res.unwrap();
    assert_eq!(lines[0].text, "Studio line");

    // Fetching live duration returns live lyrics
    let res_live = fetch_from_database_inner(
        &pool,
        "Band",
        "Track",
        "Live in Concert",
        Some(300.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_live.is_some());
    let (res, _) = res_live.unwrap();
    let (lines, _, _) = res.unwrap();
    assert_eq!(lines[0].text, "Live line");
}

#[tokio::test]
async fn test_database_cascade_delete() {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    create_schema(&pool).await.unwrap();

    store_in_database_inner(
        &pool,
        "Artist",
        "Title",
        "Album",
        Some(150.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Line".to_string(),
        &["ISRC1".to_string()],
        &["SPOT1".to_string()],
        &[],
    )
    .await;

    let row: (i64,) = sqlx::query_as("SELECT id FROM lyrics LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();

    delete_cached_row(&pool, row.0).await;

    let count_lyrics: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    let count_aliases: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track_aliases")
        .fetch_one(&pool)
        .await
        .unwrap();
    let count_ids: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track_identifiers")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(count_lyrics.0, 0);
    assert_eq!(count_aliases.0, 0);
    assert_eq!(count_ids.0, 0);
}

#[tokio::test]
async fn test_database_store_duration_guard_and_identifier_stealing() {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    create_schema(&pool).await.unwrap();

    // 1. Store studio version: 180s with shared ISRC
    store_in_database_inner(
        &pool,
        "Artist",
        "Song",
        "Studio Album",
        Some(180.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Studio line".to_string(),
        &["USUM71234567".to_string()],
        &["spotify_track_123".to_string()],
        &[],
    )
    .await;

    // 2. Store live version: 300s with same ISRC and same Spotify ID
    store_in_database_inner(
        &pool,
        "Artist",
        "Song",
        "Live Album",
        Some(300.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Live line".to_string(),
        &["USUM71234567".to_string()],
        &["spotify_track_123".to_string()],
        &[],
    )
    .await;

    // Verify 2 distinct canonical rows were created
    let lyrics_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lyrics_count.0, 2);

    // Verify 2 aliases exist
    let alias_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track_aliases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alias_count.0, 2);

    // Verify identifiers now point to live version (stolen)
    let live_id: (i64,) = sqlx::query_as("SELECT id FROM lyrics WHERE duration = 300.0")
        .fetch_one(&pool)
        .await
        .unwrap();
    let isrc_owner: (i64,) = sqlx::query_as(
        "SELECT lyrics_id FROM track_identifiers WHERE kind = 'isrc' AND value = 'USUM71234567'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(isrc_owner.0, live_id.0);

    // 3. Query studio version at 180s:
    // ISRC matches live_id, but duration mismatch -> falls through to track_aliases (Studio Album) -> returns studio lyrics
    let res_studio = fetch_from_database_inner(
        &pool,
        "Artist",
        "Song",
        "Studio Album",
        Some(180.0),
        Some("USUM71234567"),
        Some("spotify_track_123"),
        None,
    )
    .await;
    assert!(res_studio.is_some());
    let (res, _) = res_studio.unwrap();
    let (lines, _, _) = res.unwrap();
    assert_eq!(lines[0].text, "Studio line");

    // 4. Query live version at 300s:
    // ISRC matches live_id and duration matches -> returns live lyrics
    let res_live = fetch_from_database_inner(
        &pool,
        "Artist",
        "Song",
        "Live Album",
        Some(300.0),
        Some("USUM71234567"),
        Some("spotify_track_123"),
        None,
    )
    .await;
    assert!(res_live.is_some());
    let (res, _) = res_live.unwrap();
    let (lines, _, _) = res.unwrap();
    assert_eq!(lines[0].text, "Live line");
}

#[tokio::test]
async fn test_database_format_downgrade_protection() {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    create_schema(&pool).await.unwrap();

    let richsync_payload = r#"[{"ts":10.0,"x":"word","l":[{"c":"word","o":0}]}]"#;
    let lrc_payload = "[00:10.00]word";

    // 1. Store high-fidelity Richsync lyrics
    store_in_database_inner(
        &pool,
        "Artist",
        "Song",
        "Original Album",
        Some(200.0),
        LyricsFormat::Richsync,
        richsync_payload.to_string(),
        &["ISRC_RICH_1".to_string()],
        &[],
        &[],
    )
    .await;

    // 2. Store lower-fidelity Lrclib lyrics for same track (different album)
    store_in_database_inner(
        &pool,
        "Artist",
        "Song",
        "Compilation Album",
        Some(200.0),
        LyricsFormat::Lrclib,
        lrc_payload.to_string(),
        &["ISRC_RICH_1".to_string()],
        &[],
        &[],
    )
    .await;

    // Verify only 1 canonical row exists
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1);

    // Verify format in database is STILL Richsync
    let format_str: (String,) = sqlx::query_as("SELECT format FROM lyrics LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(format_str.0, "richsync");

    // Verify both album aliases were registered
    let alias_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track_aliases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alias_count.0, 2);

    // 3. Fetch via the new compilation album alias -> returns Richsync
    let res_comp = fetch_from_database_inner(
        &pool,
        "Artist",
        "Song",
        "Compilation Album",
        Some(200.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_comp.is_some());
    let (res, format) = res_comp.unwrap();
    assert_eq!(format, LyricsFormat::Richsync);
    let (lines, _, _) = res.unwrap();
    assert_eq!(lines[0].text, "word");
    assert!(lines[0].words.is_some());
}

#[tokio::test]
async fn test_database_unknown_format_does_not_delete() {
    use super::compress::compress_raw_lyrics;

    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    create_schema(&pool).await.unwrap();

    let compressed = compress_raw_lyrics("sample lyric text").unwrap();

    // Hand-insert a row with an unknown format string 'future_format'
    let insert_res: (i64,) = sqlx::query_as(
        r#"
        INSERT INTO lyrics (artist, title, duration, format, raw_lyrics)
        VALUES ('future artist', 'future title', 150.0, 'future_format', ?)
        RETURNING id
        "#,
    )
    .bind(compressed)
    .fetch_one(&pool)
    .await
    .unwrap();

    let lyrics_id = insert_res.0;

    sqlx::query(
        "INSERT INTO track_aliases (artist, title, album, lyrics_id) VALUES ('future artist', 'future title', 'future album', ?)",
    )
    .bind(lyrics_id)
    .execute(&pool)
    .await
    .unwrap();

    // Fetch should return None because format is unknown
    let res = fetch_from_database_inner(
        &pool,
        "future artist",
        "future title",
        "future album",
        Some(150.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res.is_none());

    // Row MUST NOT have been deleted from lyrics table
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1);
}

#[tokio::test]
async fn test_database_isrc_normalization() {
    use super::compress::normalize_isrc;

    // Unit checks for normalize_isrc
    assert_eq!(
        normalize_isrc("US-SUM-71-23456").as_deref(),
        Some("USSUM7123456")
    );
    assert_eq!(
        normalize_isrc("  us-um7-12-34567 ").as_deref(),
        Some("USUM71234567")
    );
    assert_eq!(normalize_isrc(""), None);
    assert_eq!(normalize_isrc("   ---   "), None);

    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    create_schema(&pool).await.unwrap();

    // Store using hyphenated lowercase ISRC
    store_in_database_inner(
        &pool,
        "Artist",
        "Title",
        "Album",
        Some(180.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Normalized line".to_string(),
        &["us-um7-12-34567".to_string()],
        &[],
        &[],
    )
    .await;

    // Fetch using clean uppercase ISRC
    let res_clean = fetch_from_database_inner(
        &pool,
        "Different",
        "Different",
        "Different",
        Some(180.0),
        Some("USUM71234567"),
        None,
        None,
    )
    .await;
    assert!(res_clean.is_some());
    let (res, _) = res_clean.unwrap();
    let (lines, _, _) = res.unwrap();
    assert_eq!(lines[0].text, "Normalized line");

    // Fetch using hyphenated uppercase ISRC
    let res_hyphen = fetch_from_database_inner(
        &pool,
        "Different",
        "Different",
        "Different",
        Some(180.0),
        Some("US-UM7-12-34567"),
        None,
        None,
    )
    .await;
    assert!(res_hyphen.is_some());
}

#[tokio::test]
async fn test_database_stale_alias_update_on_duration_mismatch() {
    let pool = create_test_pool().await;

    // 1. Store 300s Live version under Album "Greatest Hits"
    store_in_database_inner(
        &pool,
        "Artist",
        "Song",
        "Greatest Hits",
        Some(300.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Live version".to_string(),
        &[],
        &[],
        &[],
    )
    .await;

    // 2. Store 180s Studio version under Album "Original Album"
    store_in_database_inner(
        &pool,
        "Artist",
        "Song",
        "Original Album",
        Some(180.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Studio version".to_string(),
        &[],
        &[],
        &[],
    )
    .await;

    // 3. User plays 180s Studio version labeled under "Greatest Hits".
    // Initial alias for "Greatest Hits" pointed to 300s recording.
    // Step 2 rejects due to duration mismatch.
    // Step 5 matches "Artist", "Song" with duration 180s, and updates the alias to point to the 180s recording.
    let res = fetch_from_database_inner(
        &pool,
        "Artist",
        "Song",
        "Greatest Hits",
        Some(180.0),
        None,
        None,
        None,
    )
    .await;

    assert!(res.is_some());
    let (prov, _) = res.unwrap();
    let (lines, _, _) = prov.unwrap();
    assert_eq!(lines[0].text, "Studio version");

    // 4. Verify that the alias was updated: next query for 180s on "Greatest Hits" hits directly
    let res_subsequent = fetch_from_database_inner(
        &pool,
        "Artist",
        "Song",
        "Greatest Hits",
        Some(180.0),
        None,
        None,
        None,
    )
    .await;

    assert!(res_subsequent.is_some());
    let (prov2, _) = res_subsequent.unwrap();
    let (lines2, _, _) = prov2.unwrap();
    assert_eq!(lines2[0].text, "Studio version");
}

#[tokio::test]
async fn test_database_empty_and_corrupt_lyrics_purging() {
    let pool = create_test_pool().await;

    // 1. Attempt to store empty / whitespace-only raw lyrics -> should not insert row
    store_in_database_inner(
        &pool,
        "Artist",
        "Song",
        "Album",
        Some(180.0),
        LyricsFormat::Lrclib,
        "   \n\t  ".to_string(),
        &[],
        &[],
        &[],
    )
    .await;

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 0);

    // 2. Store valid raw lyrics
    store_in_database_inner(
        &pool,
        "Artist",
        "Song",
        "Album",
        Some(180.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Valid line".to_string(),
        &[],
        &[],
        &[],
    )
    .await;

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1);

    // 3. Corrupt raw_lyrics with non-decompressable junk bytes in the database
    sqlx::query("UPDATE lyrics SET raw_lyrics = X'12345678'")
        .execute(&pool)
        .await
        .unwrap();

    // Fetch should detect decompression failure, purge the row, and return None
    let res = fetch_from_database_inner(
        &pool,
        "Artist",
        "Song",
        "Album",
        Some(180.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res.is_none());

    // Row should have been purged
    let count_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count_after.0, 0);
}

#[tokio::test]
async fn test_database_duration_zero_unknown_tolerance() {
    let pool = create_test_pool().await;

    // Store track with known length 200.0s
    store_in_database_inner(
        &pool,
        "Artist",
        "Song",
        "Album",
        Some(200.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Line 1".to_string(),
        &[],
        &[],
        &[],
    )
    .await;

    // Query with length 0.0s (e.g., streaming radio or player metadata not yet loaded)
    // Should be treated as unknown duration and match instead of rejecting as 200s difference
    let res_zero = fetch_from_database_inner(
        &pool,
        "Artist",
        "Song",
        "Album",
        Some(0.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_zero.is_some());
}

#[tokio::test]
async fn test_database_consolidated_identifier_ordering() {
    let pool = create_test_pool().await;

    let isrcs = vec![
        "ISRC11111111".to_string(),
        "ISRC22222222".to_string(),
        "ISRC33333333".to_string(),
    ];
    let spot_ids = vec!["spot_1".to_string(), "spot_2".to_string()];
    let itunes_ids = vec!["itunes_1".to_string(), "itunes_2".to_string()];

    store_in_database_inner(
        &pool,
        "Artist",
        "Song",
        "Album",
        Some(180.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Line".to_string(),
        &isrcs,
        &spot_ids,
        &itunes_ids,
    )
    .await;

    // Fetch and check all identifiers are preserved in order
    let res = fetch_from_database_inner(
        &pool,
        "Artist",
        "Song",
        "Album",
        Some(180.0),
        None,
        None,
        None,
    )
    .await;

    assert!(res.is_some());
    let (prov, _) = res.unwrap();
    let (_, _, ids) = prov.unwrap();

    assert_eq!(ids.track_isrcs, isrcs);
    assert_eq!(ids.track_spotify_ids, spot_ids);
    assert_eq!(ids.track_itunes_ids, itunes_ids);
}

#[test]
fn test_database_punctuation_canonicalization() {
    // Smart apostrophes & backticks
    assert_eq!(normalize("Don’t Stop"), "don't stop");
    assert_eq!(normalize("‘Don’t’"), "'don't'");
    assert_eq!(normalize("Don`t Stop"), "don't stop");

    // Smart double quotes
    assert_eq!(normalize("“Heroes”"), "\"heroes\"");
    assert_eq!(normalize("„Heroes‟"), "\"heroes\"");

    // Dashes (en-dash, em-dash, horizontal bar)
    assert_eq!(normalize("Artist – Song"), "artist - song");
    assert_eq!(normalize("Artist — Song"), "artist - song");
    assert_eq!(normalize("Artist ― Song"), "artist - song");

    // Non-breaking & zero-width spaces
    assert_eq!(normalize("Song\u{00A0}Title"), "song title");
    assert_eq!(normalize("Song\u{200B}Title"), "songtitle");
    assert_eq!(normalize("Song\u{FEFF}Title"), "songtitle");
}

#[test]
fn test_database_strip_remaster_suffix() {
    // Standard hyphen remasters
    assert_eq!(
        strip_remaster_suffix("Heroes - 2015 Remaster"),
        Some("Heroes".to_string())
    );
    assert_eq!(
        strip_remaster_suffix("Heroes - Remastered"),
        Some("Heroes".to_string())
    );
    assert_eq!(
        strip_remaster_suffix("Heroes - Remaster"),
        Some("Heroes".to_string())
    );
    assert_eq!(
        strip_remaster_suffix("Heroes - 2011 - Remaster"),
        Some("Heroes".to_string())
    );
    assert_eq!(
        strip_remaster_suffix("Heroes - 2009 Digital Remaster"),
        Some("Heroes".to_string())
    );
    assert_eq!(
        strip_remaster_suffix("Heroes - Digital Remaster"),
        Some("Heroes".to_string())
    );

    // Parentheses and brackets
    assert_eq!(
        strip_remaster_suffix("Heroes (2015 Remastered Version)"),
        Some("Heroes".to_string())
    );
    assert_eq!(
        strip_remaster_suffix("Heroes [2011 Remaster]"),
        Some("Heroes".to_string())
    );
    assert_eq!(
        strip_remaster_suffix("Heroes (Remastered 2011)"),
        Some("Heroes".to_string())
    );
    assert_eq!(
        strip_remaster_suffix("Heroes (40th Anniversary Remaster)"),
        Some("Heroes".to_string())
    );

    // Should NOT strip semantic versions
    assert_eq!(strip_remaster_suffix("Heroes (Live at Wembley)"), None);
    assert_eq!(strip_remaster_suffix("Heroes (feat. Brian Eno)"), None);
    assert_eq!(strip_remaster_suffix("Heroes (Radio Edit)"), None);
    assert_eq!(strip_remaster_suffix("Heroes"), None);
    assert_eq!(strip_remaster_suffix("Remaster"), None);
}

#[tokio::test]
async fn test_database_punctuation_store_and_query() {
    let pool = create_test_pool().await;

    // Store with smart apostrophe and typographic dash
    store_in_database_inner(
        &pool,
        "Fleetwood Mac",
        "Don’t Stop — Remastered",
        "Rumours",
        Some(193.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Don't stop thinking about tomorrow".to_string(),
        &[],
        &[],
        &[],
    )
    .await;

    // Query with straight apostrophe and ASCII hyphen
    let res = fetch_from_database_inner(
        &pool,
        "Fleetwood Mac",
        "Don't Stop - Remastered",
        "Rumours",
        Some(193.0),
        None,
        None,
        None,
    )
    .await;

    assert!(res.is_some());
    let (prov, _) = res.unwrap();
    let (lines, _, _) = prov.unwrap();
    assert_eq!(lines[0].text, "Don't stop thinking about tomorrow");
}

#[tokio::test]
async fn test_database_remaster_bidirectional_aliasing() {
    let pool = create_test_pool().await;

    // 1. Store clean title in DB
    store_in_database_inner(
        &pool,
        "David Bowie",
        "Heroes",
        "Heroes",
        Some(360.0),
        LyricsFormat::Lrclib,
        "[00:10.00]I will be king".to_string(),
        &[],
        &[],
        &[],
    )
    .await;

    // 2. Query with player's remaster title: "Heroes - 2015 Remaster"
    // Should match via fallback and self-heal
    let res_remaster = fetch_from_database_inner(
        &pool,
        "David Bowie",
        "Heroes - 2015 Remaster",
        "Heroes",
        Some(360.0),
        None,
        None,
        None,
    )
    .await;

    assert!(res_remaster.is_some());
    let (prov, _) = res_remaster.unwrap();
    let (lines, _, _) = prov.unwrap();
    assert_eq!(lines[0].text, "I will be king");

    // Verify self-healing alias exists in DB
    let alias_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM track_aliases WHERE artist = 'david bowie' AND title = 'heroes - 2015 remaster'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(alias_count.0, 1);

    // 3. Second track: Store with remaster annotation in title
    store_in_database_inner(
        &pool,
        "Queen",
        "Bohemian Rhapsody (2011 Remaster)",
        "A Night at the Opera",
        Some(354.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Is this the real life".to_string(),
        &[],
        &[],
        &[],
    )
    .await;

    // Both "bohemian rhapsody (2011 remaster)" AND "bohemian rhapsody" should be in track_aliases
    let aliases: Vec<(String,)> =
        sqlx::query_as("SELECT title FROM track_aliases WHERE artist = 'queen' ORDER BY title ASC")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(aliases.len(), 2);
    assert_eq!(aliases[0].0, "bohemian rhapsody");
    assert_eq!(aliases[1].0, "bohemian rhapsody (2011 remaster)");

    // Query clean title: "Bohemian Rhapsody"
    let res_clean = fetch_from_database_inner(
        &pool,
        "Queen",
        "Bohemian Rhapsody",
        "A Night at the Opera",
        Some(354.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_clean.is_some());

    // Query another remaster variation: "Bohemian Rhapsody - 2015 Remaster"
    let res_other_remaster = fetch_from_database_inner(
        &pool,
        "Queen",
        "Bohemian Rhapsody - 2015 Remaster",
        "A Night at the Opera",
        Some(354.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_other_remaster.is_some());

    // Verify canonical lyrics table has exactly 2 rows (one for David Bowie, one for Queen)
    let total_lyrics: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total_lyrics.0, 2);
}

#[tokio::test]
async fn test_database_remaster_duration_gating() {
    let pool = create_test_pool().await;

    // Store track with length 360.0s
    store_in_database_inner(
        &pool,
        "David Bowie",
        "Heroes",
        "Heroes",
        Some(360.0),
        LyricsFormat::Lrclib,
        "[00:10.00]Full album version".to_string(),
        &[],
        &[],
        &[],
    )
    .await;

    // Query with single edit / different version length (180.0s) -> should NOT match
    let res_mismatch = fetch_from_database_inner(
        &pool,
        "David Bowie",
        "Heroes - 2015 Remaster",
        "Heroes",
        Some(180.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_mismatch.is_none());

    // Query with matching length (361.0s, within 2.0s tolerance) -> should match
    let res_match = fetch_from_database_inner(
        &pool,
        "David Bowie",
        "Heroes - 2015 Remaster",
        "Heroes",
        Some(361.0),
        None,
        None,
        None,
    )
    .await;
    assert!(res_match.is_some());
}
