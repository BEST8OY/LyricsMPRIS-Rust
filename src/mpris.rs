/// Fetch only the playback position for a known service (async).
pub async fn get_position_for_service(service: &str) -> Result<f64, MprisError> {
    init_dbus_connection().await?;
    let conn = get_shared_connection()?;
    let player_proxy = Proxy::new(service, "/org/mpris/MediaPlayer2", TIMEOUT, conn.clone());
    let position: Option<i64> = Properties::get(&player_proxy, "org.mpris.MediaPlayer2.Player", "Position")
        .await
        .map_err(|e| {
            e
        })
        .ok();
    Ok(position.map(|p| p as f64 / 1_000_000.0).unwrap_or(0.0))
}
// mpris.rs: Fully async MPRIS client for metadata, position, and event watching

// --- Imports ---
use dbus::nonblock::{SyncConnection, Proxy};
use dbus::message::MatchRule;
use dbus::nonblock::stdintf::org_freedesktop_dbus::Properties;
use dbus::channel::MatchingReceiver;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use thiserror::Error;
use once_cell::sync::OnceCell;

// --- Constants ---
const TIMEOUT: Duration = Duration::from_millis(5000);

// --- Types ---
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MprisPlayer {
    pub service: String,
    pub playback_status: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub position: Option<i64>,
}

impl MprisPlayer {
    /// Convert player info to TrackMetadata
    pub fn to_metadata(&self) -> TrackMetadata {
        TrackMetadata {
            title: self.title.clone().unwrap_or_default(),
            artist: self.artist.clone().unwrap_or_default(),
            album: self.album.clone().unwrap_or_default(),
        }
    }
    /// Get playback position in seconds
    pub fn position_seconds(&self) -> f64 {
        self.position.map(|p| p as f64 / 1_000_000.0).unwrap_or(0.0)
    }
}

// --- Error Types ---
#[derive(Error, Debug)]
pub enum MprisError {
    #[error("DBus error: {0}")]
    DBus(#[from] dbus::Error),
    #[error("Tokio timer error: {0}")]
    Timer(#[from] tokio::time::error::Elapsed),
    #[error("No connection to D-Bus")]
    NoConnection,
    #[error("Player not found")]
    PlayerNotFound,
}

// --- Global State ---
static DBUS_CONN: OnceCell<Arc<SyncConnection>> = OnceCell::new();

// --- Internal Utilities ---
/// Initialize the global D-Bus connection if not already present.
pub async fn init_dbus_connection() -> Result<(), MprisError> {
    if DBUS_CONN.get().is_none() {
        let (resource, conn) = dbus_tokio::connection::new_session_sync()
            .map_err(|_| MprisError::NoConnection)?;
        tokio::spawn(async move { resource.await });
        DBUS_CONN.set(conn).ok();
    }
    Ok(())
}

fn get_shared_connection() -> Result<Arc<SyncConnection>, MprisError> {
    DBUS_CONN.get().cloned().ok_or(MprisError::NoConnection)
}

fn extract_metadata(map: &dbus::arg::PropMap) -> (Option<String>, Option<String>, Option<String>) {
    let title = map.get("xesam:title").and_then(|v| v.0.as_str()).map(str::to_string);
    let artist = map.get("xesam:artist")
        .and_then(|v| v.0.as_iter())
        .and_then(|mut it| it.next())
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let album = map.get("xesam:album").and_then(|v| v.0.as_str()).map(str::to_string);
    (title, artist, album)
}

fn default_config() -> crate::Config {
    crate::Config { block: vec![], ..Default::default() }
}

// --- Public API ---
/// Gets the list of active player names from playerctld
async fn get_active_player_names() -> Result<Vec<String>, MprisError> {
    init_dbus_connection().await?;
    let conn = get_shared_connection()?;
    let proxy = Proxy::new(
        "org.mpris.MediaPlayer2.playerctld",
        "/org/mpris/MediaPlayer2",
        TIMEOUT,
        conn.clone(),
    );
    let result = Properties::get(&proxy, "com.github.altdesktop.playerctld", "PlayerNames")
        .await;
    Ok(result.unwrap_or_default())
}

/// Finds the first unblocked player name from the active players list
async fn find_first_unblocked_player(config: &crate::Config) -> Result<Option<String>, MprisError> {
    let names = get_active_player_names().await?;
    Ok(names.into_iter().find(|service| {
        !config.block.iter().any(|b| service.to_lowercase().contains(b))
    }))
}

/// Fetch a player by its D-Bus service name (async).
pub async fn get_player_by_service(service: &str) -> Result<MprisPlayer, MprisError> {
    init_dbus_connection().await?;
    let conn = get_shared_connection()?;
    let player_proxy = Proxy::new(service, "/org/mpris/MediaPlayer2", TIMEOUT, conn.clone());
    let playback_status: Option<String> = Properties::get(&player_proxy, "org.mpris.MediaPlayer2.Player", "PlaybackStatus")
        .await
        .map_err(|e| {
            e
        })
        .ok();
    let metadata: Option<dbus::arg::PropMap> = Properties::get(&player_proxy, "org.mpris.MediaPlayer2.Player", "Metadata")
        .await
        .map_err(|e| {
            e
        })
        .ok();
    let (title, artist, album) = metadata.as_ref().map_or((None, None, None), |map| extract_metadata(map));
    let position: Option<i64> = Properties::get(&player_proxy, "org.mpris.MediaPlayer2.Player", "Position")
        .await
        .map_err(|e| {
            e
        })
        .ok();
    if let Some(playback_status) = playback_status {
        Ok(MprisPlayer {
            service: service.to_string(),
            playback_status,
            title,
            artist,
            album,
            position,
        })
    } else {
        Err(MprisError::PlayerNotFound)
    }
}

/// Returns the first non-blocked MPRIS player, or None if none are available.
pub async fn select_player(config: Option<&crate::Config>) -> Result<Option<MprisPlayer>, MprisError> {
    let config_owned = config.map_or_else(default_config, |c| c.clone());
    // First find an unblocked player without querying properties
    if let Some(service) = find_first_unblocked_player(&config_owned).await? {
        // Only query properties for the first unblocked player
        get_player_by_service(&service).await.map(Some)
    } else {
        Ok(None)
    }
}

/// Returns the current MPRIS player, or None if not available.
pub async fn get_current_player(config: Option<&crate::Config>) -> Result<Option<MprisPlayer>, MprisError> {
    select_player(config).await
}

/// Returns the current track metadata for the given config (or default config).
pub async fn get_metadata(config: Option<&crate::Config>) -> Result<TrackMetadata, MprisError> {
    Ok(get_current_player(config).await?.map(|p| p.to_metadata()).unwrap_or_default())
}

/// Returns the current playback position (in seconds) for the given config (or default config).
pub async fn get_position(config: Option<&crate::Config>) -> Result<f64, MprisError> {
    Ok(get_current_player(config).await?.map(|p| p.position_seconds()).unwrap_or(0.0))
}

/// Returns the current playback status for the given config (or default config).
pub async fn get_playback_status(config: Option<&crate::Config>) -> Result<String, MprisError> {
    Ok(get_current_player(config).await?.map(|p| p.playback_status).unwrap_or_else(|| "Stopped".to_string()))
}

/// Watches for MPRIS property change signals and invokes the provided callbacks.
pub async fn watch_and_handle_events<F, G>(
    mut on_track_change: F,
    mut on_seek: G,
    config: Option<&crate::Config>,
) -> Result<(), MprisError>
where
    F: FnMut(TrackMetadata, f64, String) + Send + 'static,
    G: FnMut(TrackMetadata, f64) + Send + 'static,
{
    let (resource, conn) = dbus_tokio::connection::new_session_sync()?;
    tokio::spawn(async move { resource.await });
    let conn = Arc::new(conn);

    // Only watch for PropertiesChanged
    let rule = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
    conn.add_match(rule.clone()).await?;

    let (tx, mut rx) = mpsc::channel::<dbus::message::Message>(8);
    let conn2 = Arc::clone(&conn);
    MatchingReceiver::start_receive(
        &**conn2,
        rule,
        Box::new(move |msg, _| {
            let _ = tx.try_send(msg);
            true
        }),
    );

    let mut current_service = String::new();
    let mut last_track = TrackMetadata::default();
    let mut last_playback_status = String::new();

    // Get initial player and state
    if let Some(player) = select_player(config).await? {
        let meta = player.to_metadata();
        let pos = player.position_seconds();
        current_service = player.service.clone();
        last_track = meta.clone();
        last_playback_status = player.playback_status;
        on_track_change(meta, pos, player.service);
    }

    loop {
        // Always check for player change by querying PlayerNames
        if let Ok(names) = get_active_player_names().await {
            if let Some(service) = names.first() {
                if *service != current_service {
                    // Player changed
                    if let Ok(player) = get_player_by_service(service).await {
                        let meta = player.to_metadata();
                        let pos = player.position_seconds();
                        current_service = player.service.clone();
                        last_track = meta.clone();
                        last_playback_status = player.playback_status;
                        on_track_change(meta, pos, player.service);
                    }
                }
            }
        }

        if let Some(msg) = rx.recv().await {

            // Handle property changes
            if msg.interface().as_deref() == Some("org.freedesktop.DBus.Properties") &&
               msg.read1::<&str>().ok() == Some("org.mpris.MediaPlayer2.Player") {
                // Only process if we have a current player
                if current_service.is_empty() {
                    continue;
                }

                let player_proxy = Proxy::new(
                    &current_service,
                    "/org/mpris/MediaPlayer2",
                    TIMEOUT,
                    get_shared_connection()?.clone(),
                );

                let changed: Option<dbus::arg::PropMap> = msg.read2().ok().map(|(_, c): (String, dbus::arg::PropMap)| c);
                if let Some(changed) = changed {
                    // Metadata or PlaybackStatus changed - query only what changed
                    let mut metadata_changed = false;
                    let mut status_changed = false;

                    if changed.contains_key("Metadata") {
                        if let Ok(metadata) = Properties::get::<dbus::arg::PropMap>(&player_proxy, "org.mpris.MediaPlayer2.Player", "Metadata").await {
                            let (title, artist, album) = extract_metadata(&metadata);
                            let new_track = TrackMetadata {
                                title: title.unwrap_or_default(),
                                artist: artist.unwrap_or_default(),
                                album: album.unwrap_or_default(),
                            };
                            if new_track != last_track {
                                last_track = new_track;
                                metadata_changed = true;
                            }
                        } else {
                        }
                    }

                    if changed.contains_key("PlaybackStatus") {
                        if let Ok(status) = Properties::get::<String>(&player_proxy, "org.mpris.MediaPlayer2.Player", "PlaybackStatus").await {
                            if status != last_playback_status {
                                last_playback_status = status;
                                status_changed = true;
                            }
                        } else {
                        }
                    }

                    // Only get position if metadata or status changed (we need all info for track change)
                    if metadata_changed || status_changed {
                        let position = Properties::get::<i64>(&player_proxy, "org.mpris.MediaPlayer2.Player", "Position")
                            .await
                            .map(|p| p as f64 / 1_000_000.0)
                            .unwrap_or(0.0);
                        on_track_change(last_track.clone(), position, current_service.clone());
                    } else if let Some(pos_var) = changed.get("Position") {
                        // For position updates, use the value directly from the signal
                        if let Some(pos) = pos_var.0.as_i64() {
                            let sec = pos as f64 / 1_000_000.0;
                            
                            on_seek(last_track.clone(), sec);
                        }
                    }
                }
            }
        } else {
            break;
        }
    }
    Ok(())
}
