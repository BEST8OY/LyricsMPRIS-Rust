# 🎵 LyricsMPRIS-Rust

A lightweight, ultra-high-performance synchronized lyrics viewer for Linux that integrates seamlessly with MPRIS-compatible media players.

[![Language](https://img.shields.io/badge/Language-Rust-orange.svg?style=flat-sq&logo=rust)](https://www.rust-lang.org/)
[![Edition](https://img.shields.io/badge/Edition-2024-red.svg?style=flat-sq)](https://doc.rust-lang.org/edition-guide/rust-2024/)
[![Platform](https://img.shields.io/badge/Platform-Linux-blue.svg?style=flat-sq&logo=linux)](https://kernel.org)
[![License](https://img.shields.io/badge/License-MIT-green.svg?style=flat-sq)](LICENSE)

`LyricsMPRIS-Rust` monitors your active media player in real-time using D-Bus, fetches synchronized lyrics automatically, and renders them beautifully in a modern Terminal User Interface (TUI) with word-level karaoke support. It also features a scriptable **Pipe Mode** for status bar integration, **ISRC tag reading** from local audio files, **multi-script Romanization** (Romaji, Pinyin, Cyrillic), and an optimized **local SQLite database** with Zstd compression for instant offline lookups.

![LyricMPRIS-Rust](https://github.com/user-attachments/assets/501f224e-6c40-46cd-ac66-cd9ae4f927cf)

---

## 📖 Table of Contents

1. [🌟 Key Features](#-key-features)
2. [🚀 Installation](#-installation)
3. [💻 Basic Usage](#-basic-usage)
4. [⚙️ Configuration](#-configuration)
5. [🎤 Musixmatch Token Management](#-musixmatch-token-management)
6. [💾 SQLite Database Cache](#-sqlite-database-cache)
7. [⌨️ Keyboard Shortcuts](#%EF%B8%8F-keyboard-shortcuts)
8. [🔌 Desktop Integration](#-desktop-integration)
9. [🏗️ Project Architecture](#%EF%B8%8F-project-architecture)
10. [🐛 Troubleshooting & Debugging](#-troubleshooting--debugging)
11. [🤝 Contributing](#-contributing)

---

## 🌟 Key Features

### 🎨 Modern TUI & Pipe Mode
* **Ratatui Terminal UI**: Centered lyrics display with smooth auto-scrolling, word-by-word karaoke highlighting, active line tracking, and resilient RAII terminal lifecycle management.
* **Adaptive Playback Calibration**: Transient position calibration automatically compensates for streaming buffer delays (e.g., Spotify, browser MPRIS extensions), Bluetooth latency, and seek jitter across all media players—locking into sample-accurate sync within ~1.2s before self-disarming for zero ongoing D-Bus polling overhead.
* **Manual Scroll**: Browse past or upcoming lyrics using arrow keys (`↑`/`↓`) when playback is paused.
* **Dynamic Runtime Toggles**: Hot-toggle per-word karaoke highlighting (`k`) and phonetic script romanization (`r`) in real-time during playback.
* **Streamlined Pipe Mode**: Outputs current lyric line to `stdout` in real-time, automatically clearing out-of-bound segments (intros/outros/silence), and routing transient network warnings to `stderr` to keep status bars (Polybar, Waybar, i3blocks) pristine.

### 🌐 Multi-Language Script Romanization
* **Foreign Script Transliteration**: Automatically generates synchronized Romaji, Pinyin, Korean Hangul, and Cyrillic phonetic subtitles for non-Latin lyrics using `kakasi` and `deunicode`.
* **Flexible Activation**: Enable at launch via `--romanize` or toggle interactively in the TUI with the `r` key.

### 📚 Multiple Lyrics Providers & Hedged Racing
* **LRCLIB**: Fetches community-sourced, line-synced lyrics with support for both centisecond (`.xx`) and millisecond (`.xxx`) timestamp resolutions.
* **Musixmatch**: Modern mobile API integration supporting word-level timing (**Richsync**) with compact word payloads and line-level timing (**Subtitles**).
* **Hedged Concurrent Fetching**: Races Musixmatch and LRCLIB concurrently for instant line-level display (~100–150ms) while preserving in-flight tasks to seamlessly hot-swap to word-level Richsync karaoke upon arrival.

### 🔑 3-Tier Automatic Musixmatch Token Engine
* **Zero Configuration Needed**: Automatically fetches a fresh anonymous usertoken directly from the Musixmatch mobile API if no user token is provided.
* **Dummy Token Filtering**: Automatically detects and rejects dummy all-zero tokens returned by rate-limited or blocked endpoints.
* **Token Rotation**: Supports multiple custom usertokens in `MUSIXMATCH_USERTOKEN` env var, rotated in round-robin order.
* **Auto-Renewal & Retry**: If a token encounters a rate limit (`429`) or auth error (`401`/`403`), the engine invalidates it, fetches a new token, and retries the request transparently.

### ⚡ Resilience & Background Retries
* **Transient Error Classification**: Automatically distinguishes between missing lyrics and temporary network/server outages (rate limits, DNS failures, 5xx errors).
* **Exponential Backoff Loop**: Automatically schedules background retries with exponential backoff (up to 30s cap) when providers are temporarily unreachable without interrupting active playback.

### 🏷️ Local File ISRC Extraction
* Reads local audio file metadata (FLAC, MP3, M4A, OGG) via `lofty` when `--isrc` is enabled to perform pinpoint ISRC lookups.

### 💾 Optimized SQLite Persistence
* **Zstd Compression**: Stores raw lyrics compressed with **zstd** level 3.
* **Typographic Canonicalization**: Canonicalizes smart single/double quotes, typographic dashes (en/em/horizontal), and non-breaking spaces to ASCII for resilient matching.
* **Remaster Suffix Stripping & Aliasing**: Strips remaster tags (`- 2015 Remaster`, `(Remastered)`, etc.) with 2.0s duration gating, automatically registering aliases in `track_aliases` to self-heal lookups.
* **Authoritative Identifier Caching**: Player-reported metadata (`meta.isrc`, `meta.spotify_id`, `meta.itunes_id`) is strictly used as query inputs; only verified identifiers returned directly by Musixmatch are persisted to `track_identifiers` to prevent cache pollution.

### 🚀 Ultra-Low Resource Footprint
* **Single-Threaded Async Engine**: Utilizes Tokio `current_thread` runtime and asynchronous event streaming, reducing active thread count down to just 1–2 lightweight threads.
* **Zero-Allocation Rendering**: Zero-copy span borrowing and chunk-reversed visible line collection eliminate frame-by-frame heap allocations.
* **D-Bus Query Timeouts**: Dynamic `Position` and `PlaybackStatus` queries are protected with a 150ms timeout (`DBUS_QUERY_TIMEOUT`), preventing frozen players from stalling the reactor.
* **Dual-Tier Discovery & Playback Prioritization**: Prefers `playerctld` for recency-ordered player prioritization, with an intelligent fallback (`Playing` > `Paused` > `Stopped`) via standard D-Bus discovery.
* **Real-Time Lifecycle & Cross-Player Handoff**: 100% event-driven cross-player handoff via D-Bus `NameOwnerChanged` and broker-level `PropertiesChanged` match rules.
* **Pure Event-Driven Idle & Clean Shutdown**: Consumes zero CPU and generates zero IPC traffic when idle, with clean background task termination via Tokio watch channels.

---

## 🚀 Installation

### Prerequisites
- **Linux** desktop environment with **D-Bus** session daemon.
- **Rust Toolchain** (1.70+): Install via [rustup.rs](https://rustup.rs).
- **playerctld** *(optional)*: Enhances active player tracking across desktop sessions; completely optional as priority-ranked standalone discovery is built-in.

### Build from Source
```bash
# Clone the repository
git clone https://github.com/BEST8OY/LyricsMPRIS-Rust.git
cd LyricsMPRIS-Rust

# Build release binary
cargo build --release
```

The optimized executable binary will be created at `./target/release/lyricsmpris`.

---

## 💻 Basic Usage

```bash
# Launch with default settings (Musixmatch + LRCLIB)
./target/release/lyricsmpris

# Enable local SQLite database caching
./target/release/lyricsmpris --database ~/.local/share/lyricsmpris/cache.db

# Enable ISRC extraction from local audio files via lofty
./target/release/lyricsmpris --isrc

# Enable phonetic Romanization subtitles (Romaji/Pinyin/Cyrillic)
./target/release/lyricsmpris --romanize

# Limit TUI visible lyric lines (compact view)
./target/release/lyricsmpris --visible-lines 3

# Disable word-level karaoke highlights (line-only mode)
./target/release/lyricsmpris --no-karaoke

# Run in Pipe mode for Polybar / Waybar stdout integration
./target/release/lyricsmpris --pipe

# Target specific players or block noisy apps
./target/release/lyricsmpris --target spotify,mpv --block firefox,chromium
```

---

## ⚙️ Configuration

### Command Line Arguments

| Argument | Description | Example |
| :--- | :--- | :--- |
| `--database <PATH>` | Enable SQLite cache at specified file path | `--database ~/.cache/lyrics.db` |
| `--providers <LIST>` | Comma-separated provider priority order (default: `musixmatch,lrclib`) | `--providers musixmatch,lrclib` |
| `--visible-lines <COUNT>`| Limit maximum visible lyric lines in TUI | `--visible-lines 3` |
| `--isrc` | Extract ISRC from local audio files via `lofty` | `--isrc` |
| `--romanize` | Enable phonetic Romanization subtitles (Romaji/Pinyin) | `--romanize` |
| `--no-karaoke` | Disable per-word karaoke highlighting | `--no-karaoke` |
| `--pipe` | Output lyrics to `stdout` (no TUI) | `--pipe` |
| `--block <LIST>` | Ignore specific MPRIS service names | `--block firefox,chromium` |
| `--target <LIST>` | Target specific MPRIS service names | `--target spotify,mpv` |

### Environment Variables

Configure default options via environment variables in `~/.bashrc` or `~/.zshrc`:

```bash
# Provider preference (default: musixmatch,lrclib)
export LYRIC_PROVIDERS="musixmatch,lrclib"

# Musixmatch custom usertokens (comma-separated for round-robin rotation)
export MUSIXMATCH_USERTOKEN="token_alpha,token_beta,token_gamma"

# Logging level (tracing subscriber); off by default
export RUST_LOG=info

# Log file path for TUI mode (default: /tmp/lyricsmpris.log)
export RUST_LOG_FILE=~/.local/share/lyricsmpris/debug.log
```

---

## 🎤 Musixmatch Token Management

`LyricsMPRIS-Rust` uses a **3-tier token engine** for Musixmatch:

```
┌──────────────────────────────────────────────────────────────────┐
│  Tier 1: Explicit User Tokens                                    │
│  - Environment variable: MUSIXMATCH_USERTOKEN                    │
│  - Uses round-robin rotation across multiple tokens              │
└────────────────────────────────┬─────────────────────────────────┘
                                 │ If unconfigured or all pool tokens fail (401/402/403/429)
                                 ▼
┌──────────────────────────────────────────────────────────────────┐
│  Tier 2: Automatic Token Provisioning (`token.get`)              │
│  - Automatically requests a fresh user token directly from API    │
│  - No manual setup or API keys required                          │
└────────────────────────────────┬─────────────────────────────────┘
                                 │ If dynamic token expires during request
                                 ▼
┌──────────────────────────────────────────────────────────────────┐
│  Tier 3: Automatic Token Renewal & Retry                         │
│  - Auto-fetches a new token on 401 / 402 / 403 / 429 status      │
│  - Transparently retries the current track query                 │
└──────────────────────────────────────────────────────────────────┘
```

> **Manual Token (Optional)**: If you wish to provide your own Musixmatch usertoken, set `MUSIXMATCH_USERTOKEN="your_token"`. However, manual configuration is **completely optional** because Tier 2 automatically handles token creation for you!

---

## 💾 SQLite Database Cache

Enabling `--database <PATH>` activates persistent local caching to minimize network usage and provide instant offline access.

### Database Schema

The SQLite caching engine uses a 3-table normalized relational layout:

```sql
-- Canonical lyrics table (one row per unique recording / lyrics payload)
CREATE TABLE IF NOT EXISTS lyrics (
    id INTEGER PRIMARY KEY,
    artist TEXT NOT NULL,
    title TEXT NOT NULL,
    duration REAL,
    format TEXT NOT NULL,
    raw_lyrics BLOB NOT NULL
);

-- Multi-album alias table mapping (artist, title, album) -> canonical lyrics_id
CREATE TABLE IF NOT EXISTS track_aliases (
    artist TEXT NOT NULL,
    title TEXT NOT NULL,
    album TEXT NOT NULL,
    lyrics_id INTEGER NOT NULL REFERENCES lyrics(id) ON DELETE CASCADE,
    PRIMARY KEY (artist, title, album)
) WITHOUT ROWID;

-- Unified identifier table mapping (kind, value) -> unique canonical lyrics_id
CREATE TABLE IF NOT EXISTS track_identifiers (
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    lyrics_id INTEGER NOT NULL REFERENCES lyrics(id) ON DELETE CASCADE,
    ordering INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (kind, value)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_track_identifiers_lyrics_id_ordering
ON track_identifiers(lyrics_id, ordering);
```

### 🔒 Identifier Separation: Querying vs. Caching

`LyricsMPRIS-Rust` enforces strict separation between player-reported metadata and persistent cached identifiers:
* **Query Inputs Only**: Player-reported IDs (`xesam:url`, `mpris:trackid`) and local audio file tags (when `--isrc` is enabled) are used solely as search query inputs to look up existing cache entries or probe external APIs.
* **No Cache Pollution**: To prevent incorrect, mislabeled, or spoofed user tags from corrupting the database, player-reported IDs are **never** stored into `track_identifiers`.
* **Authoritative Musixmatch Caching**: Only authoritative identifiers returned directly by the Musixmatch API (`track_isrc`, `track_spotify_id`, `track_itunes_id`) are persisted into `track_identifiers`.

### Lookup Resolution Priority

When looking up a track in the cache, queries execute in the following order:

1. **ISRC Match** (`track_identifiers` where `kind = 'isrc'`) — Authoritative unique track identifier (punctuation-agnostic normalization).
2. **Exact Track Alias Match** (`track_aliases` where `artist = ? AND title = ? AND album = ?`) — Typographically normalized multi-album mapping.
3. **Spotify ID Match** (`track_identifiers` where `kind = 'spotify'`) — Fallback via Spotify track ID.
4. **iTunes ID Match** (`track_identifiers` where `kind = 'itunes'`) — Fallback via iTunes track ID.
5. **Remaster Suffix Strip & Bidirectional Alias** — Automatically matches clean titles against remastered variants (`- 2015 Remaster`, `(Remastered)`, etc.) within 2.0s duration tolerance, self-healing new entries into `track_aliases`.
6. **Fuzzy Track Match** (`track_aliases` where `artist = ? AND title = ?`) — Resolves matches across compilations or varying album tags.

### Compression, Fidelity & Concurrency

- **Zstd Compression**: Raw lyrics payloads are stored as zstd-compressed blobs.
- **Typographic Canonicalization**: Smart quotes (`’`, `“`), typographic dashes (`–`, `—`), and non-breaking spaces are canonicalized to standard ASCII for all string lookups.
- **Duration Tolerance**: Clamped within a 2.0s tolerance window; duration-mismatched entries are retained (multi-version matching for live vs. studio recordings); only corrupt or unparseable payloads are purged.
- **Format Fidelity Ranking**: Richsync (word-level, rank 3) > Subtitles (line-level, rank 2) > LRCLIB (LRC, rank 1). Incoming lower-fidelity fetches never downgrade higher-fidelity cached lyrics.
- **Writer Concurrency**: Uses WAL journal mode with `SqliteSynchronous::Normal`, `BEGIN IMMEDIATE`, and a 5-second busy timeout to serialize concurrent database transactions without lock contention.

---

## ⌨️ Keyboard Shortcuts

Control the modern TUI backend with these keys:

| Key | Action |
| :---: | :--- |
| `k` | Toggle Karaoke Highlight Mode |
| `r` | Toggle Phonetic Romanization Subtitles |
| `↑` | Scroll up one line (when player is paused) |
| `↓` | Scroll down one line (when player is paused) |
| `q` / `Esc` / `Ctrl+C` | Quit `LyricsMPRIS-Rust` |

> [!NOTE]
> Manual line scrolling is active only when playback is **paused**. Once playback resumes, the TUI automatically locks back to the current playing position.

---

## 🔌 Desktop Integration

Pipe mode (`--pipe`) outputs plain text lyric lines to `stdout`, making it easy to display current lyrics on desktop panels:

- **Clean Streams**: Emits active lyrics on `stdout` while routing transient errors and warnings to `stderr`.
- **Seek & Boundary Clearing**: Automatically outputs an empty line during instrumental intros, outros, or seeks beyond lyric boundaries to clear panel displays.

### Polybar Integration
```ini
[module/lyrics]
type = custom/script
exec = lyricsmpris --pipe
tail = true
format-prefix = "🎵 "
format-prefix-foreground = #8abeb7
```

### Waybar Integration
```json
"custom/lyrics": {
    "format": "🎵 {}",
    "exec": "lyricsmpris --pipe",
    "restart-interval": 5,
    "tooltip": false
}
```

---

## 🏗️ Project Architecture

```
src/
├── main.rs           # Entry point, CLI argument parsing (clap), current_thread runtime
├── event/            # Event coordinator (fetch pipeline, MPRIS handlers, retry backoff)
├── pool.rs           # Event loop supervisor (wires D-Bus → state → UI)
├── state.rs          # StateBundle, PlayerState, LyricState, Update snapshot
├── timer.rs          # PlaybackTimer (monotonic position estimation)
├── text_utils.rs     # Text wrapping utility
├── lyrics/
│   ├── providers/    # lrclib.rs, musixmatch/ (API integrations & token manager)
│   ├── database/     # SQLite cache engine with sqlx + Zstd compression
│   ├── parse.rs      # LRC (centisecond & millisecond), Richsync, subtitle parsers
│   ├── romanize/     # Multi-script romanization engine (kakasi Japanese Kana, deunicode)
│   └── types.rs      # LyricLine, TrackMatchInfo, error models
├── mpris/
│   ├── connection.rs # D-Bus session singleton, PlaybackPriority, discovery & filtering
│   ├── events.rs     # D-Bus signal listener, broker-level match rules & calibration
│   ├── metadata.rs   # MPRIS track metadata extraction & local file ISRC (lofty)
│   ├── playback.rs   # Direct D-Bus playback position & status querying
│   └── proxies.rs    # Consolidated D-Bus proxy interfaces
└── ui/
    ├── modern.rs     # Modern Ratatui TUI renderer with RAII terminal lifecycle
    ├── modern_helpers.rs # Optimized zero-copy line styling and layout calculation
    ├── pipe.rs       # Clean stdout/stderr pipe mode renderer
    └── ...           # Helpers (styles, progression, wrapping cache)
```

---

## 🐛 Troubleshooting & Debugging

1. **No Lyrics Displayed / Debugging**:
   Enable `RUST_LOG=debug` to get detailed D-Bus events and API logs.

   - **TUI mode**: logs are written to a file (default `/tmp/lyricsmpris.log`) to avoid corrupting the terminal UI. Watch them in a second terminal:
     ```bash
     RUST_LOG=debug ./lyricsmpris
     tail -f /tmp/lyricsmpris.log
     ```
     Override the log path with `RUST_LOG_FILE`:
     ```bash
     RUST_LOG=debug RUST_LOG_FILE=~/lyricsmpris.log ./lyricsmpris
     ```
   - **Pipe mode**: logs go to `stderr` as usual:
     ```bash
     RUST_LOG=debug ./lyricsmpris --pipe 2>&1 | grep musixmatch
     ```

2. **Player Not Detected**:
   Verify your media player supports MPRIS via D-Bus:
   ```bash
   playerctl -l
   ```
   `LyricsMPRIS-Rust` works completely standalone with priority-ranked fallback and broker-level event handoff. Installing `playerctld` is optional and can provide additional cross-session recency ordering.

---

## 🤝 Contributing

Contributions are welcome!

```bash
# Check code formatting
cargo fmt --check

# Run linter checks
cargo clippy

# Run unit test suite
cargo test
```

---

**Made with ❤️ for the Linux audio community.**
