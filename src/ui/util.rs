/// UI-local utilities.
///
/// This module intentionally lives under `ui` because its helpers are only
/// relevant to UI code (track id creation for UI modules).

/// Trait for types that can produce a canonical (artist, title, album) track id.
pub trait AsTrackId {
    fn as_track_id(&self) -> (String, String, String);
}

impl AsTrackId for crate::state::Update {
    fn as_track_id(&self) -> (String, String, String) {
        (self.artist.clone(), self.title.clone(), self.album.clone())
    }
}

impl AsTrackId for crate::mpris::TrackMetadata {
    fn as_track_id(&self) -> (String, String, String) {
        (self.artist.clone(), self.title.clone(), self.album.clone())
    }
}

/// Ergonomic helper: produce a (artist, title, album) triple from any AsTrackId.
pub fn track_id<T: AsTrackId>(t: &T) -> (String, String, String) {
    t.as_track_id()
}
