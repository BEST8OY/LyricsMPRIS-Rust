//! Consolidated D-Bus proxy interfaces for MPRIS and playerctld.

use std::collections::HashMap;
use zbus::proxy;
use zvariant::OwnedValue;

/// MPRIS MediaPlayer2.Player interface proxy for player state and control.
///
/// `Metadata` and `PlaybackStatus` use standard property caching (`CacheProperties::Lazily`)
/// enabling property change streams. For real-time position querying, `get_position()` uses
/// direct D-Bus `Properties.Get` to avoid stale cached data.
#[proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub trait MediaPlayer2Player {
    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn rate(&self) -> zbus::Result<f64>;

    /// Dynamic position queries: `emits_changed_signal = "false"` disables property caching
    /// in zbus 5, omitting cached_position() and causing proxy.position().await to always
    /// issue a live D-Bus Get call without separate proxies or raw connection calls.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> zbus::Result<i64>;

    #[zbus(signal)]
    fn seeked(&self, position: i64) -> zbus::Result<()>;
}

/// Playerctld interface proxy for tracking active MPRIS players.
#[proxy(
    interface = "com.github.altdesktop.playerctld",
    default_service = "org.mpris.MediaPlayer2.playerctld",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub trait Playerctld {
    #[zbus(property)]
    fn player_names(&self) -> zbus::Result<Vec<String>>;
}
