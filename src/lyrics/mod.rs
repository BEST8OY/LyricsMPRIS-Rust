// lyrics/mod.rs - top-level lyrics module re-exporting submodules
pub mod database;
pub mod parse;
pub mod providers;
pub mod similarity;
pub mod types;

// parse::parse_synced_lyrics is used via its full path in providers; no top-level re-export needed
pub use providers::{fetch_lyrics_from_lrclib, fetch_lyrics_from_musixmatch_usertoken};
pub use types::{LyricLine, LyricsError};
