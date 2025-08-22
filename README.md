# LyricsMPRIS-Rust

**A sleek, real-time terminal lyrics viewer for Linux.**

LyricsMPRIS connects to any [MPRIS](https://specifications.freedesktop.org/mpris-spec/latest/)-compatible music player (like Spotify, VLC, or mpv) and displays the current song's lyrics right in your terminal. It offers a beautiful, focused TUI and a simple "pipe" mode for scripting and status bars.

---

## Features

- **Real-Time Lyrics:** Automatically fetches and displays synced lyrics for the currently playing song.
- **Modern Terminal UI:** A clean, centered, and highlighted interface that looks great in any terminal.
- **Pipe Mode:** Output the current lyric line directly to `stdout`. Perfect for custom scripts, status bars (like `polybar` or `waybar`), or other tools.
- **Multiple Lyric Providers:** Fetch lyrics from various sources.
- **Wide Compatibility:** Works with any media player that implements the MPRIS D-Bus interface.
- **Fast and Efficient:** Built with asynchronous Rust for a smooth, non-blocking experience.
- **Local Lyrics:** (Optional) Use a local database for instant, offline lyric access.
- **Player Blocklist:** Ignore specific media players you don't want to track.

---

## Installation

### Prerequisites

- A Linux-based OS with D-Bus.
- The [Rust toolchain](https://rustup.rs/) (to build from source).
- An MPRIS-compatible media player.

### Build from Source

1.  **Clone the repository:**
    ```sh
    git clone https://github.com/your-username/LyricsMPRIS-Rust.git
    cd LyricsMPRIS-Rust
    ```

2.  **Build the release binary:**
    ```sh
    cargo build --release
    ```

3.  **Run the application:**
    The executable will be at `./target/release/lyricsmpris`. You can copy it to a directory in your `$PATH` for easy access (e.g., `~/.local/bin`).

---

## Usage

The simplest way to run LyricsMPRIS is without any arguments, which will launch the TUI.

```sh
lyricsmpris
```

### Command-Line Options

For more options, use the `--help` flag:

```sh
$ lyricsmpris --help
```

| Option | Description | Default Value |
|---|---|---|
| `--pipe` | Pipe current lyric line to stdout. | `false` |
| `--database <PATH>` | Path to local lyrics database. | `None` |
| `--block <SERVICES>` | Comma-separated list of player service names to block. | `[]` (empty list) |
| `--debug-log` | Enable backend error logging to stderr. | `false` |
| `--providers <PROVIDERS>` | Comma-separated list of lyric providers in preferred order. | `lrclib,musixmatch` |

### Environment Variables

- `LYRIC_PROVIDERS`: A comma-separated list of lyric providers to use, in preferred order. This is used as a fallback if the `--providers` option is not specified.
- `MUSIXMATCH_USERTOKEN`: To use the Musixmatch provider, you need to provide a user token. You can get this from the Musixmatch desktop app or website.

### Examples

- **Launch the default TUI:**
  ```sh
  lyricsmpris
  ```

- **Pipe lyrics to your status bar:**
  ```sh
  lyricsmpris --pipe
  ```

- **Use a local lyrics database:**
  ```sh
  lyricsmpris --database ~/.config/lyrics.db
  ```

- **Ignore Spotify and VLC:**
  ```sh
  lyricsmpris --block spotify,vlc
  ```

- **Specify lyric providers:**
  ```sh
  lyricsmpris --providers musixmatch,lrclib
  ```

---

## Supported Players

LyricsMPRIS should work with any media player that implements the MPRIS D-Bus interface. This includes, but is not limited to:

- Spotify
- VLC
- mpv (with an MPRIS plugin)
- Rhythmbox
- Audacious
- Elisa
- And many more...

---

## Contributing

Contributions are welcome! If you have a feature request, bug report, or pull request, please feel free to open an issue or PR on the GitHub repository.

---

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.