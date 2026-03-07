 # LyricsMPRIS-Rust

A lightweight, high-performance lyrics viewer for Linux that integrates seamlessly with MPRIS-compatible media players. Features real-time synchronized lyrics with optional karaoke-style word highlighting, local caching, and multiple provider support.

![LyricMPRIS-Rust](https://github.com/user-attachments/assets/501f224e-6c40-46cd-ac66-cd9ae4f927cf)

## ✨ Features

### Display Modes
- **🎨 Modern TUI**: Beautiful terminal interface with centered lyrics and smooth scrolling
  - **Compact View**: Limit visible lyrics with `--visible-lines` for small terminals
  - **Manual Scrolling**: Browse lyrics with arrow keys when paused
- **🔧 Pipe Mode**: Stream current lyrics to stdout for integration with status bars and scripts
- **🎤 Karaoke Mode**: Per-word highlighting synchronized with playback (Musixmatch Richsync)

### Lyrics Sources
- **📚 LRCLIB**: Community-maintained database (returns LRC timestamp format)
- **🎵 Musixmatch**: Professional lyrics with word-level/line-level timing (JSON formats)
- **🔄 Configurable Priority**: Set your preferred provider order
- **💾 Local Cache**: Optional database for offline access and reduced API calls


> [!NOTE]
> **Terminology**: "LRCLIB" refers to the lrclib.net provider service, while "LRC format" refers to the timestamp standard (`[MM:SS.CC]lyrics`) that LRCLIB returns.
>
> Musixmatch returns different JSON-based formats (Richsync/Subtitles).

### Player Integration
- **🎧 MPRIS Support**: Works with any MPRIS-compatible player (Spotify, VLC, mpv, etc.)
- 🎯 **Targets**: Target specific players to avoid player switching
- **🚫 Blocklist**: Exclude specific players from monitoring
- **⚡ Event-Driven**: Efficient architecture with zero polling overhead

## 🚀 Quick Start

### Prerequisites

- **Rust toolchain** (1.70+): Install from [rustup.rs](https://rustup.rs)
- **Linux** with D-Bus support
- **MPRIS-compatible media player**
- **playerctld**

### Installation

```bash
# Clone the repository
git clone https://github.com/BEST8OY/LyricsMPRIS-Rust.git
cd LyricsMPRIS-Rust

# Build release version
cargo build --release

# Binary will be at: ./target/release/lyricsmpris
```

### Basic Usage

```bash
# Launch with default settings
./target/release/lyricsmpris

# With local cache for faster loading
./target/release/lyricsmpris --database ~/.local/share/lyricsmpris/cache.db

# Disable karaoke highlighting
./target/release/lyricsmpris --no-karaoke

# Limit visible lyrics to 3 lines (compact mode)
./target/release/lyricsmpris --visible-lines 3

# Pipe mode for scripting
./target/release/lyricsmpris --pipe
```

## ⚙️ Configuration

### Command Line Options
> [!NOTE]
> Use `playerctl --list-all` to view MPRIS services

| Flag | Description | Example |
|------|-------------|---------|
| `--database PATH` | Enable SQLite lyrics cache | `--database ~/.local/share/lyricsmpris/cache.db` |
| `--providers LIST` | Set provider priority | `--providers musixmatch,lrclib` |
| `--visible-lines COUNT` | Limit visible lyric blocks (TUI only) | `--visible-lines 3` |
| `--no-karaoke` | Disable word-level highlighting | - |
| `--pipe` | Output to stdout instead of TUI | - |
| `--block LIST` | Ignore specific MPRIS services | `--block vlc,chromium` |
| `--target LIST` | Listen to specific MPRIS services | `--target spotify,mpv` |

### Environment Variables

```bash
# Musixmatch user token (required for Musixmatch provider)
export MUSIXMATCH_USERTOKEN="your-token-here"

# Logging configuration (uses tracing crate)
# Levels: error, warn, info, debug, trace
# Logs are OFF by default. Set RUST_LOG to enable:
export RUST_LOG=warn                    # Show warnings and errors
export RUST_LOG=info                    # Show info, warnings and errors  
export RUST_LOG=debug                   # Show debug logs
export RUST_LOG=lyricsmpris::lyrics=trace  # Trace specific module
```

### Default provider list (if --providers not specified)
```
export LYRIC_PROVIDERS="lrclib,musixmatch"
```

### Getting a Musixmatch Token

**Method 1: Curators Settings (Easiest)**

1. Go to the [Musixmatch Curators Settings](https://curators.musixmatch.com/settings) page
2. Login if prompted
3. Scroll down to the bottom of the page
4. Click **"Copy debug info"**
5. Paste the debug info into a text editor
6. Find the `UserToken` in the copied text
7. Copy that token and set it as `MUSIXMATCH_USERTOKEN`

### TUI Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `k` | Toggle karaoke highlighting |
| `↑` (Up) | Scroll up one lyric (when paused) |
| `↓` (Down) | Scroll down one lyric (when paused) |
| `q` or `Esc` | Quit application |

> [!NOTE]
> Scrolling with arrow keys only works when playback is paused. When you resume playback, the view automatically resets to follow the current position.

## 💾 Local Database

The database feature provides persistent SQLite-based lyrics caching for improved performance and offline access.

### Setup

```bash
# Create cache directory
mkdir -p ~/.local/share/lyricsmpris

# Run with database enabled
lyricsmpris --database ~/.local/share/lyricsmpris/cache.db
```

### How It Works

1. **First Play**: Lyrics fetched from providers → stored in SQLite database
2. **Subsequent Plays**: Lyrics loaded instantly from indexed database (no API calls)
3. **Auto-Persist**: Database automatically commits to disk after each fetch

### Storage Format

The database uses **SQLite** with indexed lookups for efficient storage and retrieval:

#### Schema

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

`raw_lyrics` is stored as a **Zstd-compressed** blob. It is transparently decompressed when reading from the cache.

#### Format Types

Lyrics are stored in their original format by provider:

- **`lrclib`**: LRC timestamp format (`[MM:SS.CC]lyrics text`)
- **`richsync`**: Musixmatch JSON with word-level timing data
- **`subtitles`**: Musixmatch JSON with line-level timing data

#### Example Stored Entry

| artist | title | album | duration | format | raw_lyrics |
|--------|-------|-------|----------|--------|------------|
| arctic monkeys | do i wanna know? | am | 272.0 | richsync | `[{"ts":29.26,"te":31.597,...}]` |
> [!NOTE]
> `raw_lyrics` is shown above **decompressed** for readability.
> 
> Artist, title, and album are normalized (lowercase, trimmed) for case-insensitive matching.

### Benefits

- ⚡ **Instant Loading**: Indexed lookups provide sub-millisecond retrieval
- 🌐 **Offline Mode**: No internet required for cached songs
- 📉 **Reduced API Calls**: Be kind to provider rate limits
- 💪 **Provider Independence**: Lyrics persist even if APIs change
- 🧠 **Minimal Memory**: SQLite loads only requested rows, not entire database
- 🔍 **Fast Queries**: Indexed by artist/title/album for efficient lookups
- 🔄 **WAL Mode**: Write-Ahead Logging for better concurrency

## 🔌 MPRIS Integration

### Supported Players

Any MPRIS-compatible player works, including:
- Spotify (official client)
- Spotify (spotifyd, spotify-tui)
- VLC Media Player
- mpv
- Audacious
- Clementine
- Rhythmbox
- And many more...

### Player Blocklist

Ignore specific players if needed:

```bash
# Block web browsers and unwanted players
lyricsmpris --block chromium,firefox
```

## 🔧 Advanced Usage

### Integration with Status Bars

```bash
# Polybar module example
[module/lyrics]
type = custom/script
exec = ~/bin/lyricsmpris --pipe
tail = true
```

```bash
# Waybar module example
"custom/lyrics": {
  "exec": "lyricsmpris --pipe",
  "return-type": "text",
}
```

## 🏗️ Architecture

### Design Principles

- **Event-Driven**: No polling, minimal CPU usage
- **Zero-Copy**: Efficient Arc-based state sharing
- **Async First**: Tokio-powered concurrent operations
- **Type Safety**: Leverages Rust's type system for correctness

### Module Overview

```
src/
├── lyrics/          # Lyrics providers and parsing
│   ├── providers/   # LRCLIB, Musixmatch implementations
│   ├── database.rs  # Local cache management
│   ├── parse.rs     # LRCLIB, Richsync, Subtitle parsers
│   └── similarity.rs # Fuzzy matching for search results
├── mpris/           # D-Bus/MPRIS integration
│   ├── events.rs    # Signal handler for player changes
│   ├── metadata.rs  # Track info extraction
│   └── playback.rs  # Position tracking
├── ui/              # Display backends
│   ├── modern.rs    # TUI implementation
│   └── pipe.rs      # Stdout mode
├── event.rs         # Event processing and coordination
├── pool.rs          # Event loop management
└── state.rs         # Shared application state
```

## 🐛 Troubleshooting

### No Lyrics Found

1. **Check provider order**: Try `--providers musixmatch,lrclib`
2. **Verify Musixmatch token**: Ensure `MUSIXMATCH_USERTOKEN` is set
3. **Enable debug logging**: Use `RUST_LOG=debug` to see detailed logs
4. **Check metadata**: Some players may not provide complete track info

### Debugging

Use the `RUST_LOG` environment variable for diagnostics:

```bash
# Show all debug information
RUST_LOG=debug lyricsmpris

# Only show errors
RUST_LOG=error lyricsmpris

# Debug specific components
RUST_LOG=lyricsmpris::lyrics=debug lyricsmpris
RUST_LOG=lyricsmpris::mpris=trace lyricsmpris

# Multiple modules with different levels
RUST_LOG=lyricsmpris::lyrics=debug,lyricsmpris::database=trace lyricsmpris

# Save logs to file
RUST_LOG=debug lyricsmpris 2> debug.log
```

### Performance Issues

1. **Enable database**: Use `--database` to reduce API latency
2. **Limit providers**: Specify only needed providers with `--providers`
3. **Check player**: Some MPRIS implementations send excessive updates

### Karaoke Not Working

1. **Provider limitation**: Only Musixmatch Richsync supports word-level timing
2. **Track availability**: Not all songs have Richsync data
3. **Fallback**: App will show line-level sync if Richsync unavailable

## 🤝 Contributing

Contributions are welcome! Please:

1. **Fork** the repository
2. **Create** a feature branch (`git checkout -b feature/amazing-feature`)
3. **Test** thoroughly (both TUI and pipe modes)
4. **Commit** with clear messages (`git commit -m 'Add amazing feature'`)
5. **Push** to your fork (`git push origin feature/amazing-feature`)
6. **Open** a Pull Request

### Development Setup

```bash
# Run in debug mode
cargo run

# Run with debug logging
RUST_LOG=debug cargo run

# Run with trace logging for specific module
RUST_LOG=lyricsmpris::lyrics=trace cargo run

# Run tests
cargo test

# Check code quality
cargo clippy
cargo fmt --check
```

## 📜 License

See the [LICENSE](LICENSE) file for details.

## 🙏 Acknowledgements

- **Community**: Thanks to all contributors and users
- **Dependencies**: Built with excellent Rust crates (see [Cargo.toml](Cargo.toml))
- **Providers**: LRCLIB and Musixmatch for lyrics data
- **Development**: Created with VS Code and GitHub Copilot assistance

## 📊 Project Stats

- **Language**: Rust 🦀
- **Architecture**: Event-driven, async/await
- **Binary Size**: ~15MB (release, stripped)
- **Memory Usage**: ~20MB typical
- **CPU Usage**: ~0% typical
- **Dependencies**: Minimal, security-conscious selection

---

**Made with ❤️ for the Linux audio community**

