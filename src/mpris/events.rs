//! Event watching and handler registration for MPRIS signals.

use crate::mpris::connection::{MprisError, find_active_service, get_dbus_conn};
use crate::mpris::metadata::{TrackMetadata, extract_metadata};
use crate::mpris::playback::get_position;
use crate::mpris::proxies::{MediaPlayer2PlayerProxy, PlayerctldProxy};
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;

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
        Self {
            on_track_change,
            on_seek,
        }
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

/// Handles MPRIS events and manages player state
pub struct MprisEventHandler<C: MprisEventCallback> {
    callback: C,
    block_list: Arc<Vec<String>>,
    target_list: Arc<Vec<String>>,
    state: PlayerState,
    conn: zbus::Connection,
}

impl<C: MprisEventCallback> MprisEventHandler<C> {
    /// Create a new MPRIS event handler
    pub async fn new(
        callback: C,
        block_list: Vec<String>,
        target_list: Vec<String>,
    ) -> Result<Self, MprisError> {
        let conn = get_dbus_conn().await?;

        let mut handler = Self {
            callback,
            block_list: Arc::new(block_list),
            target_list: Arc::new(target_list),
            state: PlayerState::default(),
            conn,
        };

        // Discover initial active player
        let _ = handler.discover_active_player().await;

        Ok(handler)
    }

    /// Main event loop - processes incoming MPRIS and D-Bus signals
    pub async fn handle_events(&mut self) -> Result<(), MprisError> {
        // Subscribe to playerctld property changes to detect recency-ordered player switches
        let playerctld_proxy = PlayerctldProxy::new(&self.conn).await.ok();
        let mut player_names_stream = if let Some(ref proxy) = playerctld_proxy {
            tracing::debug!("Subscribed to playerctld player_names changes");
            Some(proxy.receive_player_names_changed().await)
        } else {
            tracing::debug!(
                "playerctld not available, relying on D-Bus NameOwnerChanged signals and fallback polling"
            );
            None
        };

        // Subscribe to standard D-Bus daemon NameOwnerChanged signals for instant connect/disconnect/transfer detection
        let dbus_proxy = zbus::fdo::DBusProxy::new(&self.conn).await?;
        let mut name_owner_stream = dbus_proxy.receive_name_owner_changed().await?;
        tracing::debug!("Subscribed to D-Bus NameOwnerChanged signals");

        // Main event processing loop
        loop {
            tokio::select! {
                // Handle playerctld player list changes
                Some(_) = async {
                    if let Some(ref mut stream) = player_names_stream {
                        stream.next().await
                    } else {
                        None
                    }
                } => {
                    tracing::debug!("playerctld player list changed, discovering active player");
                    if let Err(e) = self.discover_active_player().await {
                        tracing::warn!(
                            error = %e,
                            "Failed to discover active player on playerctld signal"
                        );
                    }
                }

                // Handle D-Bus NameOwnerChanged signals (instant connect / disconnect / ownership transfer)
                Some(signal) = name_owner_stream.next() => {
                    if let Ok(args) = signal.args() {
                        let name = args.name.as_str();
                        let old_owner = args.old_owner.as_deref().unwrap_or("");
                        let new_owner = args.new_owner.as_deref().unwrap_or("");

                        let is_current = name == self.state.service;
                        let is_mpris = name.starts_with("org.mpris.MediaPlayer2.");
                        let owner_changed = old_owner != new_owner;

                        if (is_current || is_mpris) && owner_changed {
                            tracing::debug!(
                                name = %name,
                                is_current,
                                old_owner = %old_owner,
                                new_owner = %new_owner,
                                "MPRIS player lifecycle event detected via NameOwnerChanged"
                            );
                            if let Err(e) = self.discover_active_player().await {
                                tracing::warn!(
                                    error = %e,
                                    "Failed to discover active player on NameOwnerChanged"
                                );
                            }
                        }
                    }
                }

                // Handle events from current player if active, or idle rediscovery sleep
                res = self.handle_player_events() => {
                    if let Err(e) = res {
                        tracing::warn!(error = %e, "Player event handling failed, backing off...");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
    }

    /// Handle events from the currently active player, or sleep + rediscover if inactive
    async fn handle_player_events(&mut self) -> Result<(), MprisError> {
        if !self.state.is_active() {
            // No active player: sleep for 2s then attempt discovery
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = self.discover_active_player().await {
                tracing::warn!(
                    error = %e,
                    "Failed to discover active player during idle poll"
                );
            }
            return Ok(());
        }

        let service = self.state.service.clone();
        tracing::debug!(service = %service, "Subscribing to player events");

        let proxy = MediaPlayer2PlayerProxy::builder(&self.conn)
            .destination(service.as_str())?
            .build()
            .await?;

        let dbus_proxy = zbus::fdo::DBusProxy::new(&self.conn).await?;

        // Subscribe to signals and property changes
        let mut seeked_stream = proxy.receive_seeked().await?;
        let mut metadata_stream = proxy.receive_metadata_changed().await;
        let mut status_stream = proxy.receive_playback_status_changed().await;

        loop {
            tokio::select! {
                // Handle Seeked signal
                Some(signal) = seeked_stream.next() => {
                    if let Ok(args) = signal.args() {
                        tracing::debug!(service = %service, position = args.position, "Seeked signal received");
                        self.handle_seek_signal(args.position).await;
                    }
                }

                // Handle Metadata property change
                Some(_) = metadata_stream.next() => {
                    tracing::debug!(service = %service, "Metadata changed");
                    if let Err(e) = self.handle_metadata_change(&proxy).await {
                        tracing::warn!(
                            service = %service,
                            error = %e,
                            "Failed to handle metadata change"
                        );
                    }
                }

                // Handle PlaybackStatus property change
                Some(_) = status_stream.next() => {
                    tracing::debug!(service = %service, "Playback status changed");
                    if let Err(e) = self.handle_status_change(&proxy).await {
                        tracing::warn!(
                            service = %service,
                            error = %e,
                            "Failed to handle playback status change"
                        );
                    }
                }

                // Safety-net periodic check (3s timeout avoids 25s D-Bus freeze; uses real D-Bus daemon query)
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    let has_owner = match zbus::names::BusName::try_from(service.as_str()) {
                        Ok(bus_name) => matches!(
                            tokio::time::timeout(
                                Duration::from_secs(3),
                                dbus_proxy.name_has_owner(bus_name)
                            )
                            .await,
                            Ok(Ok(true))
                        ),
                        Err(_) => false,
                    };

                    if !has_owner {
                        tracing::debug!(
                            service = %service,
                            "Player name has no owner or is unreachable, discovering new player"
                        );
                        if let Err(e) = self.discover_active_player().await {
                            tracing::warn!(
                                error = %e,
                                "Failed to discover player after disconnect"
                            );
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
        let active = match find_active_service(&self.block_list, &self.target_list).await {
            Ok(service) => service,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to discover active player due to D-Bus error");
                return Err(e);
            }
        };
        tracing::debug!(active_player = ?active, "Discovered active player");

        match active {
            Some(service) => {
                if service != self.state.service {
                    tracing::debug!(
                        old_service = %self.state.service,
                        new_service = %service,
                        "Switching to player"
                    );
                    self.switch_to_player(&service).await?;
                }
            }
            None => {
                if self.state.is_active() {
                    tracing::debug!(
                        service = %self.state.service,
                        "Deactivating player (no active players)"
                    );
                    self.deactivate_player();
                }
            }
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

        tracing::debug!(
            service = %service,
            title = %metadata.title,
            artist = %metadata.artist,
            position = position,
            status = %playback_status,
            "Switched to player"
        );

        self.state = PlayerState {
            service: service.to_string(),
            track: metadata.clone(),
            playback_status,
            position,
        };

        self.callback
            .on_track_change(metadata, position, service.to_string());

        Ok(())
    }

    fn deactivate_player(&mut self) {
        self.state.clear();
        self.callback
            .on_track_change(TrackMetadata::default(), 0.0, String::new());
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
        target_list: Vec<String>,
    ) -> Result<Self, MprisError> {
        let callback = ClosureCallback::new(on_track_change, on_seek);
        Self::new(callback, block_list, target_list).await
    }
}
