pub mod modern;
pub mod modern_helpers;
pub mod progression;
pub mod pipe;
pub mod styles;
pub mod util;

// Re-export the ergonomic helper so callers can use `crate::ui::track_id(...)`.
pub use util::track_id;
// Re-export useful progression helpers for a shorter path: `crate::ui::estimate_update_and_next_sleep`.
pub use progression::estimate_update_and_next_sleep;
