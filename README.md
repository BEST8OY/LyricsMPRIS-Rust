# LyricsMPRIS-Rust

A fast, standalone synchronized lyrics viewer for Linux that tracks any MPRIS-compatible media player over D-Bus and renders real-time lyrics in a Ratatui terminal interface or a scriptable `stdout` pipe.

[![Language](https://img.shields.io/badge/Language-Rust-orange.svg?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Edition](https://img.shields.io/badge/Edition-2024-red.svg?style=flat-square)](https://doc.rust-lang.org/edition-guide/rust-2024/)
[![Platform](https://img.shields.io/badge/Platform-Linux-blue.svg?style=flat-square&logo=linux)](https://kernel.org)
[![License](https://img.shields.io/badge/License-MIT-green.svg?style=flat-square)](LICENSE)

![LyricsMPRIS-Rust TUI](https://github.com/user-attachments/assets/501f224e-6c40-46cd-ac66-cd9ae4f927cf)

---

## Features

- **Modern Ratatui TUI**: Centered viewport with active-line focus, per-word karaoke highlighting (toggleable live), background/duet vocal formatting, and RAII terminal cleanup on exit or panic.
- **Pipe Mode (`--pipe`)**: Streams the current lyric line to `stdout` for Polybar, Waybar, and shell scripts, emits blank lines to clear intros or out-of-bounds seeks, folds background vocals inline, and routes logs to `stderr`.
- **23 Lyrics Providers**: Races sources in a 3-stage concurrent pipeline with word-sync preference, metadata candidate scoring, and interactive candidate cycling (`r`).
- **Rich Format & Crypto Support**: Parses TTML2 (syllable timing, background vocals, agent roles), ELRC/LRC, KRC, QRC, YRC, Lyricify, LyricsFile YAML, PetitLyrics WSY/LSY, Musixmatch richsync/subtitles, and YouTube timedtext/json3.
- **Standalone Event-Driven MPRIS**: Uses `playerctld` when present and falls back to priority-ranked D-Bus discovery (`Playing > Paused > Stopped`) with `NameOwnerChanged` and `PropertiesChanged` rules—generating zero D-Bus traffic while idle.
- **Adaptive Sync & Local ISRC**: Calibrates buffering delay on track start/seek and optionally reads ISRC tags from local audio files (`--isrc`) via `lofty`.
- **Zero-Config Musixmatch**: Provisions and rotates anonymous Musixmatch tokens automatically, with optional user-provided tokens for round-robin pooling.

---

## Installation

### Prerequisites

- **Linux** with a running D-Bus session bus
- **Rust 1.85+** (required for Rust Edition 2024): install via [rustup.rs](https://rustup.rs)
- **`playerctld`** *(optional)*: adds cross-session player recency ordering when installed; standalone discovery works without it

### Build from Source

```bash
git clone https://github.com/BEST8OY/LyricsMPRIS-Rust.git
cd LyricsMPRIS-Rust
cargo build --release
```

The release binary is placed at `./target/release/lyricsmpris`. You can also install it directly into `~/.cargo/bin`:

```bash
cargo install --path .
```

---

## Quick Start

```bash
# Launch the interactive TUI (default)
lyricsmpris

# Stream active lines to stdout (for status bars / scripts)
lyricsmpris --pipe

# Compact TUI view capped at 5 logical lyric lines
lyricsmpris --visible-lines 5

# Exclude specific providers from the default 23-provider chain
lyricsmpris --providers -genius,-rmm

# Prioritize specific providers only
lyricsmpris --providers apple,musixmatch,lrclib

# Target or ignore specific MPRIS player services
lyricsmpris --target spotify,mpv --block firefox,chromium

# Enable local file ISRC lookup and plain-text lyric fallback
lyricsmpris --isrc --allow-plain
```

---

## TUI Keyboard Shortcuts

| Key | Action |
| :---: | :--- |
| `k` | Toggle per-word karaoke highlighting |
| `r` | Cycle through fetched alternate lyric candidates |
| `↑` / `↓` | Scroll one line up or down *(when paused)* |
| `PgUp` / `PgDn` | Jump 5 lines up or down *(when paused)* |
| `Home` / `End` | Jump to top or bottom *(when paused)* |
| `q` / `Esc` / `Ctrl+C` | Quit |

> Manual scrolling is available while playback is paused; resuming playback automatically snaps the viewport back to the active line.

---

## Configuration

Settings follow strict layered precedence:

```text
CLI flags  >  Environment variables  >  TOML config file  >  Defaults
```

### CLI Options

| Flag | Description |
| :--- | :--- |
| `--pipe` | Print the current lyric line to `stdout` instead of launching the TUI |
| `--config <PATH>` | Path to TOML config file (default: `~/.config/lyricsmpris/config.toml`) |
| `--block <SERVICE1,SERVICE2>` | Comma-separated MPRIS service names to ignore (case-insensitive) |
| `--target <SERVICE1,SERVICE2>` | Comma-separated MPRIS service names to exclusively follow |
| `--no-karaoke` | Start with per-word karaoke highlighting disabled |
| `--visible-lines <COUNT>` | Maximum logical lyric lines displayed in TUI (wrapped lines count as one) |
| `--providers <LIST>` | Comma-separated provider chain or `-provider` exclusions |
| `--isrc` | Extract ISRC tags from local audio files via `lofty` |
| `--allow-plain` | Allow unsynced plain-text lyrics when synced lyrics are unavailable |
| `--no-split-bg` | Keep parenthesized background vocals inline instead of splitting lines |
| `-h`, `--help` | Print CLI help |
| `-V`, `--version` | Print version |

### TOML Config (`~/.config/lyricsmpris/config.toml`)

```toml
# Provider order or "-provider" exclusions (defaults to all 23 Morphe-ordered providers)
# providers = ["apple", "musixmatch", "amll", "lrclib"]

allow_plain = false
karaoke = true
# visible_lines = 7
isrc = false

# MPRIS service filters
# target = ["spotify", "mpv"]
# block = ["firefox", "chromium"]

[tokens]
# musixmatch_usertoken = "tok1,tok2"  # Comma-separated tokens rotate round-robin
# spotify_sp_dc = ""
# apple_media_user_token = ""
# deezer_arl = ""
# youtube_cookies = ""

[endpoints]
# lrclib = "https://lrclib.net"
# amll = ""
# blyrics = ""
# lyricsplus = ""
# lunabeat = ""
# simpmusic = ""
# unison = ""

[filters]
han_mode = "none"              # "none", "simplified", or "traditional"
split_background = true
retime_overlaps = true
force_capitalize = false
strip_trailing_punctuation = false
# custom_metadata_regex = ""
# lyrics_text_filter_regex = ""
# credit_line_markers = ""
```

### Environment Variables

| Variable | Purpose |
| :--- | :--- |
| `LYRIC_PROVIDERS` | Comma-separated provider order or `-provider` exclusions |
| `LYRICS_ALLOW_PLAIN` | Set `1`, `true`, or `yes` to enable plain-text fallback |
| `LYRICS_HAN_MODE` | Chinese script conversion: `none`, `simplified`, or `traditional` |
| `MUSIXMATCH_USERTOKEN` | Custom Musixmatch usertoken(s), comma-separated for round-robin rotation |
| `SPOTIFY_SP_DC` | Spotify `sp_dc` cookie for Spotify lyrics |
| `APPLE_MEDIA_USER_TOKEN` | Apple Music `media-user-token` |
| `DEEZER_ARL` | Deezer `arl` cookie |
| `YOUTUBE_COOKIES` | YouTube session cookies |
| `LRCLIB_ENDPOINT`, `AMLL_ENDPOINT`, `BLYRICS_ENDPOINT`, `LYRICSPLUS_ENDPOINT`, `LUNABEAT_ENDPOINT`, `SIMPMUSIC_ENDPOINT`, `UNISON_ENDPOINT` | Custom `http://` or `https://` base URL overrides per provider |
| `RUST_LOG` | Tracing filter (e.g., `info`, `debug`) |
| `RUST_LOG_FILE` | Log file path in TUI mode (default: `/tmp/lyricsmpris.log`) |

---

## Lyrics Providers

By default, all 23 providers race across 3 staged deadlines in Morphe's curated priority order:

```text
apple, lyricsplus, lrcred, blyrics, lyricify, unison, petitlyrics, amll,
qq, simpmusic, kugou, musixmatch, ytmusic, lrclib, netease, luna,
kuwo, lunabeat, captions, spotify, deezer, rmm, genius
```

- Use `-name` exclusions (e.g., `--providers -spotify,-genius`) to keep the full default order with specific providers removed.
- Pass explicit names (e.g., `--providers lrcred,musixmatch,lrclib`) to restrict and prioritize only those providers.
- **Musixmatch tokens**: No setup is required—`token.get` automatically fetches and caches an anonymous token (`~/.cache/lyricsmpris/musixmatch_token.json`) and renews it on auth/rate-limit errors (`401`/`402`/`403`/`429`). Supplying `MUSIXMATCH_USERTOKEN` enables Tier-1 round-robin pooling across your own tokens first.

---

## Desktop Integration (`--pipe`)

Pipe mode writes only active lyrics (and empty lines when clearing intros or seeks before the first line) to `stdout`, while sending errors and logs to `stderr`.

### Waybar

```json
"custom/lyrics": {
    "format": "♪ {}",
    "exec": "lyricsmpris --pipe",
    "restart-interval": 5,
    "tooltip": false
}
```

### Polybar

```ini
[module/lyrics]
type = custom/script
exec = lyricsmpris --pipe
tail = true
format-prefix = "♪ "
format-prefix-foreground = #8abeb7
```

---

## Troubleshooting

**Enable debug logs**
- **TUI mode** writes logs to `/tmp/lyricsmpris.log` (override with `RUST_LOG_FILE`) so terminal rendering is untouched:
  ```bash
  RUST_LOG=debug lyricsmpris
  tail -f /tmp/lyricsmpris.log
  ```
- **Pipe mode** writes logs directly to `stderr`:
  ```bash
  RUST_LOG=debug lyricsmpris --pipe 2>/tmp/lyricsmpris.log
  ```

**Player not detected**
Verify your media player exposes an `org.mpris.MediaPlayer2.*` D-Bus interface:
```bash
playerctl -l
```
Also ensure the player is not filtered out by `--target` or `--block` rules in CLI or `config.toml`.

---

## Project Architecture

```text
src/
├── main.rs              # CLI parsing (clap), tracing setup, Tokio current_thread entry point
├── config.rs            # Precedence resolution (CLI > env > TOML > defaults)
├── pool.rs              # D-Bus event loop supervisor, fetch orchestration, candidate cycling
├── state.rs             # PlayerState, LyricState, Update snapshots
├── timer.rs             # PlaybackTimer & TimeAnchor monotonic position projection
├── text_utils.rs        # Text normalization, zhconv Han conversion, metadata cleaning
├── event/
│   ├── fetch.rs         # 3-stage concurrent provider racing & tiered query builder
│   ├── display_state.rs # Candidate scoring, word-sync upgrade hold, deduplication
│   └── handlers.rs      # MPRIS position calibration & track transition handlers
├── mpris/
│   ├── connection.rs    # D-Bus discovery (playerctld + priority fallback) & filtering
│   ├── events.rs        # NameOwnerChanged & PropertiesChanged signal streams
│   ├── metadata.rs      # MPRIS metadata, YouTube videoId, and lofty ISRC extraction
│   ├── playback.rs      # Direct D-Bus Position & PlaybackStatus queries
│   └── proxies.rs       # Consolidated zbus 6 proxy definitions
├── lyrics/
│   ├── providers/       # 23 provider implementations (including musixmatch/ package)
│   ├── parsers/         # LRC/ELRC, TTML2, KRC, Lyricify, LyricsFile YAML parsers
│   ├── crypto.rs        # AES-ECB/PKCS#7, Kugou XOR+zlib, QRC 3DES-ECB, PetitLyrics XOR
│   ├── filter.rs        # Credit filtering, bracket bg splitting, overlap retiming, pipe folding
│   ├── parse.rs         # Top-level parser facade & Musixmatch richsync/LRC parsing
│   └── types.rs         # LyricLine, WordTiming, Lyrics, SyncTracker
└── ui/
    ├── modern.rs        # Ratatui TUI event loop, key handling, RAII terminal guard
    ├── modern_helpers.rs# Syllable/word wrapping, karaoke spans, split/grouped bg rendering
    ├── progression.rs   # Adaptive TUI & pipe mode timer boundary scheduler
    ├── pipe.rs          # Clean stdout line emitter for status bars
    └── styles.rs        # Color & emphasis palette
```

---

## Contributing

Run formatting, lints, and the test suite before submitting changes:

```bash
cargo fmt --check && cargo clippy && cargo test
```

## License

Licensed under the [MIT License](LICENSE).
