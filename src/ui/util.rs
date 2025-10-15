//! UI utility functions for track identification.
//!
//! This module provides helpers for creating canonical track identifiers
//! used by UI code to detect track changes. Track IDs are based on the
//! (artist, title, album) triple.
//!
//! # Design Note
//! This module lives under `ui` because track identification is primarily
//! used for UI state management (detecting when to clear cached lyrics,
//! reset display state, etc.).

/// Trait for types that can be converted to a canonical track identifier.
///
/// A track ID is a tuple of (artist, title, album) strings that uniquely
/// identifies a track for UI purposes.
///
/// # Example
/// ```ignore
/// use crate::ui::util::{AsTrackId, track_id};
/// 
/// let update = get_update();
/// let id = track_id(&update);
/// if last_id != Some(id) {
///     // Track changed - reset UI state
/// }
/// ```
pub trait AsTrackId {
    /// Extract the canonical track identifier.
    ///
    /// Returns a tuple of (artist, title, album).
    fn as_track_id(&self) -> (String, String, String);
}

impl AsTrackId for crate::state::Update {
    fn as_track_id(&self) -> (String, String, String) {
        (
            self.artist.clone(),
            self.title.clone(),
            self.album.clone(),
        )
    }
}

impl AsTrackId for crate::mpris::TrackMetadata {
    fn as_track_id(&self) -> (String, String, String) {
        (
            self.artist.clone(),
            self.title.clone(),
            self.album.clone(),
        )
    }
}

/// Extract a track identifier from any type implementing `AsTrackId`.
///
/// This is a convenience function that allows more ergonomic usage:
/// ```ignore
/// let id = track_id(&update);
/// ```
/// instead of:
/// ```ignore
/// let id = update.as_track_id();
/// ```
///
/// # Arguments
/// * `t` - Any type that implements `AsTrackId`
///
/// # Returns
/// A tuple of (artist, title, album) strings
pub fn track_id<T: AsTrackId>(t: &T) -> (String, String, String) {
    t.as_track_id()
}
