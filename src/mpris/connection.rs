//! D-Bus connection management and player discovery for MPRIS.

use super::proxies::PlayerctldProxy;
use tokio::sync::OnceCell;

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

    // Try playerctld first (recency-ordered active players)
    match PlayerctldProxy::new(&conn).await {
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

    // Fallback: list all names on the bus matching org.mpris.MediaPlayer2.*
    let dbus_proxy = zbus::fdo::DBusProxy::new(&conn).await?;
    let names = dbus_proxy.list_names().await?;
    let mpris_names: Vec<String> = names
        .into_iter()
        .map(|n| n.to_string())
        .filter(|n| n.starts_with("org.mpris.MediaPlayer2."))
        .collect();

    Ok(mpris_names)
}

/// Find the first active MPRIS player service that is not blocked and matches target list if any.
///
/// Returns `Ok(Some(service))` if a player is found, `Ok(None)` if no players match,
/// or `Err(e)` if querying D-Bus failed.
pub async fn find_active_service(
    block: &[String],
    target: &[String],
) -> Result<Option<String>, MprisError> {
    let names = get_active_player_names().await?;
    Ok(names
        .into_iter()
        .find(|s| !is_blocked(s, block) && is_targeted(s, target)))
}

/// Check if a player service name should be blocked
///
/// Returns true if the service name (case-insensitive) contains any blocked string.
pub fn is_blocked(service: &str, block_list: &[String]) -> bool {
    let service_lower = service.to_lowercase();
    block_list
        .iter()
        .any(|blocked| service_lower.contains(&blocked.to_lowercase()))
}

/// Check if a player service is targeted, and should be listened to
///
/// Returns true if the service name (case-insensitive) is wanted by the target flag
pub fn is_targeted(service: &str, target_list: &[String]) -> bool {
    // If target_list is empty, return true
    if target_list.is_empty() {
        return true;
    }

    let service_lower = service.to_lowercase();
    target_list
        .iter()
        .any(|target| service_lower.contains(&target.to_lowercase()))
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
}
