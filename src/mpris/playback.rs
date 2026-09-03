//! Playback status and position querying for MPRIS.

use crate::mpris::connection::{MprisError, get_dbus_conn};
use std::ops::Deref;
use zvariant::OwnedValue;

/// Query the playback position for a specific MPRIS player service.
///
/// Returns position in seconds, or an error if the service is unavailable or D-Bus query fails.
/// Uses direct D-Bus `Properties.Get` to bypass proxy caches and eliminate proxy construction overhead.
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

    let val: OwnedValue = reply.body().deserialize()?;
    let microsecs = match val.deref() {
        zvariant::Value::I64(v) => *v,
        _ => {
            return Err(MprisError::ZBus(zbus::Error::Failure(
                "Unexpected Position property type from MPRIS player".into(),
            )));
        }
    };

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

    let Ok(reply) = conn
        .call_method(
            Some(service),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.mpris.MediaPlayer2.Player", "PlaybackStatus"),
        )
        .await
    else {
        return "Stopped".to_string();
    };

    let Ok(val) = reply.body().deserialize::<OwnedValue>() else {
        return "Stopped".to_string();
    };

    match val.deref() {
        zvariant::Value::Str(s) => s.as_str().to_string(),
        _ => "Stopped".to_string(),
    }
}
