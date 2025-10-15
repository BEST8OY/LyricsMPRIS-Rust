//! Event watching and handler registration for MPRIS signals.

use crate::mpris::connection::{get_active_player_names, get_dbus_conn, is_blocked, MprisError};
use crate::mpris::metadata::{extract_metadata, TrackMetadata};
use crate::mpris::playback::get_position;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use zbus::proxy;
use zvariant::OwnedValue;

/// Callback trait for MPRIS events
pub trait MprisEventCallback: Send + 'static {
    fn on_track_change(&mut self, metadata: TrackMetadata, position: f64, service: String);
    fn on_seek(&mut self, metadata: TrackMetadata, position: f64, service: String);
}

/// Simple callback implementation using closures
pub struct ClosureCallback<F, G>
where
    F: FnMut(TrackMetadata, f64, String) + Send + 'static,
    G: FnMut(TrackMetadata, f64, String) + Send + 'static,
{
    on_track_change: F,
    on_seek: G,
}

impl<F, G> ClosureCallback<F, G>
where
    F: FnMut(TrackMetadata, f64, String) + Send + 'static,
    G: FnMut(TrackMetadata, f64, String) + Send + 'static,
{
    pub fn new(on_track_change: F, on_seek: G) -> Self {
        Self { on_track_change, on_seek }
    }
}

impl<F, G> MprisEventCallback for ClosureCallback<F, G>
where
    F: FnMut(TrackMetadata, f64, String) + Send + 'static,
    G: FnMut(TrackMetadata, f64, String) + Send + 'static,
{
    fn on_track_change(&mut self, metadata: TrackMetadata, position: f64, service: String) {
        (self.on_track_change)(metadata, position, service);
    }

    fn on_seek(&mut self, metadata: TrackMetadata, position: f64, service: String) {
        (self.on_seek)(metadata, position, service);
    }
}

/// Represents the current state of the active player
#[derive(Debug, Clone, Default)]
struct PlayerState {
    service: String,
    track: TrackMetadata,
    playback_status: String,
    position: f64,
}

impl PlayerState {
    fn is_active(&self) -> bool {
        !self.service.is_empty()
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// MPRIS MediaPlayer2.Player interface proxy
#[proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait MediaPlayer2Player {
    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    #[zbus(property)]
    fn position(&self) -> zbus::Result<i64>;

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;

    #[zbus(signal)]
    fn seeked(&self, position: i64) -> zbus::Result<()>;
}

/// Playerctld interface proxy for player management
#[proxy(
    interface = "com.github.altdesktop.playerctld",
    default_service = "org.mpris.MediaPlayer2.playerctld",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait Playerctld {
    #[zbus(property)]
    fn player_names(&self) -> zbus::Result<Vec<String>>;
}

/// Handles MPRIS events and manages player state
pub struct MprisEventHandler<C: MprisEventCallback> {
    callback: C,
    block_list: Arc<Vec<String>>,
    state: PlayerState,
    conn: Arc<zbus::Connection>,
}

impl<C: MprisEventCallback> MprisEventHandler<C> {
    /// Create a new MPRIS event handler
    pub async fn new(callback: C, block_list: Vec<String>) -> Result<Self, MprisError> {
        let conn = get_dbus_conn().await?;

        let mut handler = Self {
            callback,
            block_list: Arc::new(block_list),
            state: PlayerState::default(),
            conn: conn.clone(),
        };

        // Discover initial active player
        handler.discover_active_player().await?;

        Ok(handler)
    }

    /// Main event loop - processes incoming MPRIS signals
    pub async fn handle_events(&mut self) -> Result<(), MprisError> {
        // Subscribe to playerctld property changes to detect player switches
        let playerctld_proxy = PlayerctldProxy::new(&self.conn).await.ok();

        let mut player_names_stream = if let Some(ref proxy) = playerctld_proxy {
            Some(proxy.receive_player_names_changed().await)
        } else {
            None
        };

        // Main event processing loop
        loop {
            tokio::select! {
                // Handle playerctld PlayerNames property changes
                Some(_) = async {
                    if let Some(ref mut stream) = player_names_stream {
                        stream.next().await
                    } else {
                        None
                    }
                } => {
                    if let Err(e) = self.discover_active_player().await {
                        eprintln!("Error discovering active player: {}", e);
                    }
                }
                
                // Handle events from current player if active
                _ = self.handle_player_events() => {}
            }
        }
    }

    /// Handle events from the currently active player
    async fn handle_player_events(&mut self) -> Result<(), MprisError> {
        if !self.state.is_active() {
            // No active player, wait a bit before checking again
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            return Ok(());
        }

        let service = self.state.service.clone();
        
        let proxy = MediaPlayer2PlayerProxy::builder(&self.conn)
            .destination(service.as_str())?
            .build()
            .await?;

        // Subscribe to signals and property changes
        let mut seeked_stream = proxy.receive_seeked().await?;
        let mut metadata_stream = proxy.receive_metadata_changed().await;
        let mut position_stream = proxy.receive_position_changed().await;
        let mut status_stream = proxy.receive_playback_status_changed().await;

        loop {
            tokio::select! {
                // Handle Seeked signal
                Some(signal) = seeked_stream.next() => {
                    if let Ok(args) = signal.args() {
                        self.handle_seek_signal(args.position).await;
                    }
                }
                
                // Handle Metadata property change
                Some(_) = metadata_stream.next() => {
                    if let Err(e) = self.handle_metadata_change(&proxy).await {
                        eprintln!("Error handling metadata change: {}", e);
                    }
                }
                
                // Handle Position property change (not common, but some players use it)
                Some(_) = position_stream.next() => {
                    if let Err(e) = self.handle_position_change(&proxy).await {
                        eprintln!("Error handling position change: {}", e);
                    }
                }
                
                // Handle PlaybackStatus property change
                Some(_) = status_stream.next() => {
                    if let Err(e) = self.handle_status_change(&proxy).await {
                        eprintln!("Error handling status change: {}", e);
                    }
                }
                
                // Check if we should switch to a different player
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                    // Periodically check if the service is still valid
                    // This handles cases where the player disconnects
                    if proxy.playback_status().await.is_err() {
                        // Player disconnected, try to discover a new one
                        if let Err(e) = self.discover_active_player().await {
                            eprintln!("Error discovering player after disconnect: {}", e);
                        }
                        break; // Exit inner loop to restart with new player
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_seek_signal(&mut self, position_microsecs: i64) {
        let position = position_microsecs as f64 / 1_000_000.0;
        self.state.position = position;
        self.callback.on_seek(
            self.state.track.clone(),
            position,
            self.state.service.clone(),
        );
    }

    async fn handle_metadata_change(
        &mut self,
        proxy: &MediaPlayer2PlayerProxy<'_>,
    ) -> Result<(), MprisError> {
        let metadata_map = proxy.metadata().await?;
        let new_track = extract_metadata(&metadata_map);
        
        if new_track != self.state.track {
            self.state.track = new_track;
            
            // Also update position when track changes
            if let Ok(pos_microsecs) = proxy.position().await {
                self.state.position = pos_microsecs as f64 / 1_000_000.0;
            }
            
            self.callback.on_track_change(
                self.state.track.clone(),
                self.state.position,
                self.state.service.clone(),
            );
        }

        Ok(())
    }

    async fn handle_position_change(
        &mut self,
        proxy: &MediaPlayer2PlayerProxy<'_>,
    ) -> Result<(), MprisError> {
        if let Ok(pos_microsecs) = proxy.position().await {
            let position = pos_microsecs as f64 / 1_000_000.0;
            self.state.position = position;
            self.callback.on_seek(
                self.state.track.clone(),
                position,
                self.state.service.clone(),
            );
        }

        Ok(())
    }

    async fn handle_status_change(
        &mut self,
        proxy: &MediaPlayer2PlayerProxy<'_>,
    ) -> Result<(), MprisError> {
        if let Ok(status) = proxy.playback_status().await
            && status != self.state.playback_status
        {
            self.state.playback_status = status;
            
            // Get fresh position on playback status change
            let position = if let Ok(pos) = get_position(&self.state.service).await {
                self.state.position = pos;
                pos
            } else {
                self.state.position
            };
            
            // Notify about the playback status change
            self.callback.on_track_change(
                self.state.track.clone(),
                position,
                self.state.service.clone(),
            );
        }

        Ok(())
    }

    /// Discovers and switches to the active unblocked player
    async fn discover_active_player(&mut self) -> Result<(), MprisError> {
        let names = get_active_player_names().await?;

        if let Some(service) = names.iter().find(|s| !is_blocked(s, &self.block_list)) {
            if *service != self.state.service {
                self.switch_to_player(service).await?;
            }
        } else if self.state.is_active() {
            // No active players found, but we had one before
            self.deactivate_player();
        }

        Ok(())
    }

    async fn switch_to_player(&mut self, service: &str) -> Result<(), MprisError> {
        let proxy = MediaPlayer2PlayerProxy::builder(&self.conn)
            .destination(service)?
            .build()
            .await?;

        // Fetch initial state
        let metadata = proxy
            .metadata()
            .await
            .map(|map| extract_metadata(&map))
            .unwrap_or_default();
        
        let position = proxy
            .position()
            .await
            .map(|microsecs| microsecs as f64 / 1_000_000.0)
            .unwrap_or(0.0);
        
        let playback_status = proxy
            .playback_status()
            .await
            .unwrap_or_else(|_| "Stopped".to_string());

        self.state = PlayerState {
            service: service.to_string(),
            track: metadata.clone(),
            playback_status,
            position,
        };

        self.callback.on_track_change(metadata, position, service.to_string());

        Ok(())
    }

    fn deactivate_player(&mut self) {
        self.state.clear();
        self.callback.on_track_change(
            TrackMetadata::default(),
            0.0,
            String::new(),
        );
    }
}
// Convenience constructor for closure-based callbacks
impl<F, G> MprisEventHandler<ClosureCallback<F, G>>
where
    F: FnMut(TrackMetadata, f64, String) + Send + 'static,
    G: FnMut(TrackMetadata, f64, String) + Send + 'static,
{
    /// Create an event handler with closure-based callbacks
    pub async fn with_closures(
        on_track_change: F,
        on_seek: G,
        block_list: Vec<String>,
    ) -> Result<Self, MprisError> {
        let callback = ClosureCallback::new(on_track_change, on_seek);
        Self::new(callback, block_list).await
    }
}
