# AGENTS.md

Rust CLI app for synchronized lyrics display via MPRIS (Linux D-Bus). Single binary crate, no workspace.

## Build & Verify

Execute the standard verification pipeline after making changes:

```bash
cargo fmt --check && cargo clippy && cargo test
```

- **Build release**: `cargo build --release` (binary at `target/release/lyricsmpris`)
- **Single test run**: `cargo test -- <test_name_filter>` (e.g., `cargo test -- test_prepared_track_scoring`)
- No Makefile, CI runner, or external database setup required. Tests run inline without external services.

## Architecture & Data Flow

```
D-Bus signals / MPRIS polling
      │
      ▼
src/mpris/ (events.rs, metadata.rs, connection.rs)
      │  (extracts TrackMetadata + YouTube videoId + local ISRC via lofty if --isrc)
      ▼
src/pool.rs (event loop orchestrator & 'r' candidate cycling)
      │
      ▼
src/event/ (fetch.rs, display_state.rs, handlers.rs) (3-stage concurrent pipeline)
      └─▶ 23 External providers (src/lyrics/providers/ + crypto/parsers/filter)
      │
      ▼
src/state.rs (StateBundle -> Update snapshot broadcast)
      │
      ▼
src/ui/ (modern.rs Ratatui TUI or pipe.rs stdout)
```

## Key Components & Conventions

- **Rust Edition 2024** (`edition = "2024"` in Cargo.toml). Enables Edition 2024 let chains (`if let ... && let ...`). Uses `std::sync::LazyLock`.
- **Configuration (`src/config.rs`)**: Layered precedence (`CLI > env > TOML (~/.config/lyricsmpris/config.toml or --config) > defaults`). Supports `-provider` exclusion tokens, `--allow-plain`, `--no-split-bg`, custom base URLs, and provider tokens.
- **MPRIS & D-Bus Engine (`src/mpris/`, `src/pool.rs`)**:
  - Consolidated Proxies: All D-Bus proxy interfaces (`MediaPlayer2Player`, `Playerctld`) are centralized in `src/mpris/proxies.rs`.
  - Discovery Priority: Queries `playerctld` (recency-ordered) first; falls back to standard D-Bus `ListNames` (`org.mpris.MediaPlayer2.*`, excluding `playerctld`) when `playerctld` is absent or returns empty, ranking candidates by `PlaybackPriority` (`Playing` > `Paused` > `Stopped` > `Unresponsive`) with concurrent status probing.
  - Stability & Standalone Handoff: Retains current player on priority ties to prevent bouncing; immediately yields to newly playing players via broker-level `PropertiesChanged` match rules on `/org/mpris/MediaPlayer2`, providing 100% event-driven cross-player handoff and reactive idle wakeup without requiring `playerctld`.
  - Error Safety: Discovery helpers return `Result<Option<String>, MprisError>`; transient D-Bus errors preserve current state and never clear/deactivate active lyrics.
  - Pure Event-Driven Idle: Inactive state uses pure event-driven sleep waiting for D-Bus `NameOwnerChanged`, broker-level `PropertiesChanged`, and `playerctld` property streams, generating zero IPC traffic while idle.
  - Real-Time Position & Property Streams: `Metadata` and `PlaybackStatus` use standard caching (`CacheProperties::Lazily`) enabling property change streams. Dynamic `Position` is queried via `get_position()` using direct D-Bus `Properties.Get` (via `conn.call_method()`) to avoid stale cached position data and eliminate proxy construction overhead, propagating errors so callers retain estimated positions on transient failures.
  - Resilience: The watcher task in `pool.rs` retries with exponential backoff (500 ms → ×2, cap 30 s), resetting only after sustained uptime (≥ 5 s). The event loop backs off on error to prevent CPU spin.
  - Multi-Threaded Tag Reading: ISRC extraction checks `runtime_flavor() == RuntimeFlavor::MultiThread` before invoking `block_in_place`.
- **Multi-Provider & Parsers Pipeline (`src/event/{fetch.rs, display_state.rs}`, `src/lyrics/{crypto.rs, filter.rs, parsers/, providers/, types.rs}`)**:
  - 23 Morphe-ordered providers (`SUPPORTED_PROVIDERS` in `src/config.rs`), racing in 3 stages (`2s` / `4s` / `9s` deadlines, `5s` endgame, `800ms` word-sync `FIRST_PAINT_HOLD`, `15%` cut-off retry).
  - Full candidate queue fill runs in background only when interactive TUI cycling (`'r'`) is active; skipped in `--pipe` mode once published.
  - Parsers cover LRC/ELRC, TTML2, KRC, Lyricify (Lines/Syllable/Translation), and LyricsFile YAML (`crypto.rs` handles AES-ECB/PKCS#7, Kugou XOR+zlib, QRC 3DES-ECB, PetitLyrics LSY XOR).
  - Display prep (`prepare_lyrics_for_display`): credit filtering, text filtering, parenthesized background vocal splitting, overlap retiming, and anomalous word timing repairs.
- **Musixmatch Engine (`src/lyrics/providers/musixmatch/`)**:
  - Tier 1: Round-robin pool from `MUSIXMATCH_USERTOKEN` env var.
  - Tier 2: Auto-fetches anonymous token via `token.get` API if Tier 1 is unconfigured or exhausted.
  - Tier 3: Auto-purges invalid tokens on 401/402/403/429 responses, requests a fresh token, and retries once transparently.
  - Rate Limiting: 500ms Morphe request pacing per API call (no 10-minute cooldown lockout); expired Tier-1 tokens (`401` with `hint: renew`) are pruned for the session. Disk token cached at `~/.cache/lyricsmpris/musixmatch_token.json`.
  - Query Fallback Pipeline:
    1. Direct `macro.subtitles.get` combining metadata and track IDs (ISRC, Spotify ID), validated against target metadata.
    2. Pure metadata fallback (dropping IDs if ID lookup misses or mismatches).
    3. Candidate search (`track.search` via `search_and_fetch_best_candidate`).
  - Candidate Selection Rules (`select_best_candidate_track_id`):
    - Must have `has_richsync`, `has_subtitles`, `has_lyrics`, or `instrumental`.
    - Version tag parity (`VERSION_RE`): studio tracks will not match candidates with live, acoustic, remix, mix, demo, or rehearsal annotations.
    - Clamped within ±2.0s duration window; candidates with `track_length == 0.0` are rejected when target duration is known.
- **ISRC Extraction (`src/mpris/metadata.rs`)**:
  - `--isrc` inspects `file://` or path in `xesam:url` using `lofty` to extract ISRC tags. Also extracts `youtube_video_id` from YouTube URLs/trackids.
- **Logging**: Goes exclusively to `stderr` via `tracing` + `tracing-subscriber`. Enable with `RUST_LOG=debug`. Keep `stdout` clean for `--pipe` mode.
