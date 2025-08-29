 # LyricsMPRIS

 Lightweight TUI and pipe-mode lyrics viewer that listens to MPRIS players and displays synchronized lyrics.

 ## Features

 - Modern terminal UI with centered lyric display and optional per-word (karaoke) highlighting.
 - Pipe mode for piping current lyric line to stdout (script-friendly).
 - Multiple lyric providers (built-in `lrclib`, `musixmatch`), configurable order.
 - Local lyrics database support and simple blocklist for player services.


 Lightweight TUI and pipe-mode lyrics viewer that listens to MPRIS players and displays synchronized lyrics.

 ## Quick start

 ### Prerequisites

 - Rust toolchain (rustc + cargo) installed. See https://rust-lang.org for setup.

 ### Build

 ```
 # build debug
 cargo build

 # build optimized release
 cargo build --release
 ```

 ### Run (examples)

 Run the release binary directly from target:

 ```
 ./target/release/lyricsmpris -h
 ```

 Or via cargo (debug):

 ```
 cargo run -- --no-karaoke
 ```

 ## Environment variables

 - `MUSIXMATCH_USERTOKEN` — Optional: API user token for the Musixmatch provider. If you plan to use `musixmatch` as a provider, set this environment variable to your Musixmatch token:

 ```fish
 set -x MUSIXMATCH_USERTOKEN "your-token-here"
 ```

 - `LYRIC_PROVIDERS` — Comma-separated provider list used as a fallback when `--providers` is not provided on the command line.

 ## Command line flags

 - `--no-karaoke` — Disable per-word karaoke highlighting. Karaoke is enabled by default; pass `--no-karaoke` to turn it off.
 - `--pipe` — Run in pipe mode (prints current lyric line to stdout) instead of the modern TUI.
 - `--database <PATH>` — Path to a local lyrics database file.
 - `--block SERVICE1,SERVICE2` — Comma-separated, case-insensitive list of MPRIS service names to ignore.
 - `--debug-log` — Enable backend error logging to stderr.
 - `--providers lrclib,musixmatch` — Comma-separated provider list in preferred order. If omitted, the `LYRIC_PROVIDERS` environment variable is used as a fallback.

 ## Runtime controls (TUI)

 - Press `k` to toggle karaoke highlighting at runtime.
 - Press `q` or `Esc` to quit the TUI.

 ## Development environment

 This project was developed in Visual Studio Code and authored with the assistance of GitHub Copilot.

 ## Contributing

 - Issues and PRs welcome. Keep changes small and test the TUI and pipe modes.

 ## License

 - See the `LICENSE` file in this repository for license details.

 ## Acknowledgements

 - This project uses several community crates — see `Cargo.toml` for dependencies.
