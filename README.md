# LyricsMPRIS

A blazing-fast, terminal-based lyrics viewer for Linux that syncs in real time with your currently playing song using the MPRIS D-Bus interface. Enjoy a modern TUI (terminal user interface) for immersive lyric display, or use minimal pipe mode for scripting and automation.

## Features
- **Modern TUI**: Centered, highlighted lyrics with smooth, real-time updates.
- **Pipe Mode**: Outputs only the current lyric line to stdout—perfect for scripts and status bars.
- **MPRIS Integration**: Seamlessly detects the active player via `playerctld` (MPRIS D-Bus service).
- **Async & Fast**: Built with async Rust (`tokio`, `dbus-tokio`) for instant, non-blocking updates.
- **Custom Poll Interval**: Choose how often lyrics are refreshed.
- **Local Lyrics Database**: Optionally cache synced lyrics in a local JSON file for instant, offline access.

## Requirements
- Linux with a running MPRIS-compatible media player and `playerctld` ([playerctl](https://github.com/altdesktop/playerctl)).
- Rust (edition 2024) and Cargo for building.

## Installation
Clone the repository and build with Cargo:

```sh
# Clone and build
git clone https://github.com/yourusername/lyricsmpris.git
cd lyricsmpris
cargo build --release
```

## Usage

```sh
# Modern TUI (default)
./target/release/lyricsmpris

# Pipe mode (outputs only the current lyric line)
./target/release/lyricsmpris --pipe

# Set custom poll interval (in milliseconds)
./target/release/lyricsmpris --poll 500

# Use a custom local lyrics database
./target/release/lyricsmpris --database /path/to/lyrics.json
```

### Arguments
- `--pipe` : Pipe current lyric line to stdout (disables TUI)
- `--poll <ms>` : Set lyric poll interval in milliseconds (default: 500)
- `--database <path>` : Use a custom local lyrics database (JSON)

## How It Works
- Connects to the session D-Bus and queries the active MPRIS player via `playerctld`.
- Fetches song metadata (title, artist, album) and playback position.
- Periodically polls for updates and displays synced lyrics in the terminal.
- Optionally caches and retrieves synced lyrics from a local JSON database for offline/instant access.

## Troubleshooting
- Ensure `playerctld` is running: `playerctld --fork`
- Make sure your media player supports MPRIS and is running.
- If you see "No lyrics found for this track", lyrics may not be available for the current song.
- For database issues, check the path and permissions of your lyrics JSON file.

## License
MIT License — see [LICENSE](LICENSE)
