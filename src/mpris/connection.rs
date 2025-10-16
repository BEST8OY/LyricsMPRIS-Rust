//! D-Bus connection management and player discovery for MPRIS.

use std::sync::Arc;
use tokio::sync::OnceCell;
use zbus::proxy;

/// Errors that can occur during MPRIS operations
#[derive(thiserror::Error, Debug)]
pub enum MprisError {
    #[error("D-Bus error: {0}")]
    ZBus(#[from] zbus::Error),
    #[error("Failed to establish D-Bus connection")]
    NoConnection,
}

/// Global D-Bus connection singleton
static DBUS_CONNECTION: OnceCell<Arc<zbus::Connection>> = OnceCell::const_new();

/// Get or create a shared D-Bus session connection
pub async fn get_dbus_conn() -> Result<Arc<zbus::Connection>, MprisError> {
    DBUS_CONNECTION
        .get_or_try_init(|| async {
            let conn = zbus::Connection::session()
                .await
                .map_err(|_| MprisError::NoConnection)?;
            Ok(Arc::new(conn))
        })
        .await
        .cloned()
}

/// Proxy interface for playerctld to get active MPRIS players
#[proxy(
    interface = "com.github.altdesktop.playerctld",
    default_service = "org.mpris.MediaPlayer2.playerctld",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait Playerctld {
    #[zbus(property)]
    fn player_names(&self) -> zbus::Result<Vec<String>>;
}

/// Get list of active MPRIS player service names
/// 
/// This queries playerctld if available, otherwise returns an empty list.
pub async fn get_active_player_names() -> Result<Vec<String>, MprisError> {
    let conn = get_dbus_conn().await?;
    
    match PlayerctldProxy::new(&conn).await {
        Ok(proxy) => {
            proxy.player_names().await.or(Ok(Vec::new()))
        }
        Err(_) => Ok(Vec::new()),
    }
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
