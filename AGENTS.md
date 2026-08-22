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
src/event.rs (lyrics lookup pipeline)
      ├─▶ SQLite DB cache (src/lyrics/database.rs)
      └─▶ External providers (src/lyrics/providers/: lrclib, musixmatch)
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
  - Discovery Priority: Queries `playerctld` (recency-ordered) first; falls back to standard D-Bus `ListNames` (`org.mpris.MediaPlayer2.*`) when `playerctld` is absent or returns empty.
  - Error Safety: Discovery helpers return `Result<Option<String>, MprisError>`; transient D-Bus errors preserve current state and never clear/deactivate active lyrics.
  - Real-Time Lifecycle: Listens to D-Bus `NameOwnerChanged` signals for instant connect, disconnect, and ownership transfer (`old_owner != new_owner`) detection.
  - Authoritative Polling: Safety-net checks use `fdo::DBusProxy::name_has_owner(bus_name)` with 3.0s timeouts, bypassing client-side property caches. Inactive state polls every 2.0s.
  - Resilience: The watcher task in `pool.rs` retries with exponential backoff (500 ms → ×2, cap 30 s), resetting only after sustained uptime (≥ 5 s). The event loop backs off on error to prevent CPU spin.
  - Multi-Threaded Tag Reading: ISRC extraction checks `runtime_flavor() == RuntimeFlavor::MultiThread` before invoking `block_in_place`.
- **Musixmatch Token Engine (`src/lyrics/providers/musixmatch.rs`)**:
  - Tier 1: Round-robin pool from `MUSIXMATCH_USERTOKEN` env var.
  - Tier 2: Auto-fetches anonymous token via `token.get` API if Tier 1 is unconfigured or exhausted.
  - Tier 3: Auto-purges invalid tokens on 401/402/403/429 responses, requests a fresh token, and retries once transparently.
- **Database Engine (`src/lyrics/database/`)**:
  - SQLite pool with `lyrics (id INTEGER PRIMARY KEY)`, multi-album alias table `track_aliases (artist, title, album, lyrics_id)` (`WITHOUT ROWID`), and unified `track_identifiers (kind, value, lyrics_id)` clustered table (`WITHOUT ROWID`) with cascading foreign keys.
  - Raw lyrics stored as zstd-compressed blobs with explicit `LyricsFormat` (`Richsync`, `Subtitles`, `Lrclib`).
  - Lookup priority: ISRC (`track_identifiers`) → `track_aliases (artist, title, album)` → Spotify ID → iTunes ID → fuzzy `(artist, title)` across any album.
  - Duration matching: Clamped within 2.0s tolerance window; duration-mismatched entries are retained (multi-version matching); only corrupt/unparseable entries are purged.
  - Format fidelity ranking: Richsync (3) > Subtitles (2) > Lrclib (1); incoming lower-fidelity formats never downgrade cached higher-fidelity lyrics.
- **ISRC Extraction (`src/mpris/metadata.rs`)**:
  - `--isrc` inspects `file://` or path in `xesam:url` using `lofty` to extract ISRC tags. Uses `block_in_place` conditionally inside Tokio runtime.
- **Logging**: Goes exclusively to `stderr` via `tracing` + `tracing-subscriber`. Enable with `RUST_LOG=debug`. Keep `stdout` clean for `--pipe` mode.
