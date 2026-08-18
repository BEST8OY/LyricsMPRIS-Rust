//! Integration and unit tests for database CRUD operations and unified identifier indexing.

use super::ops::{fetch_from_database_inner, store_in_database_inner};
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
    // Assert all associated IDs are correctly recovered
    assert_eq!(ids.track_isrcs, isrcs);
    assert_eq!(ids.track_spotify_ids, spotify_ids);
    assert_eq!(ids.track_itunes_ids, itunes_ids);

    // 3. Fetch by primary ISRC (with unknown artist/title)
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

    // 4. Fetch by secondary ISRC (with unknown artist/title)
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

    // 10. Test duration tolerance rejection (>2.0s difference)
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

    // 11. Confirm row in lyrics AND all associated identifiers in track_identifiers were deleted due to duration mismatch
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lyrics")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 0);

    let identifier_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track_identifiers")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(identifier_count.0, 0);
}
