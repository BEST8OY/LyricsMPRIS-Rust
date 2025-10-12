//! Minimal D-Bus connection and player discovery for MPRIS.

use std::sync::Arc;

#[derive(thiserror::Error, Debug)]
pub enum MprisError {
    #[error("D-Bus error: {0}")]
    ZBus(#[from] zbus::Error),
    #[error("No connection to D-Bus")]
    NoConnection,
}

pub async fn get_dbus_conn() -> Result<Arc<zbus::Connection>, MprisError> {
    static ONCE: once_cell::sync::OnceCell<Arc<zbus::Connection>> = once_cell::sync::OnceCell::new();
    if let Some(conn) = ONCE.get() {
        return Ok(conn.clone());
    }
    // Create a session connection using tokio integration
    let conn = zbus::Connection::session().await.map_err(|_| MprisError::NoConnection)?;
    let arc = Arc::new(conn);
    let _ = ONCE.set(arc.clone());
    Ok(arc)
}

pub async fn get_active_player_names() -> Result<Vec<String>, MprisError> {
    let conn = get_dbus_conn().await?;
    // Use zbus Proxy to get the PlayerNames property
    let proxy = zbus::Proxy::new(
        &conn,
        "org.mpris.MediaPlayer2.playerctld",
        "/org/mpris/MediaPlayer2",
        "com.github.altdesktop.playerctld",
    )
    .await?;

    // The property is a[string]
    let value: Result<Vec<String>, zbus::Error> = proxy.get_property("PlayerNames").await;
    Ok(value.unwrap_or_default())
}

pub fn is_blocked(service: &str, block_list: &[String]) -> bool {
    block_list
        .iter()
        .any(|b| service.to_lowercase().contains(b))
}
