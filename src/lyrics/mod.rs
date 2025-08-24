// lyrics/mod.rs - top-level lyrics module re-exporting submodules
pub mod parse;
pub mod providers;
pub mod types;

pub use parse::parse_synced_lyrics;
pub use providers::{fetch_lyrics_from_lrclib, fetch_lyrics_from_musixmatch_usertoken};
pub use types::{LyricLine, LyricsError};
