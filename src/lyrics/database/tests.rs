//! Integration and unit tests for database CRUD operations, alias deduplication, and identifier indexing.

use super::ops::{delete_cached_row, fetch_from_database_inner, store_in_database_inner};
use super::schema::{LyricsFormat, create_schema};
use sqlx::sqlite::SqlitePoolOptions;

#[tokio::test]
async fn test_database_crud_and_lookups() {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    create_schema(&pool).await.unwrap();

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
