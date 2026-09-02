//! Playback status and position querying for MPRIS.

use crate::mpris::connection::{MprisError, get_dbus_conn};
use crate::mpris::proxies::MediaPlayer2PlayerProxy;

/// Query the playback position for a specific MPRIS player service
///
/// Returns position in seconds, or an error if the service is unavailable or D-Bus query fails.
/// Uses native uncached proxy position() to bypass proxy caches and ensure real-time accuracy.
pub async fn get_position(service: &str) -> Result<f64, MprisError> {
    if service.is_empty() {
        return Ok(0.0);
    }

    let conn = get_dbus_conn().await?;
    let proxy = MediaPlayer2PlayerProxy::builder(&conn)
        .destination(service)?
        .build()
        .await?;

    let microsecs = proxy.position().await?;
    Ok(microsecs as f64 / 1_000_000.0)
}

/// Query the playback status for a specific MPRIS player service.
///
/// Returns "Playing", "Paused", or "Stopped" as a string.
/// Returns "Stopped" if the service is unavailable or on error.
pub async fn get_playback_status(service: &str) -> String {
    if service.is_empty() {
        return "Stopped".to_string();
    }

    let Ok(conn) = get_dbus_conn().await else {
        return "Stopped".to_string();
    };
    let Ok(proxy) = MediaPlayer2PlayerProxy::builder(&conn).destination(service) else {
        return "Stopped".to_string();
    };
    let Ok(proxy) = proxy.build().await else {
        return "Stopped".to_string();
    };

    match proxy.playback_status().await {
        Ok(status) => status,
        Err(_) => "Stopped".to_string(),
    }
}
