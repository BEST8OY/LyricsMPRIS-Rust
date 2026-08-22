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
    let (lines, raw, ids) = res_key.unwrap().unwrap();
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
    let (lines, _, _) = res_studio.unwrap().unwrap();
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
    let (lines, _, _) = res_live.unwrap().unwrap();
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
