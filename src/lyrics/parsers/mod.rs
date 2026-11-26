pub mod krc;
pub mod lrc;
pub mod lyricify;
pub mod lyrics_file;
pub mod ttml;

pub use krc::parse_krc;
pub use lrc::{extract_credit_metadata, parse_plain, parse_synced, parse_synced_with_credits};
pub use lyricify::{parse_lyricify_lines, parse_lyricify_syllable, parse_lyricify_translation};
pub use lyrics_file::parse_lyrics_file;
pub use ttml::parse_ttml;
