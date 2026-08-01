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
- **Musixmatch Token Engine (`src/lyrics/providers/musixmatch.rs`)**:
  - Tier 1: Round-robin pool from `MUSIXMATCH_USERTOKEN` env var.
  - Tier 2: Auto-fetches anonymous token via `token.get` API if Tier 1 is unconfigured or exhausted.
  - Tier 3: Auto-purges invalid tokens on 401/402/403/429 responses, requests a fresh token, and retries once transparently.
- **Database Engine (`src/lyrics/database.rs`)**:
  - SQLite pool with `PRIMARY KEY (artist, title, album)`.
  - Raw lyrics stored as zstd-compressed blobs.
  - Lookup priority: ISRC (`WHERE isrc = ?`) → `(artist, title, album)` → Spotify ID → iTunes ID.
  - Duration matching: Clamped within 5% tolerance window; invalid/mismatched entries automatically purged.
- **ISRC Extraction (`src/mpris/metadata.rs`)**:
  - `--isrc` inspects `file://` or path in `xesam:url` using `lofty` to extract ISRC tags. Uses `block_in_place` conditionally inside Tokio runtime.
- **Logging**: Goes exclusively to `stderr` via `tracing` + `tracing-subscriber`. Enable with `RUST_LOG=debug`. Keep `stdout` clean for `--pipe` mode.
