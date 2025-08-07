//! MPRIS module: re-exports and module declarations for submodules.

pub mod connection;
pub mod metadata;
pub mod playback;
pub mod events;

// Re-export main API for compatibility
pub use connection::{get_active_player_names, is_blocked};
pub use metadata::TrackMetadata;
pub use playback::get_playback_status;

