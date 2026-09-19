# AGENTS.md

Rust CLI app for synchronized lyrics display via MPRIS (Linux D-Bus). Single binary crate, no workspace.

## Build & Verify

Execute the standard verification pipeline after making changes:

```bash
cargo fmt --check && cargo clippy && cargo test
```

- **Build release**: `cargo build --release` (binary at `target/release/lyricsmpris`)
- **Single test run**: `cargo test -- <test_name_filter>` (e.g., `cargo test -- test_database_crud`)
- No Makefile, CI runner, or external database setup required. Inline tests use in-memory SQLite and local mocks.

## Architecture & Data Flow

```
D-Bus signals / MPRIS polling
      │
      ▼
src/mpris/ (events.rs, metadata.rs, connection.rs)
      │  (extracts TrackMetadata + local ISRC via lofty if --isrc)
      ▼
src/pool.rs (event loop orchestrator)
      │
      ▼
src/event/ (fetch.rs, handlers.rs) (lyrics lookup pipeline)
      ├─▶ SQLite DB cache (src/lyrics/database/)
      └─▶ External providers (src/lyrics/providers/: lrclib.rs, musixmatch/)
      │
      ▼
src/state.rs (StateBundle -> Update snapshot broadcast)
      │
      ▼
src/ui/ (modern.rs Ratatui TUI or pipe.rs stdout)
```

## Key Components & Conventions

- **Rust Edition 2024** (`edition = "2024"` in Cargo.toml). Enables Edition 2024 let chains (`if let ... && let ...`).
- **MPRIS & D-Bus Engine (`src/mpris/`, `src/pool.rs`)**:
  - Consolidated Proxies: All D-Bus proxy interfaces (`MediaPlayer2Player`, `Playerctld`) are centralized in `src/mpris/proxies.rs`.
  - Discovery Priority: Queries `playerctld` (recency-ordered) first; falls back to standard D-Bus `ListNames` (`org.mpris.MediaPlayer2.*`, excluding `playerctld`) when `playerctld` is absent or returns empty, ranking candidates by `PlaybackPriority` (`Playing` > `Paused` > `Stopped` > `Unresponsive`) with concurrent status probing.
  - Stability & Standalone Handoff: Retains current player on priority ties to prevent bouncing; immediately yields to newly playing players via broker-level `PropertiesChanged` match rules on `/org/mpris/MediaPlayer2`, providing 100% event-driven cross-player handoff and reactive idle wakeup without requiring `playerctld`.
  - Error Safety: Discovery helpers return `Result<Option<String>, MprisError>`; transient D-Bus errors preserve current state and never clear/deactivate active lyrics.
  - Pure Event-Driven Idle: Inactive state uses pure event-driven sleep waiting for D-Bus `NameOwnerChanged`, broker-level `PropertiesChanged`, and `playerctld` property streams, generating zero IPC traffic while idle.
  - Real-Time Position & Property Streams: `Metadata` and `PlaybackStatus` use standard caching (`CacheProperties::Lazily`) enabling property change streams. Dynamic `Position` is queried via `get_position()` using direct D-Bus `Properties.Get` (via `conn.call_method()`) to avoid stale cached position data and eliminate proxy construction overhead, propagating errors so callers retain estimated positions on transient failures.
  - Resilience: The watcher task in `pool.rs` retries with exponential backoff (500 ms → ×2, cap 30 s), resetting only after sustained uptime (≥ 5 s). The event loop backs off on error to prevent CPU spin.
  - Multi-Threaded Tag Reading: ISRC extraction checks `runtime_flavor() == RuntimeFlavor::MultiThread` before invoking `block_in_place`.
- **Musixmatch Engine (`src/lyrics/providers/musixmatch/`)**:
  - Tier 1: Round-robin pool from `MUSIXMATCH_USERTOKEN` env var.
  - Tier 2: Auto-fetches anonymous token via `token.get` API if Tier 1 is unconfigured or exhausted.
  - Tier 3: Auto-purges invalid tokens on 401/402/403/429 responses, requests a fresh token, and retries once transparently.
  - Rate Limiting: 1.5s request spacing gate + 10-minute cooldown on non-renewable 401/402/403/429. Disk token cached at `~/.cache/lyricsmpris/musixmatch_token.json`.
  - Query Fallback Pipeline:
    1. Direct `macro.subtitles.get` combining metadata and track IDs (ISRC, Spotify ID, iTunes ID).
    2. Pure metadata fallback (dropping IDs if ID lookup returns `TrackNotFound`).
    3. Cleaned title fallback (stripping feat/remaster annotations via `clean_title_annotations`).
    4. Candidate search (`track.search` via `search_and_fetch_best_candidate`).
  - Candidate Selection Rules (`select_best_candidate_track_id`):
    - Must have `has_richsync`, `has_subtitles`, `has_lyrics`, or `instrumental`.
    - Version tag parity (`VERSION_RE`): studio tracks will not match candidates with live, acoustic, remix, mix, demo, or rehearsal annotations.
    - Clamped within ±2.0s duration window; candidates with `track_length == 0.0` are rejected when target duration is known.
- **Database Engine (`src/lyrics/database/`)**:
  - SQLite pool with `lyrics (id INTEGER PRIMARY KEY)`, multi-album alias table `track_aliases (artist, title, album, lyrics_id)` (`WITHOUT ROWID`), and unified `track_identifiers (kind, value, lyrics_id)` clustered table (`WITHOUT ROWID`) with cascading foreign keys.
  - Raw lyrics stored as zstd-compressed blobs with explicit `LyricsFormat` (`Richsync`, `Subtitles`, `Lrclib`).
  - Lookup priority: ISRC (`track_identifiers`) → `track_aliases (artist, title, album)` → Spotify ID → iTunes ID → fuzzy `(artist, title)` across any album.
  - Duration matching: Clamped within 2.0s tolerance window; duration-mismatched entries are retained (multi-version matching); only corrupt/unparseable entries are purged.
  - Format fidelity ranking: Richsync (3) > Subtitles (2) > Lrclib (1); incoming lower-fidelity formats never downgrade cached higher-fidelity lyrics.
- **ISRC Extraction (`src/mpris/metadata.rs`)**:
  - `--isrc` inspects `file://` or path in `xesam:url` using `lofty` to extract ISRC tags. Uses `block_in_place` conditionally inside Tokio runtime.
- **Logging**: Goes exclusively to `stderr` via `tracing` + `tracing-subscriber`. Enable with `RUST_LOG=debug`. Keep `stdout` clean for `--pipe` mode.
