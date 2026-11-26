//! Playback status and position querying for MPRIS.

use crate::mpris::connection::{MprisError, get_dbus_conn};
use crate::mpris::proxies::MediaPlayer2PlayerProxy;
use std::ops::Deref;
use std::time::Duration;
use zbus::{OwnedVariant, Variant};

/// Timeout applied to dynamic D-Bus queries to prevent frozen players from blocking the caller.
const DBUS_QUERY_TIMEOUT: Duration = Duration::from_millis(150);

fn decode_position_variant(val: &OwnedVariant) -> Result<f64, MprisError> {
    let microsecs = match val.deref() {
        Variant::I64(v) => *v,
        Variant::U64(v) => i64::try_from(*v).unwrap_or(i64::MAX),
        _ => {
            return Err(MprisError::ZBus(zbus::Error::Failure(
                "Unexpected Position property type from MPRIS player".into(),
            )));
        }
    };

    Ok((microsecs.max(0) as f64) / 1_000_000.0)
}

/// Query the playback position for an existing MPRIS player proxy with timeout protection.
pub async fn get_position_from_proxy(
    proxy: &MediaPlayer2PlayerProxy<'_>,
) -> Result<f64, MprisError> {
    let query = async {
        let val: OwnedVariant = proxy.inner().get_property("Position").await?;
        decode_position_variant(&val)
    };

    tokio::time::timeout(DBUS_QUERY_TIMEOUT, query)
        .await
        .map_err(|_| MprisError::Timeout)?
}

/// Query the playback position for a specific MPRIS player service.
///
/// Returns position in seconds, or an error if the service is unavailable or D-Bus query fails.
/// Uses direct D-Bus `Properties.Get` to bypass proxy caches and eliminate proxy construction overhead.
pub async fn get_position(service: &str) -> Result<f64, MprisError> {
    if service.is_empty() {
        return Ok(0.0);
    }

    let conn = get_dbus_conn().await?;
    let query = async {
        let reply = conn
            .call_method(
                Some(service),
                "/org/mpris/MediaPlayer2",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.mpris.MediaPlayer2.Player", "Position"),
            )
            .await?;

        let val: OwnedVariant = reply.body().deserialize()?;
        decode_position_variant(&val)
    };

    tokio::time::timeout(DBUS_QUERY_TIMEOUT, query)
        .await
        .map_err(|_| MprisError::Timeout)?
}

/// Query the playback status for a specific MPRIS player service.
///
/// Returns "Playing", "Paused", or "Stopped" as a string.
/// Returns "Stopped" if the service is unavailable, on timeout, or on error.
pub async fn get_playback_status(service: &str) -> String {
    if service.is_empty() {
        return "Stopped".to_string();
    }

    let Ok(conn) = get_dbus_conn().await else {
        return "Stopped".to_string();
    };

    let query = async {
        let reply = conn
            .call_method(
                Some(service),
                "/org/mpris/MediaPlayer2",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.mpris.MediaPlayer2.Player", "PlaybackStatus"),
            )
            .await
            .ok()?;

        let val: OwnedVariant = reply.body().deserialize().ok()?;
        match val.deref() {
            Variant::Str(s) => Some(s.as_str().to_string()),
            _ => None,
        }
    };

    tokio::time::timeout(DBUS_QUERY_TIMEOUT, query)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "Stopped".to_string())
}
