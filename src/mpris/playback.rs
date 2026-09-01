//! Playback status and position querying for MPRIS.

use crate::mpris::connection::{MprisError, get_dbus_conn};

/// Playback status values according to MPRIS specification
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    #[default]
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

/// Query the playback position for a specific MPRIS player service
///
/// Returns position in seconds, or an error if the service is unavailable or D-Bus query fails.
/// Uses direct D-Bus Properties.Get to bypass proxy caches and ensure real-time accuracy.
pub async fn get_position(service: &str) -> Result<f64, MprisError> {
    if service.is_empty() {
        return Ok(0.0);
    }

    let conn = get_dbus_conn().await?;

    let reply = conn
        .call_method(
            Some(service),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.mpris.MediaPlayer2.Player", "Position"),
        )
        .await?;

    let (val,): (zvariant::OwnedValue,) = reply.body().deserialize()?;
    let microseconds = if let Ok(microsecs) = i64::try_from(&val) {
        microsecs
    } else if let Ok(microsecs) = u64::try_from(&val) {
        microsecs as i64
    } else {
        return Err(MprisError::Fdo(zbus::fdo::Error::Failed(
            "Invalid Position value".to_string(),
        )));
    };
    Ok(microseconds as f64 / 1_000_000.0)
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

    let reply = conn
        .call_method(
            Some(service),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.mpris.MediaPlayer2.Player", "PlaybackStatus"),
        )
        .await;

    match reply {
        Ok(msg) => match msg.body().deserialize::<(zvariant::OwnedValue,)>() {
            Ok((val,)) => Ok(String::try_from(val).unwrap_or_else(|_| "Stopped".to_string())),
            Err(_) => Ok("Stopped".to_string()),
        },
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
