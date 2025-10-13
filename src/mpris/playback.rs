//! Minimal playback status and position querying for MPRIS.

use crate::mpris::connection::{MprisError, get_dbus_conn};
use zbus::Proxy;
use zvariant::OwnedValue;

fn parse_position_from_owned(val: &OwnedValue) -> Option<f64> {
    // Try direct integer types
    if let Ok(i) = std::convert::TryInto::<i64>::try_into(val.clone()) {
        return Some(i as f64 / 1_000_000.0);
    }
    if let Ok(u) = std::convert::TryInto::<u64>::try_into(val.clone()) {
        return Some(u as f64 / 1_000_000.0);
    }

    // Try tuple forms like (i64,) or (u64,)
    if let Ok((i,)) = std::convert::TryInto::<(i64,)>::try_into(val.clone()) {
        return Some(i as f64 / 1_000_000.0);
    }
    if let Ok((u,)) = std::convert::TryInto::<(u64,)>::try_into(val.clone()) {
        return Some(u as f64 / 1_000_000.0);
    }

    None
}

/// Query the playback position for a specific MPRIS player service.
pub async fn get_position(service: &str) -> Result<f64, MprisError> {
    if service.is_empty() {
        return Ok(0.0);
    }
    let conn = get_dbus_conn().await?;
    // Use targeted Properties.Get to avoid triggering GetAll on some players
    let props_proxy = Proxy::new(&conn, service, "/org/mpris/MediaPlayer2", "org.freedesktop.DBus.Properties").await?;
    if let Ok(reply) = props_proxy.call_method("Get", &("org.mpris.MediaPlayer2.Player", "Position")).await {
        if let Ok(val) = reply.body().deserialize::<OwnedValue>() {
            if let Some(pos) = parse_position_from_owned(&val) {
                return Ok(pos);
            }
        }
    }
    Ok(0.0)
}

/// Query the playback status for a specific MPRIS player service.
pub async fn get_playback_status(service: &str) -> Result<String, MprisError> {
    if service.is_empty() {
        return Ok("Stopped".to_string());
    }
    let conn = get_dbus_conn().await?;
    let props_proxy = Proxy::new(&conn, service, "/org/mpris/MediaPlayer2", "org.freedesktop.DBus.Properties").await?;
    if let Ok(reply) = props_proxy.call_method("Get", &("org.mpris.MediaPlayer2.Player", "PlaybackStatus")).await {
        if let Ok(val) = reply.body().deserialize::<OwnedValue>() {
            if let Ok(status) = std::convert::TryInto::<String>::try_into(val) {
                return Ok(status);
            }
        }
    }
    Ok("Stopped".to_string())
}
