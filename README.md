 # LyricsMPRIS-Rust

A lightweight, high-performance lyrics viewer for Linux that integrates seamlessly with MPRIS-compatible media players. Features real-time synchronized lyrics with optional karaoke-style word highlighting, local caching, and multiple provider support.

## ✨ Features

### Display Modes
- **🎨 Modern TUI**: Beautiful terminal interface with centered lyrics and smooth scrolling
- **🔧 Pipe Mode**: Stream current lyrics to stdout for integration with status bars and scripts
- **🎤 Karaoke Mode**: Per-word highlighting synchronized with playback (Musixmatch Richsync)

### Lyrics Sources
- **📚 LRCLIB**: Community-maintained database (returns LRC timestamp format)
- **🎵 Musixmatch**: Professional lyrics with word-level timing (JSON formats)
- **🔄 Configurable Priority**: Set your preferred provider order
- **💾 Local Cache**: Optional database for offline access and reduced API calls

> **Note on Terminology**: "LRCLIB" refers to the lrclib.net provider service, while "LRC format" refers to the timestamp standard (`[MM:SS.CC]lyrics`) that LRCLIB returns. Musixmatch returns different JSON-based formats (Richsync/Subtitles).

### Player Integration
- **🎧 MPRIS Support**: Works with any MPRIS-compatible player (Spotify, VLC, mpv, etc.)
- **🚫 Blocklist**: Exclude specific players from monitoring
- **⚡ Event-Driven**: Efficient architecture with zero polling overhead

## 🚀 Quick Start

### Prerequisites

- **Rust toolchain** (1.70+): Install from [rustup.rs](https://rustup.rs)
- **Linux** with D-Bus support
- **MPRIS-compatible media player**

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
./target/release/lyricsmpris --database ~/.config/lyricsmpris/cache.json

# Disable karaoke highlighting
./target/release/lyricsmpris --no-karaoke

# Pipe mode for scripting
./target/release/lyricsmpris --pipe
```

## ⚙️ Configuration

### Command Line Options

| Flag | Description | Example |
|------|-------------|---------|
| `--database PATH` | Enable local lyrics cache | `--database ~/.cache/lyrics.json` |
| `--providers LIST` | Set provider priority | `--providers musixmatch,lrclib` |
| `--no-karaoke` | Disable word-level highlighting | - |
| `--pipe` | Output to stdout instead of TUI | - |
| `--block LIST` | Ignore specific MPRIS services | `--block vlc,chromium` |
| `--debug-log` | Enable diagnostic logging | - |

### Environment Variables

```bash
# Musixmatch user token (required for Musixmatch provider)
export MUSIXMATCH_USERTOKEN="your-token-here"

# Default provider list (if --providers not specified)
export LYRIC_PROVIDERS="lrclib,musixmatch"
```

#### Getting a Musixmatch Token

1. Open [Musixmatch Web Player](https://www.musixmatch.com)
2. Open Browser DevTools (F12) → Network tab
3. Play any song and look for requests to `apic-desktop.musixmatch.com`
4. Find the `x-mxm-token-guid` cookie value
5. Set it as `MUSIXMATCH_USERTOKEN`

### TUI Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `k` | Toggle karaoke highlighting |
| `q` or `Esc` | Quit application |

## 💾 Local Database

The database feature provides persistent lyrics caching for improved performance and offline access.

### Setup

```bash
# Create cache directory
mkdir -p ~/.local/share/lyricsmpris

# Run with database enabled
lyricsmpris --database ~/.local/share/lyricsmpris/cache.json
```

### How It Works

1. **First Play**: Lyrics fetched from providers → stored in database
2. **Subsequent Plays**: Lyrics loaded instantly from database (no API calls)
3. **Auto-Save**: Database automatically persists to disk after each fetch

### Storage Format

The database stores lyrics in their original format by provider:

- **LRCLIB Provider**: LRC timestamp format (`[MM:SS.CC]lyrics text`)
- **Musixmatch Richsync**: Raw JSON with word-level timing data
- **Musixmatch Subtitles**: Raw JSON with line-level timing

#### Example Entry

```json
{
  "entries": {
    "artist|title|album": {
      "artist": "Arctic Monkeys",
      "title": "Do I Wanna Know?",
      "album": "AM",
      "duration": 272.0,
      "format": "richsync",
      "raw_lyrics": "[{\"ts\":29.26,\"te\":31.597,...}]",
      "created_at": 1729123456
    }
  }
}
```

### Benefits

- ⚡ **Instant Loading**: Cached tracks display immediately
- 🌐 **Offline Mode**: No internet required for cached songs
- 📉 **Reduced API Calls**: Be kind to provider rate limits
- 💪 **Provider Independence**: Lyrics persist even if APIs change

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
lyricsmpris --block chromium,firefox,plasma-browser-integration
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
  "interval": 1
}
```

### Systemd User Service

Create `~/.config/systemd/user/lyricsmpris.service`:

```ini
[Unit]
Description=LyricsMPRIS Lyrics Viewer
After=graphical-session.target

[Service]
Type=simple
ExecStart=%h/.local/bin/lyricsmpris --database %h/.local/share/lyricsmpris/cache.json
Restart=on-failure

[Install]
WantedBy=default.target
```

Enable and start:
```bash
systemctl --user enable --now lyricsmpris
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
3. **Enable debug logging**: Use `--debug-log` to see API responses
4. **Check metadata**: Some players may not provide complete track info

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
cargo run -- --debug-log

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
- **Binary Size**: ~10MB (release, stripped)
- **Memory Usage**: <20MB typical
- **CPU Usage**: ~0% typical
- **Dependencies**: Minimal, security-conscious selection

---

**Made with ❤️ for the Linux audio community**
