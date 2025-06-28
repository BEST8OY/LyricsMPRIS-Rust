# LyricsMPRIS-Rust

A modern, async terminal lyrics viewer for Linux, powered by the [MPRIS](https://specifications.freedesktop.org/mpris-spec/latest/) D-Bus interface. Displays synced lyrics for your currently playing song from any compatible media player (Spotify, VLC, mpv, etc.), with both a beautiful TUI and a scripting-friendly pipe mode.

---

> **Note:** This project is fully written by GitHub Copilot in Visual Studio Code.

---

## Features

- **Modern TUI**: Centered, highlighted, real-time lyrics display in your terminal.
- **Pipe Mode**: Output the current lyric line to stdout for scripting or status bars.
- **MPRIS Support**: Works with any Linux player supporting MPRIS (Spotify, VLC, mpv, etc.).
- **Async & Fast**: Fully async Rust code for smooth, responsive updates.
- **Local Lyrics Database**: Optionally use a local lyrics database for offline lyrics.
- **Blocklist**: Exclude specific MPRIS player services.
- **Error Logging**: Optional debug logging to stderr.

---

## Installation

### Prerequisites
- Linux (with D-Bus and MPRIS-compatible media player)
- [Rust toolchain](https://rustup.rs/)

### Build from Source
```sh
# Clone the repo
 git clone https://github.com/yourusername/LyricsMPRIS-Rust.git
 cd LyricsMPRIS-Rust

# Build the project
 cargo build --release

# Run (see usage below)
 ./target/release/lyricsmpris
```

---

## Usage

```sh
lyricsmpris [OPTIONS]
```

### Options
- `--pipe`           Pipe current lyric line to stdout (for scripting)
- `--poll <ms>`      Lyric poll interval in milliseconds (default: 1000)
- `--database <path>`  Path to local lyrics database (optional)
- `--block <SERVICES>` Blocklist for MPRIS player service names (comma-separated, case-insensitive)
- `--debug-log`      Enable backend error logging to stderr
- `-h, --help`       Print help
- `-V, --version`    Print version

### Examples

- **Modern TUI (default):**
  ```sh
  lyricsmpris
  ```
- **Pipe mode for status bar:**
  ```sh
  lyricsmpris --pipe
  ```
- **Use a local lyrics database:**
  ```sh
  lyricsmpris --database ~/.local/share/lyrics.db
  ```
- **Block Spotify, VLC and Edge:**
  ```sh
  lyricsmpris --block spotify,vlc
  ```

---

## How It Works

- Connects to the D-Bus session and queries MPRIS-compatible players for metadata and playback position.
- Periodically polls and/or listens for D-Bus events to update lyrics in real time.
- Displays lyrics in a modern TUI or pipes the current line to stdout.
- Optionally loads lyrics from a local database.

---

## Supported Players
Any Linux media player that implements the MPRIS D-Bus interface, including:
- Spotify
- VLC
- mpv
- Rhythmbox
- Audacious
- ...and many more

---

## Troubleshooting
- **No lyrics shown?**  Make sure your player supports MPRIS and is playing a track with metadata.
- **D-Bus errors?**  Ensure you have a running D-Bus session and the player is started normally (not as root).
- **Debugging:**  Use `--debug-log` to print backend errors to stderr.

---

## Contributing
Pull requests, bug reports, and feature suggestions are welcome! Please open an issue or PR on GitHub.

---

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE) for details.
