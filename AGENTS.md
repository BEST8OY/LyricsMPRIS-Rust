# AGENTS.md

Rust CLI app for synchronized lyrics display via MPRIS (Linux D-Bus). Single crate, no workspace.

## Build & Verify

```bash
cargo build --release          # Release binary at target/release/lyricsmpris
cargo test                     # Unit tests (inline #[cfg(test)] modules)
cargo clippy                   # Lints
cargo fmt --check              # Formatting check
```

No separate test runner, CI config, or Makefile exists. Tests are embedded in source files.

## Architecture

```
src/
├── main.rs           # Entry point, CLI parsing (clap), tokio runtime
├── event.rs          # Event processing (track changes, seeks, playback updates)
├── pool.rs           # Event loop orchestrator (wires D-Bus → state → UI)
├── state.rs          # StateBundle, PlayerState, LyricState, Update snapshot
├── timer.rs          # PlaybackTimer (monotonic position estimation)
├── text_utils.rs     # Text wrapping utility
├── lyrics/
│   ├── providers/    # lrclib.rs, musixmatch.rs (API integrations)
│   ├── database.rs   # SQLite cache with sqlx + zstd compression
│   ├── parse.rs      # LRC, Richsync, subtitle parsers
│   ├── similarity.rs # Fuzzy matching
│   └── types.rs      # LyricLine, error types
├── mpris/
│   ├── connection.rs # D-Bus session singleton, playerctld proxy
│   ├── events.rs     # D-Bus signal listener (PropertiesChanged)
│   ├── metadata.rs   # Track metadata extraction from D-Bus
│   └── playback.rs   # Playback position/status queries
└── ui/
    ├── modern.rs     # Ratatui TUI (auto-scroll, karaoke, manual scroll)
    ├── pipe.rs       # Stdout pipe mode for bar integration
    └── ...           # Helper modules (styles, progression, util)
```

**Data flow**: D-Bus events → `pool.rs` event loop → `state.rs` StateBundle → UI update channel → `ui/` renders.

## Key Dependencies

- `zbus` v5 — D-Bus (tokio feature)
- `ratatui` + `crossterm` — TUI rendering
- `sqlx` — SQLite async (runtime-tokio)
- `zstd` — Compression for cached lyrics blobs
- `clap` v4 — CLI argument parsing (derive mode)
- `tokio` — Async runtime (full features)

## Conventions

- **Rust edition 2024** (`edition = "2024"` in Cargo.toml). Requires Rust 1.70+.
- Logging goes to **stderr** via `tracing` + `tracing-subscriber` (env-filter). Off by default; set `RUST_LOG=debug` to enable.
- State uses immutable `Update` snapshots (Arc-wrapped lyrics) broadcast to observers. Don't mutate `Update` directly.
- D-Bus connection is a global singleton (`OnceCell<Arc<Connection>>`).
- Musixmatch tokens rotate round-robin with automatic fallback on 429/401/403.
- Provider priority: configurable via `--providers` CLI flag or `LYRIC_PROVIDERS` env var. Default: `lrclib,musixmatch`.
- SQLite cache keys on `(artist, title, album)` — all normalized lowercase. Raw lyrics stored as zstd-compressed blobs.

## Testing

Tests are inline `#[cfg(test)]` modules in: `state.rs`, `timer.rs`, `lyrics/providers/musixmatch.rs`, `mpris/playback.rs`, `mpris/metadata.rs`.

Run with `cargo test`. No external services required for unit tests (they use mocked/local data).

## Gotchas

- No `rustfmt.toml` or `clippy.toml` — uses Rust defaults.
- No CI workflows (`.github/` is empty).
- `playerctld` is a recommended runtime dependency for tracking the active MPRIS player. Without it, the app uses a fallback discovery path.
- The `--database` flag enables SQLite caching; without it, no persistence occurs.
- `--pipe` mode writes lyrics to stdout — keep stderr for logs to avoid polluting output.
