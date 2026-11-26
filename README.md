# 🎵 LyricsMPRIS-Rust

A lightweight, ultra-high-performance synchronized lyrics viewer for Linux that integrates seamlessly with MPRIS-compatible media players.

[![Language](https://img.shields.io/badge/Language-Rust-orange.svg?style=flat-sq&logo=rust)](https://www.rust-lang.org/)
[![Edition](https://img.shields.io/badge/Edition-2024-red.svg?style=flat-sq)](https://doc.rust-lang.org/edition-guide/rust-2024/)
[![Platform](https://img.shields.io/badge/Platform-Linux-blue.svg?style=flat-sq&logo=linux)](https://kernel.org)
[![License](https://img.shields.io/badge/License-MIT-green.svg?style=flat-sq)](LICENSE)

`LyricsMPRIS-Rust` monitors your active media player in real-time using D-Bus, fetches synchronized lyrics automatically, and renders them beautifully in a modern Terminal User Interface (TUI) with word-level karaoke support. It also features a scriptable **Pipe Mode** for status bar integration and **ISRC tag reading** from local audio files.

![LyricMPRIS-Rust](https://github.com/user-attachments/assets/501f224e-6c40-46cd-ac66-cd9ae4f927cf)

---

## 📖 Table of Contents

1. [🌟 Key Features](#-key-features)
2. [🚀 Installation](#-installation)
3. [💻 Basic Usage](#-basic-usage)
4. [⚙️ Configuration](#-configuration)
5. [🎤 Musixmatch Token Management](#-musixmatch-token-management)
6. [⌨️ Keyboard Shortcuts](#%EF%B8%8F-keyboard-shortcuts)
7. [🔌 Desktop Integration](#-desktop-integration)
8. [🏗️ Project Architecture](#%EF%B8%8F-project-architecture)
9. [🐛 Troubleshooting & Debugging](#-troubleshooting--debugging)
10. [🤝 Contributing](#-contributing)

---

## 🌟 Key Features

### 🎨 Modern TUI & Pipe Mode
* **Ratatui Terminal UI**: Centered lyrics display with smooth auto-scrolling, word-by-word karaoke highlighting, active line tracking, and resilient RAII terminal lifecycle management.
* **Adaptive Playback Calibration**: Transient position calibration automatically compensates for streaming buffer delays (e.g., Spotify, browser MPRIS extensions), Bluetooth latency, and seek jitter across all media players—locking into sample-accurate sync within ~1.2s before self-disarming for zero ongoing D-Bus polling overhead.
* **Manual Scroll**: Browse past or upcoming lyrics using arrow keys (`↑`/`↓`) when playback is paused.
* **Dynamic Runtime Toggle**: Hot-toggle per-word karaoke highlighting (`k`) in real-time during playback.
* **Streamlined Pipe Mode**: Outputs current lyric line to `stdout` in real-time, automatically clearing out-of-bound segments (intros/outros/silence), and routing transient network warnings to `stderr` to keep status bars (Polybar, Waybar, i3blocks) pristine.

### 📚 23 Lyrics Providers & 3-Stage Concurrent Racing
* **23 Morphe-Order Providers**: Races `apple`, `lyricsplus`, `lrcred`, `blyrics`, `lyricify`, `unison`, `petitlyrics`, `amll`, `qq`, `simpmusic`, `kugou`, `musixmatch`, `ytmusic`, `lrclib`, `netease`, `luna`, `kuwo`, `lunabeat`, `captions`, `spotify`, `deezer`, `rmm`, and `genius`.
* **Rich Format Parsers**: Supports TTML2, LRC/ELRC, KRC, QRC, YRC, Lyricify, LyricsFile YAML, PetitLyrics WSY/LSY, and YouTube timedtext/json3.
* **3-Stage Concurrent Orchestrator**: Runs staged racing (`2s` / `4s` / `9s` deadlines, `5s` endgame fallback, and `800ms` first-paint hold for word-sync priority) plus candidate cycling (`r` in TUI).

### 🔑 3-Tier Automatic Musixmatch Token Engine
* **Zero Configuration Needed**: Automatically fetches a fresh anonymous usertoken directly from the Musixmatch mobile API if no user token is provided.
* **Dummy Token Filtering**: Automatically detects and rejects dummy all-zero tokens returned by rate-limited or blocked endpoints.
* **Token Rotation**: Supports multiple custom usertokens in `MUSIXMATCH_USERTOKEN` env var, rotated in round-robin order.
* **Auto-Renewal & Retry**: If a token encounters a rate limit (`429`) or auth error (`401`/`403`), the engine invalidates it, fetches a new token, and retries the request transparently.

### ⚡ Resilience & Background Retries
* **Transient Error Classification**: Automatically distinguishes between missing lyrics and temporary network/server outages (rate limits, DNS failures, 5xx errors).
* **Exponential Backoff Loop**: Automatically schedules background retries with exponential backoff (up to 60s cap) when providers are temporarily unreachable without interrupting active playback.
* **Non-Blocking Epoch-Protected Workers**: Decoupled asynchronous worker tasks retrieve lyrics in the background without blocking the D-Bus event loop, discarding stale responses via generational epoch matching during rapid track skipping.

### 🏷️ Local File ISRC Extraction
* Reads local audio file metadata (FLAC, MP3, M4A, OGG) via `lofty` when `--isrc` is enabled to perform pinpoint ISRC lookups.

### 🚀 Ultra-Low Resource Footprint
* **Single-Threaded Async Engine**: Utilizes Tokio `current_thread` runtime and asynchronous event streaming, reducing active thread count down to just 1–2 lightweight threads.
* **Non-Blocking Reactor**: Decoupled asynchronous fetch workers and monotonic time projections prevent network I/O from blocking D-Bus event handling or TUI responsiveness.
* **Zero-Allocation Rendering**: Zero-copy span borrowing and chunk-reversed visible line collection eliminate frame-by-frame heap allocations.
* **D-Bus Query Timeouts**: Dynamic `Position` and `PlaybackStatus` queries are protected with a 150ms timeout (`DBUS_QUERY_TIMEOUT`), preventing frozen players from stalling the reactor.
* **Dual-Tier Discovery & Playback Prioritization**: Prefers `playerctld` for recency-ordered player prioritization, with an intelligent fallback (`Playing` > `Paused` > `Stopped`) via standard D-Bus discovery.
* **Real-Time Lifecycle & Cross-Player Handoff**: 100% event-driven cross-player handoff via D-Bus `NameOwnerChanged` and broker-level `PropertiesChanged` match rules.
* **Pure Event-Driven Idle & Clean Shutdown**: Consumes zero CPU and generates zero IPC traffic when idle, with clean background task termination via Tokio watch channels.

---

## 🚀 Installation

### Prerequisites
- **Linux** desktop environment with **D-Bus** session daemon.
- **Rust Toolchain** (1.85+): Install via [rustup.rs](https://rustup.rs) (required for Rust Edition 2024).
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
# Exclude specific providers from the default 23-provider chain
./target/release/lyricsmpris --providers -genius,-rmm

# Allow unsynced plain-text lyrics or load custom TOML settings
./target/release/lyricsmpris --allow-plain --config ~/.config/lyricsmpris/config.toml

# Enable ISRC extraction from local audio files via lofty
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
| `--config <PATH>` | Path to TOML configuration file (default: `~/.config/lyricsmpris/config.toml`) | `--config ./config.toml` |
| `--providers <LIST>` | Ordered providers or `-provider` exclusions | `--providers apple,lrclib,-genius` |
| `--allow-plain` | Allow unsynced plain-text lyrics when synced lyrics are unavailable | `--allow-plain` |
| `--no-split-bg` | Keep parenthesized background vocals inline instead of splitting | `--no-split-bg` |
| `--visible-lines <COUNT>`| Limit maximum visible lyric lines in TUI | `--visible-lines 3` |
| `--isrc` | Extract ISRC from local audio files via `lofty` | `--isrc` |
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

## ⌨️ Keyboard Shortcuts

Control the modern TUI backend with these keys:

| Key | Action |
| :---: | :--- |
| `k` | Toggle Karaoke Highlight Mode |
| `r` | Cycle through fetched alternate lyrics candidates |
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
├── config.rs         # Layered config (CLI > env > TOML > defaults) & provider resolution
├── event/            # 3-stage concurrent fetcher, DisplayState, MPRIS handlers, retry backoff
├── pool.rs           # Event loop supervisor (wires D-Bus → state → UI) & candidate cycling
├── state.rs          # StateBundle, PlayerState, LyricState, Update snapshot
├── timer.rs          # PlaybackTimer & TimeAnchor (monotonic position projection)
├── text_utils.rs     # Normalization, zhconv, metadata cleaning, and text wrapping
├── lyrics/
│   ├── providers/    # 23 lyrics providers (apple, lyricsplus, lrcred, musixmatch/, etc.)
│   ├── parsers/      # LRC/ELRC, TTML2, KRC, Lyricify, LyricsFile YAML parsers
│   ├── crypto.rs     # AES-ECB/PKCS#7, Kugou XOR+zlib, QRC 3DES-ECB, PetitLyrics LSY XOR
│   ├── filter.rs     # Credit/text filters, bracket splitting, overlap retiming & formatting
│   ├── parse.rs      # Top-level parser facade + Musixmatch JSON parsers
│   └── types.rs      # LyricLine, Lyrics, WordTiming, SyncTracker, TrackMatchInfo
├── mpris/
│   ├── connection.rs # D-Bus session singleton, PlaybackPriority, discovery & filtering
│   ├── events.rs     # D-Bus signal listener, broker-level match rules & calibration
│   ├── metadata.rs   # MPRIS metadata, YouTube videoId, & local file ISRC (lofty)
│   ├── playback.rs   # Direct D-Bus playback position & status querying
│   └── proxies.rs    # Consolidated D-Bus proxy interfaces
└── ui/
    ├── modern.rs     # Modern Ratatui TUI renderer with RAII terminal lifecycle
    ├── modern_helpers.rs # Syllable-aware line/word layout & karaoke highlighting
    ├── pipe.rs       # Clean stdout/stderr pipe mode renderer
    └── ...           # Styles, progression scheduler, wrapping cache
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
