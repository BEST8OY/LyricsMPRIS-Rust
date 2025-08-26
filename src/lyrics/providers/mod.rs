pub mod lrclib;
pub mod musixmatch;

pub use lrclib::fetch_lyrics_from_lrclib;
pub use musixmatch::fetch_lyrics_from_musixmatch_usertoken;
