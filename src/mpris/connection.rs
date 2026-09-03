//! D-Bus connection management and player discovery for MPRIS.

use super::proxies::PlayerctldProxy;
use std::ops::Deref;
use tokio::sync::OnceCell;
use zbus::fdo::DBusProxy;
use zbus::names::BusName;
use zvariant::OwnedValue;

/// Errors that can occur during MPRIS operations
#[derive(thiserror::Error, Debug)]
pub enum MprisError {
    #[error("D-Bus error: {0}")]
    ZBus(#[from] zbus::Error),
    #[error("D-Bus FDO error: {0}")]
    Fdo(#[from] zbus::fdo::Error),
    #[error("Failed to establish D-Bus connection: {0}")]
    NoConnection(zbus::Error),
}

/// Global D-Bus connection singleton
static DBUS_CONNECTION: OnceCell<zbus::Connection> = OnceCell::const_new();

/// Get or create a shared D-Bus session connection
pub async fn get_dbus_conn() -> Result<zbus::Connection, MprisError> {
    DBUS_CONNECTION
        .get_or_try_init(|| async {
            zbus::Connection::session()
                .await
                .map_err(MprisError::NoConnection)
        })
        .await
        .cloned()
}

/// Get list of active MPRIS player service names.
///
/// Queries `playerctld` first (recency-ordered active players).
/// If `playerctld` is unavailable or returns an empty list, falls back to
/// querying the standard D-Bus daemon (`ListNames`) for any `org.mpris.MediaPlayer2.*` names.
pub async fn get_active_player_names() -> Result<Vec<String>, MprisError> {
    let conn = get_dbus_conn().await?;
    let dbus_proxy = DBusProxy::new(&conn).await?;

    // Tier 1: Query playerctld (recency-ordered active players)
    // Check name_has_owner first to prevent triggering unwanted D-Bus auto-activation
    if dbus_proxy
        .name_has_owner(BusName::from_static_str("org.mpris.MediaPlayer2.playerctld").unwrap())
        .await
        .unwrap_or(false)
    {
        let playerctld_proxy = PlayerctldProxy::builder(&conn)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await;

        match playerctld_proxy {
            Ok(proxy) => match proxy.player_names().await {
                Ok(names) if !names.is_empty() => return Ok(names),
                Ok(_) => {
                    tracing::debug!(
                        "playerctld returned empty player list; falling back to DBus ListNames"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "playerctld player_names property failed; falling back to DBus ListNames"
                    );
                }
            },
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "playerctld proxy unavailable; falling back to DBus ListNames"
                );
            }
        }
    }

    // Fallback: list all names on the bus matching org.mpris.MediaPlayer2.*
    let names = dbus_proxy.list_names().await?;
    let mpris_names: Vec<String> = names
        .into_iter()
        .map(|n| n.to_string())
        .filter(|n| {
            n.starts_with("org.mpris.MediaPlayer2.") && n != "org.mpris.MediaPlayer2.playerctld"
        })
        .collect();

    Ok(mpris_names)
}

/// Playback priority score for active player selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlaybackPriority {
    Unresponsive = 0,
    Stopped = 1,
    Paused = 2,
    Playing = 3,
}

impl PlaybackPriority {
    pub fn from_status_str(status: &str) -> Self {
        match status {
            "Playing" => PlaybackPriority::Playing,
            "Paused" => PlaybackPriority::Paused,
            "Stopped" => PlaybackPriority::Stopped,
            _ => PlaybackPriority::Unresponsive,
        }
    }
}

/// Probe a player's PlaybackStatus with a short timeout to prevent slow or wedged players from hanging.
pub async fn query_player_priority(conn: &zbus::Connection, service: &str) -> PlaybackPriority {
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
        let val: OwnedValue = reply.body().deserialize().ok()?;
        let status = match val.deref() {
            zvariant::Value::Str(s) => s.as_str(),
            _ => return None,
        };
        Some(PlaybackPriority::from_status_str(status))
    };

    tokio::time::timeout(std::time::Duration::from_millis(50), query)
        .await
        .ok()
        .flatten()
        .unwrap_or(PlaybackPriority::Unresponsive)
}

/// Find the optimal active MPRIS player service prioritizing playback state (`Playing` > `Paused` > `Stopped`).
///
/// If `current_service` is provided and has the highest priority level found, it is retained to avoid
/// unnecessary bouncing between equal-priority players. If `preferred_sender_unique` matches a player's
/// unique bus name and that player is `Playing`, it is given precedence.
pub async fn find_best_active_service(
    block_list: &[String],
    target_list: &[String],
    current_service: Option<&str>,
    preferred_sender_unique: Option<&str>,
) -> Result<Option<String>, MprisError> {
    let names = get_active_player_names().await?;
    let candidates: Vec<String> = names
        .into_iter()
        .filter(|s| !is_blocked(s, block_list) && is_targeted(s, target_list))
        .collect();

    if candidates.is_empty() {
        return Ok(None);
    }

    if candidates.len() == 1 {
        return Ok(candidates.into_iter().next());
    }

    let conn = get_dbus_conn().await?;
    let dbus_proxy = DBusProxy::new(&conn).await?;

    // Query status and unique bus name for each candidate concurrently
    let query_futs = candidates.iter().map(|svc| {
        let conn = conn.clone();
        let dbus_proxy = &dbus_proxy;
        async move {
            let priority = query_player_priority(&conn, svc).await;
            let is_preferred = if let Some(target_unique) = preferred_sender_unique {
                if let Ok(bus_name) = BusName::try_from(svc.as_str()) {
                    dbus_proxy
                        .get_name_owner(bus_name)
                        .await
                        .ok()
                        .map(|u| u.as_str() == target_unique)
                        .unwrap_or(false)
                } else {
                    false
                }
            } else {
                false
            };
            (priority, is_preferred)
        }
    });

    let results = futures_util::future::join_all(query_futs).await;

    // Structure: (service_name, priority, is_preferred, original_index)
    let mut scored: Vec<(&String, PlaybackPriority, bool, usize)> = candidates
        .iter()
        .zip(results)
        .enumerate()
        .map(|(idx, (svc, (prio, is_pref)))| (svc, prio, is_pref, idx))
        .collect();

    // Sort:
    // 1. PlaybackPriority descending
    // 2. Preferred sender descending (true > false)
    // 3. Original list order ascending (stable)
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.3.cmp(&b.3))
    });

    let (best_service, best_priority, best_is_pref, _) = scored[0];

    // Stability check: If current_service is still alive and has the same best priority (e.g. both are Paused),
    // keep current_service instead of hopping arbitrarily, unless a new preferred player is explicitly Playing.
    if let Some(curr) = current_service
        && !best_is_pref
        && let Some((_, curr_priority, _, _)) =
            scored.iter().find(|(s, _, _, _)| s.as_str() == curr)
        && *curr_priority >= best_priority
    {
        return Ok(Some(curr.to_string()));
    }

    Ok(Some(best_service.clone()))
}

/// Find the first active MPRIS player service that is not blocked and matches target list if any.
///
/// Returns `Ok(Some(service))` if a player is found, `Ok(None)` if no players match,
/// or `Err(e)` if querying D-Bus failed.
pub async fn find_active_service(
    block: &[String],
    target: &[String],
) -> Result<Option<String>, MprisError> {
    find_best_active_service(block, target, None, None).await
}

/// Check if a player service name should be blocked
///
/// Returns true if the service name (case-insensitive) contains any blocked string.
pub fn is_blocked(service: &str, block_list: &[String]) -> bool {
    let s_bytes = service.as_bytes();
    block_list.iter().any(|blocked| {
        let b_bytes = blocked.as_bytes();
        if b_bytes.is_empty() {
            return false;
        }
        s_bytes
            .windows(b_bytes.len())
            .any(|window| window.eq_ignore_ascii_case(b_bytes))
    })
}

/// Check if a player service is targeted, and should be listened to
///
/// Returns true if the service name (case-insensitive) is wanted by the target flag
pub fn is_targeted(service: &str, target_list: &[String]) -> bool {
    // If target_list is empty, return true
    if target_list.is_empty() {
        return true;
    }

    let s_bytes = service.as_bytes();
    target_list.iter().any(|target| {
        let t_bytes = target.as_bytes();
        if t_bytes.is_empty() {
            return false;
        }
        s_bytes
            .windows(t_bytes.len())
            .any(|window| window.eq_ignore_ascii_case(t_bytes))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_blocked() {
        let block_list = vec!["chromium".to_string(), "firefox".to_string()];
        assert!(is_blocked(
            "org.mpris.MediaPlayer2.chromium.instance123",
            &block_list
        ));
        assert!(is_blocked("org.mpris.MediaPlayer2.Firefox", &block_list));
        assert!(!is_blocked("org.mpris.MediaPlayer2.spotify", &block_list));
        assert!(!is_blocked("org.mpris.MediaPlayer2.mpv", &[]));
    }

    #[test]
    fn test_is_targeted() {
        let target_list = vec!["spotify".to_string(), "mpv".to_string()];
        assert!(is_targeted("org.mpris.MediaPlayer2.spotify", &target_list));
        assert!(is_targeted(
            "org.mpris.MediaPlayer2.mpv.instance1",
            &target_list
        ));
        assert!(!is_targeted("org.mpris.MediaPlayer2.vlc", &target_list));
        // Empty target list allows all
        assert!(is_targeted("org.mpris.MediaPlayer2.vlc", &[]));
    }

    #[test]
    fn test_mpris_error_display() {
        let err = MprisError::NoConnection(zbus::Error::Address("test address".into()));
        assert!(
            err.to_string()
                .contains("Failed to establish D-Bus connection")
        );
    }

    #[test]
    fn test_playback_priority_ordering() {
        assert!(PlaybackPriority::Playing > PlaybackPriority::Paused);
        assert!(PlaybackPriority::Paused > PlaybackPriority::Stopped);
        assert!(PlaybackPriority::Stopped > PlaybackPriority::Unresponsive);
    }

    #[test]
    fn test_playback_priority_from_status_str() {
        assert_eq!(
            PlaybackPriority::from_status_str("Playing"),
            PlaybackPriority::Playing
        );
        assert_eq!(
            PlaybackPriority::from_status_str("Paused"),
            PlaybackPriority::Paused
        );
        assert_eq!(
            PlaybackPriority::from_status_str("Stopped"),
            PlaybackPriority::Stopped
        );
        assert_eq!(
            PlaybackPriority::from_status_str("Unknown"),
            PlaybackPriority::Unresponsive
        );
        assert_eq!(
            PlaybackPriority::from_status_str(""),
            PlaybackPriority::Unresponsive
        );
    }
}
