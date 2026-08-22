//! MPRIS module: re-exports and module declarations for submodules.

pub mod connection;
pub mod events;
pub mod metadata;
pub mod playback;
pub mod proxies;

// Re-export main API for use across the crate
pub use connection::find_active_service;
pub use metadata::TrackMetadata;
pub use playback::get_playback_status;
