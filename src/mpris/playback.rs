//! Playback status and position querying for MPRIS.

use crate::mpris::connection::{get_dbus_conn, MprisError};
use zbus::proxy;

/// Playback status values according to MPRIS specification
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

impl PlaybackStatus {
    /// Convert MPRIS string status to enum
    pub fn from_str(s: &str) -> Self {
        match s {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            _ => Self::Stopped,
        }
    }

    /// Convert to MPRIS string status
    pub fn as_str(&self) -> &str {
        match self {
            Self::Playing => "Playing",
            Self::Paused => "Paused",
            Self::Stopped => "Stopped",
        }
    }
}

impl Default for PlaybackStatus {
    fn default() -> Self {
        Self::Stopped
    }
}

impl From<String> for PlaybackStatus {
    fn from(s: String) -> Self {
        Self::from_str(&s)
    }
}

impl From<PlaybackStatus> for String {
    fn from(status: PlaybackStatus) -> Self {
        status.as_str().to_string()
    }
}

/// MPRIS MediaPlayer2.Player interface proxy for playback control
#[proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait MediaPlayer2Player {
    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn position(&self) -> zbus::Result<i64>;
}

/// Query the playback position for a specific MPRIS player service
/// 
/// Returns position in seconds. Returns 0.0 if the service is unavailable or on error.
pub async fn get_position(service: &str) -> Result<f64, MprisError> {
    if service.is_empty() {
        return Ok(0.0);
    }

    let conn = get_dbus_conn().await?;
    
    let proxy = MediaPlayer2PlayerProxy::builder(&conn)
        .destination(service)?
        .build()
        .await?;

    match proxy.position().await {
        Ok(microseconds) => {
            // Convert microseconds to seconds
            Ok(microseconds as f64 / 1_000_000.0)
        }
        Err(_) => Ok(0.0),
    }
}

/// Query the playback status for a specific MPRIS player service
/// 
/// Returns "Playing", "Paused", or "Stopped" as a string.
/// Returns "Stopped" if the service is unavailable or on error.
pub async fn get_playback_status(service: &str) -> Result<String, MprisError> {
    if service.is_empty() {
        return Ok("Stopped".to_string());
    }

    let conn = get_dbus_conn().await?;
    
    let proxy = MediaPlayer2PlayerProxy::builder(&conn)
        .destination(service)?
        .build()
        .await?;

    match proxy.playback_status().await {
        Ok(status) => Ok(status),
        Err(_) => Ok("Stopped".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_playback_status_conversion() {
        assert_eq!(PlaybackStatus::from_str("Playing"), PlaybackStatus::Playing);
        assert_eq!(PlaybackStatus::from_str("Paused"), PlaybackStatus::Paused);
        assert_eq!(PlaybackStatus::from_str("Stopped"), PlaybackStatus::Stopped);
        assert_eq!(PlaybackStatus::from_str("Unknown"), PlaybackStatus::Stopped);

        assert_eq!(PlaybackStatus::Playing.as_str(), "Playing");
        assert_eq!(PlaybackStatus::Paused.as_str(), "Paused");
        assert_eq!(PlaybackStatus::Stopped.as_str(), "Stopped");
    }
}