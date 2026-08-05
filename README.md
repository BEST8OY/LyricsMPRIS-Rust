# 🎵 LyricsMPRIS-Rust

A lightweight, ultra-high-performance synchronized lyrics viewer for Linux that integrates seamlessly with MPRIS-compatible media players.

[![Language](https://img.shields.io/badge/Language-Rust-orange.svg?style=flat-sq&logo=rust)](https://www.rust-lang.org/)
[![Edition](https://img.shields.io/badge/Edition-2024-red.svg?style=flat-sq)](https://doc.rust-lang.org/edition-guide/rust-2024/)
[![Platform](https://img.shields.io/badge/Platform-Linux-blue.svg?style=flat-sq&logo=linux)](https://kernel.org)
[![License](https://img.shields.io/badge/License-MIT-green.svg?style=flat-sq)](LICENSE)

`LyricsMPRIS-Rust` monitors your active media player in real-time using D-Bus, fetches synchronized lyrics automatically, and renders them beautifully in a modern Terminal User Interface (TUI) with word-level karaoke support. It also features a scriptable **Pipe Mode** for status bar integration, **ISRC tag reading** from local audio files, and an optimized **local SQLite database** with Zstd compression for instant offline lookups.

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
* **Ratatui Terminal UI**: Centered lyrics display with smooth auto-scrolling, word-by-word karaoke highlighting, and active line tracking.
* **Manual Scroll**: Browse past or upcoming lyrics using arrow keys (`j`/`k`) when playback is paused.
* **Streamlined Pipe Mode**: Outputs current lyric line to `stdout` in real-time, making it effortless to pipe into Polybar, Waybar, i3blocks, or custom scripts.

### 📚 Multiple Lyrics Providers
* **LRCLIB**: Fetches community-sourced, line-synced lyrics (`LRC` format).
* **Musixmatch**: Official desktop API integration supporting both word-level timing (**Richsync**) and line-level timing (**Subtitles**).

### 🔑 3-Tier Automatic Musixmatch Token Engine
* **Zero Configuration Needed**: Automatically fetches a fresh anonymous usertoken directly from Musixmatch API if no user token is provided.
* **Token Rotation**: Supports multiple custom usertokens in `MUSIXMATCH_USERTOKEN` env var, rotated in round-robin order.
* **Auto-Renewal & Retry**: If a token encounters a rate limit (`429`) or auth error (`401`/`403`), the engine invalidates it, fetches a new token, and retries the request transparently.

### 🏷️ Local File ISRC Extraction
* Reads local audio file metadata (FLAC, MP3, M4A, OGG) via `lofty` when `--isrc` is enabled to perform pinpoint ISRC lookups.

### 💾 Optimized SQLite Persistence
* Stores raw lyrics compressed with **zstd** level 3.
* Multi-identifier indexing (ISRC, Spotify ID, iTunes ID, Artist/Title/Album) ensures instant offline hits without redundant network calls.

---

## 🚀 Installation

### Prerequisites
- **Linux** desktop environment with **D-Bus** session daemon.
- **Rust Toolchain** (1.70+): Install via [rustup.rs](https://rustup.rs).
- **playerctld** *(recommended)*: Tracks active MPRIS players automatically.

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
# Launch with default settings (LRCLIB + Musixmatch)
./target/release/lyricsmpris

# Enable local SQLite database caching
./target/release/lyricsmpris --database ~/.local/share/lyricsmpris/cache.db

# Enable ISRC extraction from local audio files
./target/release/lyricsmpris --isrc

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
| `-d`, `--database <PATH>` | Enable SQLite cache at specified file path | `--database ~/.cache/lyrics.db` |
| `-p`, `--providers <LIST>` | Comma-separated provider priority order | `--providers musixmatch,lrclib` |
| `-v`, `--visible-lines <NUM>`| Limit maximum visible lyric lines in TUI | `--visible-lines 3` |
| `--isrc` | Extract ISRC from local audio files via `lofty` | `--isrc` |
| `--no-karaoke` | Disable per-word karaoke highlighting | `--no-karaoke` |
| `--pipe` | Output lyrics to `stdout` (no TUI) | `--pipe` |
| `-b`, `--block <LIST>` | Ignore specific MPRIS service names | `--block firefox,chromium` |
| `--target <LIST>` | Target specific MPRIS service names | `--target spotify,mpv` |

### Environment Variables

Configure default options via environment variables in `~/.bashrc` or `~/.zshrc`:

```bash
# Provider preference (default: lrclib,musixmatch)
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

```sql
CREATE TABLE IF NOT EXISTS lyrics (
    artist TEXT NOT NULL,
    title TEXT NOT NULL,
    album TEXT NOT NULL,
    duration REAL,
    format TEXT NOT NULL,
    raw_lyrics BLOB NOT NULL,
    isrc TEXT,
    spotify_id TEXT,
    itunes_id TEXT,
    PRIMARY KEY (artist, title, album)
);

CREATE INDEX IF NOT EXISTS idx_lookup ON lyrics(artist, title, album);
CREATE INDEX IF NOT EXISTS idx_isrc ON lyrics(isrc);
CREATE INDEX IF NOT EXISTS idx_spotify_id ON lyrics(spotify_id);
CREATE INDEX IF NOT EXISTS idx_itunes_id ON lyrics(itunes_id);
```

### Lookup Resolution Priority

When looking up a track in the cache, queries execute in the following order:

1. **ISRC Match** (`WHERE isrc = ?`) — Most accurate unique track identifier.
2. **Normalized Metadata Match** (`WHERE artist = ? AND title = ? AND album = ?`) — Case-insensitive fallback.
3. **Spotify ID Match** (`WHERE spotify_id = ?`) — Fallback via Spotify track ID.
4. **iTunes ID Match** (`WHERE itunes_id = ?`) — Fallback via iTunes track ID.

### Compression & Validation
- **Zstd Level 3**: Raw lyrics payloads (LRC text / Richsync JSON) are compressed before storage to save space.
- **Duration Tolerance**: Cached entries validate playback duration within a 5% tolerance window; invalid or outdated entries are purged automatically.

---

## ⌨️ Keyboard Shortcuts

Control the modern TUI backend with these keys:

| Key | Action |
| :---: | :--- |
| `k` | Toggle Karaoke Highlight Mode |
| `↑` / `k` | Scroll up one line (when player is paused) |
| `↓` / `j` | Scroll down one line (when player is paused) |
| `q` / `Esc` | Quit `LyricsMPRIS-Rust` |

> [!NOTE]
> Manual line scrolling is active only when playback is **paused**. Once playback resumes, the TUI automatically locks back to the current playing position.

---

## 🔌 Desktop Integration

Pipe mode (`--pipe`) outputs plain text lyric lines to `stdout`, making it easy to display current lyrics on desktop panels.

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
├── main.rs           # Entry point, CLI argument parsing (clap), tokio runtime
├── event.rs          # Event coordinator (track changes, seeks, provider dispatch)
├── pool.rs           # Event loop supervisor (wires D-Bus → state → UI)
├── state.rs          # StateBundle, PlayerState, LyricState, Update snapshot
├── timer.rs          # PlaybackTimer (monotonic position estimation)
├── text_utils.rs     # Text wrapping utility
├── lyrics/
│   ├── providers/    # lrclib.rs, musixmatch.rs (API integrations & token manager)
│   ├── database.rs   # SQLite cache engine with sqlx + Zstd compression
│   ├── parse.rs      # LRC, Richsync, subtitle parsers
│   ├── similarity.rs # Fuzzy song matching
│   └── types.rs      # LyricLine, TrackMatchInfo, error models
├── mpris/
│   ├── connection.rs # D-Bus session singleton
│   ├── events.rs     # D-Bus signal listener (PropertiesChanged)
│   ├── metadata.rs   # MPRIS track metadata extraction & local file ISRC (lofty)
│   └── playback.rs   # Playback position & status querying
└── ui/
    ├── modern.rs     # Modern Ratatui TUI renderer
    ├── pipe.rs       # Stdout pipe mode renderer
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
   Installing `playerctld` ensures `lyricsmpris` tracks your active player reliably.

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
