//! Minimal playback status and position querying for MPRIS.

use crate::mpris::connection::{MprisError, get_dbus_conn};
use zbus::Proxy;

/// Query the playback position for a specific MPRIS player service.
pub async fn get_position(service: &str) -> Result<f64, MprisError> {
    if service.is_empty() {
        return Ok(0.0);
    }
    let conn = get_dbus_conn().await?;
    let proxy = Proxy::new(&conn, service, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player").await?;
    let position: Option<i64> = proxy.get_property("Position").await.ok();
    Ok(position.map(|p| p as f64 / 1_000_000.0).unwrap_or(0.0))
}

/// Query the playback status for a specific MPRIS player service.
pub async fn get_playback_status(service: &str) -> Result<String, MprisError> {
    if service.is_empty() {
        return Ok("Stopped".to_string());
    }
    let conn = get_dbus_conn().await?;
    let proxy = Proxy::new(&conn, service, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player").await?;
    let playback_status: Option<String> = proxy.get_property("PlaybackStatus").await.ok();
    Ok(playback_status.unwrap_or_else(|| "Stopped".to_string()))
}
