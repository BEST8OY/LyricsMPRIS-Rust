# 🎵 LyricsMPRIS-Rust

A lightweight, ultra-high-performance synchronized lyrics viewer for Linux that integrates seamlessly with MPRIS-compatible media players.

[![Language](https://img.shields.io/badge/Language-Rust-orange.svg?style=flat-sq&logo=rust)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Linux-blue.svg?style=flat-sq&logo=linux)](https://kernel.org)
[![License](https://img.shields.io/badge/License-MIT-green.svg?style=flat-sq)](LICENSE)
[![Build Status](https://img.shields.io/badge/Build-Passing-brightgreen.svg?style=flat-sq)]()

`LyricsMPRIS-Rust` monitors your active media player in real-time using D-Bus, fetches synchronized lyrics automatically, and renders them beautifully in a modern Terminal User Interface (TUI). It also features a scriptable **Pipe Mode** for custom desktop widgets, standard **LRC file parsing**, and a highly optimized **local SQLite database** with Zstd compression for instant offline lookups.

![LyricMPRIS-Rust](https://github.com/user-attachments/assets/501f224e-6c40-46cd-ac66-cd9ae4f927cf)

---

## 📖 Table of Contents

1. [🌟 Key Features](#-key-features)
2. [🚀 Installation](#-installation)
3. [💻 Basic Usage](#-basic-usage)
4. [⚙️ Configuration](#-configuration)
5. [🎤 Musixmatch Token Rotation](#-musixmatch-token-rotation)
6. [⌨️ Keyboard Shortcuts](#%EF%B8%8F-keyboard-shortcuts)
7. [💾 SQLite Database Cache](#-sqlite-database-cache)
8. [🔌 Desktop Integration](#-desktop-integration)
9. [🏗️ Project Architecture](#%EF%B8%8F-project-architecture)
10. [🐛 Troubleshooting & Debugging](#-troubleshooting--debugging)
11. [🤝 Contributing](#-contributing)

---

## 🌟 Key Features

### 🎨 Rendering Backend Options
*   **Modern TUI**: A beautiful terminal view featuring centered lyrics, smooth auto-scrolling, customizable visibility limits, and active line tracking.
*   **Manual Scroll**: Interact with historical or upcoming lyrics by scrolling using the arrow keys when playback is paused.
*   **Streamlined Pipe Mode**: Outputs raw lyrics to standard output (`stdout`) in real-time, making it trivial to pipe into status bars, notification systems, or external scripts.

### 📚 Multiple Lyrics Providers
*   **LRCLIB**: Automatically fetches community-sourced, line-synced lyrics (`LRC` format).
*   **Musixmatch**: Fetches official subtitles with support for word-level timing (Richsync JSON) and line-level timing.
*   **Local SQLite Cache**: Saves fetched lyrics locally so subsequent plays load in sub-milliseconds without firing redundant API queries.

### 🛡️ Robust Token Manager
*   **Round-Robin Rotation**: Distributes queries across multiple Musixmatch user tokens to bypass rate limits.
*   **Automatic Fallback Retry**: If a token fails due to a rate limit (`429`) or unauthorized/expired status (`401`/`403`), the application automatically attempts to resolve your request with the next configured token.

---

## 🚀 Installation

### Prerequisites
Make sure your Linux environment has:
- **Rust Toolchain** (1.70+): Install via [rustup.rs](https://rustup.rs)
- **D-Bus** development libraries (usually pre-installed on most modern Linux desktop systems)
- **playerctld** (recommended for tracking the active MPRIS target player)

### Build from Source
```bash
# Clone the repository
git clone https://github.com/BEST8OY/LyricsMPRIS-Rust.git
cd LyricsMPRIS-Rust

# Build and optimize for release
cargo build --release
```
The optimized executable binary will be available at `./target/release/lyricsmpris`.

---

## 💻 Basic Usage

```bash
# Run the application with default settings
./target/release/lyricsmpris

# Run with local database caching enabled
./target/release/lyricsmpris --database ~/.local/share/lyricsmpris/cache.db

# Disable word-level karaoke highlight mode
./target/release/lyricsmpris --no-karaoke

# Run in compact TUI mode (limits visible lines to 3)
./target/release/lyricsmpris --visible-lines 3

# Pipe lyrics to stdout for scripting/bar integration
./target/release/lyricsmpris --pipe
```

---

## ⚙️ Configuration

### Command Line Arguments

| Command-line Argument | Description | Example |
| :--- | :--- | :--- |
| `-d`, `--database <PATH>` | Paths to SQLite local cache file | `--database ~/.cache/lyrics.db` |
| `-p`, `--providers <LIST>` | Comma-separated list to prioritize providers | `--providers musixmatch,lrclib` |
| `-v`, `--visible-lines <NUM>` | Limits visible TUI lyric lines (compact view) | `--visible-lines 3` |
| `--no-karaoke` | Disables word-by-word highlighted playback | `--no-karaoke` |
| `--pipe` | Launches in CLI pipe mode instead of TUI | `--pipe` |
| `-b`, `--block <LIST>` | Comma-separated player names to ignore | `--block chromium,firefox` |
| `--target <LIST>` | Listen only to specific MPRIS services | `--target spotify,mpv` |

### Environment Variables
Configure default parameters and authentication credentials in your shell startup file (e.g., `~/.bashrc` or `~/.zshrc`):

```bash
# Prioritize and enable providers (default is lrclib,musixmatch)
export LYRIC_PROVIDERS="lrclib,musixmatch"

# Configure Musixmatch User Tokens (supports single or comma-separated lists)
export MUSIXMATCH_USERTOKEN="token_alpha,token_beta,token_gamma"

# Logging configuration (uses the tracing framework)
# Levels: error, warn, info, debug, trace
export RUST_LOG=info
```

---

## 🎤 Musixmatch Token Rotation

The Musixmatch provider utilizes a client-side usertoken. To minimize rate limit errors and scale requests, `LyricsMPRIS-Rust` supports multiple user tokens.

### How to Retrieve a Musixmatch Token
1. Open the [Musixmatch Curators Settings](https://curators.musixmatch.com/settings) and log in.
2. Scroll to the bottom of the page and click **"Copy debug info"**.
3. Paste the contents in a text editor and copy the `UserToken` value.
4. Set it as `MUSIXMATCH_USERTOKEN`.

### Rotation and Fallback Algorithm
```bash
# Add multiple tokens separated by commas
export MUSIXMATCH_USERTOKEN="token1,token2,token3"
```
The internal token manager works in a round-robin rotation:
1. **First request** queries using `token1`.
2. **Second request** queries using `token2`.
3. If `token2` hits a rate limit (`429`) or is invalid (`401`/`403`), the engine automatically falls back to `token3` to satisfy the current request seamlessly.
4. If a track is genuinely not found (returns `200 OK` but empty lyrics), the app halts searching immediately, avoiding redundant API calls.

---

## ⌨️ Keyboard Shortcuts

Interact with the modern TUI backend using these keys:

| Key | Action |
| :---: | :--- |
| `k` | Toggle Karaoke Highlight Mode |
| `↑` / `k` | Scroll up by one line (when player is paused) |
| `↓` / `j` | Scroll down by one line (when player is paused) |
| `q` / `Esc` | Safely exit the application |

> [!NOTE]
> Scrolling is only enabled while playback is paused. Once playback resumes, the interface snaps back to follow the active playing position.

---

## 💾 SQLite Database Cache

Enable the local database cache to prevent redundant API queries, ensure lightning-fast retrieval, and enable offline usage.

### Schema Details
```sql
CREATE TABLE lyrics (
    artist TEXT NOT NULL,
    title TEXT NOT NULL,
    album TEXT NOT NULL,
    duration REAL,
    format TEXT NOT NULL,
    raw_lyrics BLOB NOT NULL
);

CREATE INDEX idx_lookup ON lyrics(artist, title, album);
```

### Storage Efficiency
- **Compression**: The raw lyrics data is compressed using the **Zstd** format before storing in the `raw_lyrics` BLOB to save disk space.
- **Normalization**: Fields `artist`, `title`, and `album` are normalized (lowercase and trimmed) to make matching case-insensitive.
- **Speed**: Indexed database queries offer sub-millisecond retrieval.

---

## 🔌 Desktop Integration

Pipe mode streams current lyrics directly to standard output, making it easy to create widgets for panels like Polybar, Waybar, or i3blocks.

### Polybar Configuration Example
```ini
[module/lyrics]
type = custom/script
exec = lyricsmpris --pipe
tail = true
format-prefix = "🎵 "
format-prefix-foreground = #8abeb7
```

### Waybar Configuration Example
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
├── lyrics/             # Core lyrics module
│   ├── providers/      # Integration with LRCLIB & Musixmatch APIs
│   ├── database.rs     # SQLite caching engine & Zstd compression
│   ├── parse.rs        # LRC, Richsync, and line Subtitle parsers
│   ├── similarity.rs   # Fuzzy string matching logic
│   └── types.rs        # Lyric data models & Error types
├── mpris/              # MPRIS D-Bus interfaces
│   ├── events.rs       # D-Bus player signals
│   ├── metadata.rs     # Metadata extraction
│   └── playback.rs     # Player controls & position retrieval
├── ui/                 # Frontend interfaces
│   ├── modern.rs       # Modern Ratatui-based TUI
│   └── pipe.rs         # Stdout pipe mode
├── event.rs            # Main event loop and coordinator
├── pool.rs             # Thread/process state supervisor
└── state.rs            # Atomic state bundles
```

---

## 🐛 Troubleshooting & Debugging

If you encounter issues or no lyrics are displayed:
1. **Enable Debug Mode**: Launch the application with `RUST_LOG=debug` to view detailed trace information:
   ```bash
   RUST_LOG=debug lyricsmpris
   ```
2. **Check Player Metadata**: Make sure your target media player correctly registers on MPRIS and exports title and artist metadata.
3. **Verify Token Validity**: Test your Musixmatch user tokens manually if you consistently receive warning logs.

---

## 🤝 Contributing

We welcome community contributions!
1. Fork this repository.
2. Create your feature branch (`git checkout -b feature/amazing-feature`).
3. Ensure formatting is correct (`cargo fmt --check`) and lints pass (`cargo clippy`).
4. Commit your changes and open a Pull Request.

---

**Made with ❤️ for the Linux audio community.**
