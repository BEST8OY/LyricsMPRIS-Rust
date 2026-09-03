//! Event watching and handler registration for MPRIS signals.

use crate::mpris::connection::{MprisError, find_best_active_service, get_dbus_conn};
use crate::mpris::metadata::{TrackMetadata, extract_metadata};
use crate::mpris::proxies::{MediaPlayer2PlayerProxy, PlayerctldProxy};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::ops::Deref;
use std::pin::Pin;
use std::time::Duration;
use zbus::fdo::NameOwnerChangedStream;
use zbus::{MatchRule, MessageStream};
use zvariant::OwnedValue;

/// Tuning constants for transient buffering position calibration.
pub const CALIBRATION_INITIAL_DELAY: Duration = Duration::from_millis(50);
pub const CALIBRATION_CONFIRMATION_INTERVAL: Duration = Duration::from_millis(250);
pub const CALIBRATION_STEADY_POLL_INTERVAL: Duration = Duration::from_millis(1000);
pub const MAX_CALIBRATION_ATTEMPTS: u8 = 35;
pub const MAX_ERROR_ATTEMPTS: u8 = 8;
pub const MOVEMENT_DELTA_THRESHOLD_SECS: f64 = 0.03;

/// Returns the adaptive delay for buffering edge-detection probes.
fn probe_delay(attempt: u8) -> Duration {
    match attempt {
        0 => CALIBRATION_INITIAL_DELAY,
        1 => Duration::from_millis(100),
        2 => Duration::from_millis(150),
        3 => Duration::from_millis(250),
        4 => Duration::from_millis(400),
        5 => Duration::from_millis(600),
        _ => CALIBRATION_STEADY_POLL_INTERVAL,
    }
}

/// Transient calibration state machine for detecting when a player actually starts
/// advancing audio position after buffering.
#[derive(Debug)]
struct CalibrationTracker {
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
    attempts: u8,
    error_attempts: u8,
    confirmed: bool,
    anchor_pos: f64,
}

impl CalibrationTracker {
    fn new(is_playing: bool, initial_pos: f64) -> Self {
        let mut tracker = Self {
            timer: None,
            attempts: 0,
            error_attempts: 0,
            confirmed: false,
            anchor_pos: initial_pos,
        };
        tracker.sync_playback(is_playing, initial_pos);
        tracker
    }

    fn arm(&mut self, anchor_pos: f64) {
        self.attempts = 0;
        self.error_attempts = 0;
        self.confirmed = false;
        self.anchor_pos = anchor_pos;
        self.timer = Some(Box::pin(tokio::time::sleep(probe_delay(0))));
    }

    fn disarm(&mut self) {
        self.timer = None;
    }

    fn sync_playback(&mut self, is_playing: bool, anchor_pos: f64) {
        if is_playing {
            self.arm(anchor_pos);
        } else {
            self.disarm();
        }
    }

    /// Wait for the next calibration tick if armed, or pend indefinitely if disarmed.
    async fn tick(&mut self) {
        if let Some(ref mut timer) = self.timer {
            timer.as_mut().await;
        } else {
            futures_util::future::pending().await
        }
    }

    /// Step the calibration state machine given the newly fetched authoritative position.
    fn on_step(&mut self, real_pos: f64) {
        self.attempts += 1;
        self.error_attempts = 0;
        let moved = (real_pos - self.anchor_pos).abs() > MOVEMENT_DELTA_THRESHOLD_SECS;

        if moved {
            if !self.confirmed {
                // Audio started advancing; schedule 1 final confirmation check to lock in steady-state sync
                self.confirmed = true;
                self.timer = Some(Box::pin(tokio::time::sleep(
                    CALIBRATION_CONFIRMATION_INTERVAL,
                )));
            } else {
                // Steady-state verified: disarm calibration for the remainder of this track
                self.disarm();
            }
        } else if self.attempts < MAX_CALIBRATION_ATTEMPTS {
            // Still buffering (real_pos unchanged from anchor); adaptive probe
            self.timer = Some(Box::pin(tokio::time::sleep(probe_delay(self.attempts))));
        } else {
            // Reached maximum attempts (slow network timeout): disarm
            self.disarm();
        }
    }

    /// Handle a query failure during calibration by scheduling a retry if attempts remain.
    fn on_error(&mut self) {
        self.error_attempts += 1;
        if self.error_attempts < MAX_ERROR_ATTEMPTS {
            self.timer = Some(Box::pin(tokio::time::sleep(Duration::from_millis(400))));
        } else {
            self.disarm();
        }
    }
}

/// Callback trait for MPRIS events
pub trait MprisEventCallback: Send + 'static {
    fn on_track_change(
        &mut self,
        metadata: TrackMetadata,
        position: f64,
        service: String,
        playback_status: Option<String>,
        sampled_at: std::time::Instant,
    );
    fn on_seek(
        &mut self,
        metadata: TrackMetadata,
        position: f64,
        service: String,
        sampled_at: std::time::Instant,
    );
    fn on_calibrate(&mut self, position: f64, sampled_at: std::time::Instant);
}

impl MprisEventCallback for tokio::sync::mpsc::Sender<crate::event::Event> {
    fn on_track_change(
        &mut self,
        metadata: TrackMetadata,
        position: f64,
        service: String,
        playback_status: Option<String>,
        sampled_at: std::time::Instant,
    ) {
        let _ = self.try_send(crate::event::Event::PlayerUpdate(
            metadata,
            position,
            service,
            playback_status,
            sampled_at,
        ));
    }

    fn on_seek(
        &mut self,
        metadata: TrackMetadata,
        position: f64,
        service: String,
        sampled_at: std::time::Instant,
    ) {
        let _ = self.try_send(crate::event::Event::Seeked(
            metadata, position, service, sampled_at,
        ));
    }

    fn on_calibrate(&mut self, position: f64, sampled_at: std::time::Instant) {
        let _ = self.try_send(crate::event::Event::Calibrated {
            position,
            sampled_at,
        });
    }
}

/// Represents the current state of the watched active player
#[derive(Debug, Clone, Default)]
struct WatcherPlayerState {
    service: String,
    track: TrackMetadata,
    playback_status: String,
    position: f64,
}

impl WatcherPlayerState {
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
    block_list: Vec<String>,
    target_list: Vec<String>,
    state: WatcherPlayerState,
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
            block_list,
            target_list,
            state: WatcherPlayerState::default(),
            conn,
        };

        // Discover initial active player
        let _ = handler.discover_active_player().await;

        Ok(handler)
    }

    /// Main event loop - processes incoming MPRIS and D-Bus signals
    pub async fn handle_events(&mut self) -> Result<(), MprisError> {
        // Subscribe to standard D-Bus daemon NameOwnerChanged signals for instant connect/disconnect/transfer detection
        let dbus_proxy = zbus::fdo::DBusProxy::new(&self.conn).await?;
        let mut name_owner_stream = dbus_proxy.receive_name_owner_changed().await?;
        tracing::debug!("Subscribed to D-Bus NameOwnerChanged signals");

        // Broker-level match rule for PropertiesChanged on /org/mpris/MediaPlayer2
        let mpris_prop_rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path("/org/mpris/MediaPlayer2")?
            .arg(0, "org.mpris.MediaPlayer2.Player")?
            .build();
        let mut global_prop_stream =
            MessageStream::for_match_rule(mpris_prop_rule, &self.conn, None).await?;
        tracing::debug!("Subscribed to broker-level MPRIS PropertiesChanged signals");

        // Main event processing loop
        loop {
            if self.state.is_active() {
                if let Err(e) = self
                    .handle_player_events(&mut name_owner_stream, &mut global_prop_stream)
                    .await
                {
                    tracing::warn!(error = %e, "Player event handler error, backing off...");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            } else if let Err(e) = self
                .handle_idle_events(&mut name_owner_stream, &mut global_prop_stream)
                .await
            {
                tracing::warn!(error = %e, "Idle event handler error, backing off...");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }

    /// Handle events while a player is active.
    ///
    /// Keeps player-specific signal streams alive continuously without cancelling/recreating them
    /// on external signals, ensuring property change events (e.g. Play/Pause) are never dropped.
    async fn handle_player_events(
        &mut self,
        name_owner_stream: &mut NameOwnerChangedStream,
        global_prop_stream: &mut MessageStream,
    ) -> Result<(), MprisError> {
        let service = self.state.service.clone();
        tracing::debug!(service = %service, "Subscribing to player events");

        let proxy = MediaPlayer2PlayerProxy::builder(&self.conn)
            .destination(service.as_str())?
            .build()
            .await?;

        let dbus_proxy = zbus::fdo::DBusProxy::new(&self.conn).await?;
        let current_unique_name: Option<String> =
            if let Ok(bus_name) = zbus::names::BusName::try_from(service.as_str()) {
                dbus_proxy
                    .get_name_owner(bus_name)
                    .await
                    .ok()
                    .map(|u| u.to_string())
            } else {
                None
            };

        // Subscribe to player signals and property changes
        let mut seeked_stream = proxy.receive_seeked().await?;
        let mut metadata_stream = proxy.receive_metadata_changed().await;
        let mut status_stream = proxy.receive_playback_status_changed().await;

        // Subscribe to playerctld if available (checking ownership to prevent autostart)
        let playerctld_proxy = if dbus_proxy
            .name_has_owner(
                zbus::names::BusName::from_static_str("org.mpris.MediaPlayer2.playerctld").unwrap(),
            )
            .await
            .unwrap_or(false)
        {
            PlayerctldProxy::new(&self.conn).await.ok()
        } else {
            None
        };
        let mut player_names_stream = if let Some(ref p) = playerctld_proxy {
            Some(p.receive_player_names_changed().await)
        } else {
            None
        };

        // Perform an initial sync in case state changed during proxy setup
        let _ = self.handle_status_change(&proxy).await;
        let _ = self.handle_metadata_change(&proxy).await;

        let mut tracker =
            CalibrationTracker::new(self.state.playback_status == "Playing", self.state.position);

        loop {
            tokio::select! {
                // 0. Transient post-buffering position calibration for streaming players (e.g. Spotify)
                _ = tracker.tick() => {
                    if self.state.playback_status == "Playing" && self.state.is_active() {
                        match proxy.position().await {
                            Ok(microsecs) => {
                                let real_pos = microsecs as f64 / 1_000_000.0;
                                let sampled_at = std::time::Instant::now();
                                self.state.position = real_pos;
                                self.callback.on_calibrate(real_pos, sampled_at);
                                tracker.on_step(real_pos);
                            }
                            Err(e) => {
                                tracing::debug!(
                                    service = %self.state.service,
                                    error = %e,
                                    "Calibration position query failed, retrying"
                                );
                                tracker.on_error();
                            }
                        }
                    } else {
                        tracker.disarm();
                    }
                }
                // 1. playerctld active player list change
                Some(_) = async {
                    if let Some(ref mut stream) = player_names_stream {
                        stream.next().await
                    } else {
                        futures_util::future::pending().await
                    }
                } => {
                    tracing::debug!(service = %service, "playerctld player list changed while watching player");
                    if self.sync_active_service(&service).await? {
                        break;
                    }
                }

                // 2. D-Bus NameOwnerChanged signals
                Some(signal) = name_owner_stream.next() => {
                    if let Ok(args) = signal.args() {
                        let name = args.name.as_str();
                        let old_owner = args.old_owner.as_deref().unwrap_or("");
                        let new_owner = args.new_owner.as_deref().unwrap_or("");

                        let is_current = name == service;
                        let is_mpris = name.starts_with("org.mpris.MediaPlayer2.");
                        let owner_changed = old_owner != new_owner;

                        if (is_current || is_mpris) && owner_changed {
                            tracing::debug!(
                                name = %name,
                                is_current,
                                old_owner = %old_owner,
                                new_owner = %new_owner,
                                "NameOwnerChanged signal received while watching player"
                            );

                            if is_current {
                                // Current player disconnected or transferred ownership to a new PID.
                                // Re-discover to ensure fresh GetAll against the new process PID.
                                tracing::debug!(
                                    service = %service,
                                    old_owner = %old_owner,
                                    new_owner = %new_owner,
                                    "Current player ownership changed, refreshing player connection"
                                );
                                let _ = self.discover_active_player().await;
                                break;
                            } else if self.sync_active_service(&service).await? {
                                break;
                            }
                        }
                    }
                }

                // 3. Player Seeked signal
                Some(signal) = seeked_stream.next() => {
                    if let Ok(args) = signal.args() {
                        tracing::debug!(service = %service, position = args.position, "Seeked signal received");
                        let pos_sec = args.position as f64 / 1_000_000.0;
                        if pos_sec <= 0.0 && self.state.playback_status == "Playing" {
                            tracker.arm(pos_sec);
                        } else {
                            tracker.disarm();
                        }
                        self.handle_seek_signal(args.position).await;
                    }
                }

                // 4. Player Metadata property change
                Some(_) = metadata_stream.next() => {
                    tracing::debug!(service = %service, "Metadata changed signal received");
                    match self.handle_metadata_change(&proxy).await {
                        Ok(true) => {
                            tracker.sync_playback(
                                self.state.playback_status == "Playing",
                                self.state.position,
                            );
                        }
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!(
                                service = %service,
                                error = %e,
                                "Failed to handle metadata change"
                            );
                            if self.sync_active_service(&service).await? {
                                break;
                            }
                        }
                    }
                }

                // 5. Player PlaybackStatus property change
                Some(_) = status_stream.next() => {
                    tracing::debug!(service = %service, "Playback status changed signal received");
                    match self.handle_status_change(&proxy).await {
                        Ok(true) => {
                            tracker.sync_playback(
                                self.state.playback_status == "Playing",
                                self.state.position,
                            );

                            // If current player transitioned to Stopped, check if another player is currently Playing
                            if self.state.playback_status == "Stopped"
                                && let Ok(Some(ref best_service)) = find_best_active_service(
                                    &self.block_list,
                                    &self.target_list,
                                    Some(&self.state.service),
                                    None,
                                ).await
                                && best_service != &self.state.service {
                                    tracing::debug!(
                                        old_service = %service,
                                        new_service = %best_service,
                                        "Current player stopped playing and another player is playing, switching"
                                    );
                                    self.switch_to_player(best_service).await?;
                                    break;
                            }
                        }
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!(
                                service = %service,
                                error = %e,
                                "Failed to handle playback status change"
                            );
                            if self.sync_active_service(&service).await? {
                                break;
                            }
                        }
                    }
                }

                // 6. Global MPRIS PropertiesChanged (cross-player transitions)
                Some(Ok(msg)) = global_prop_stream.next() => {
                    if let Ok((_iface, changed, _invalidated)) =
                        msg.body().deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                        && let Some(val) = changed.get("PlaybackStatus") {
                            let is_playing = match val.deref() {
                                zvariant::Value::Str(s) => s.as_str() == "Playing",
                                _ => false,
                            };
                            if is_playing {
                                let header = msg.header();
                                let sender_unique = header.sender().map(|u| u.as_str());

                                // Skip self: If this signal was emitted by the currently monitored player,
                                // our dedicated status_stream already handles it. Avoid redundant whole-bus scans.
                                if let Some(ref cur_u) = current_unique_name
                                    && sender_unique == Some(cur_u.as_str())
                                {
                                    continue;
                                }

                                match find_best_active_service(
                                    &self.block_list,
                                    &self.target_list,
                                    Some(&self.state.service),
                                    sender_unique,
                                ).await {
                                    Ok(Some(ref best_service)) if best_service != &self.state.service => {
                                        tracing::debug!(
                                            old_service = %self.state.service,
                                            new_service = %best_service,
                                            "Another player started playing, switching"
                                        );
                                        self.switch_to_player(best_service).await?;
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                }
            }
        }

        Ok(())
    }

    /// Handle events while in idle state (no active player).
    async fn handle_idle_events(
        &mut self,
        name_owner_stream: &mut NameOwnerChangedStream,
        global_prop_stream: &mut MessageStream,
    ) -> Result<(), MprisError> {
        let dbus_proxy = zbus::fdo::DBusProxy::new(&self.conn).await?;
        let playerctld_proxy = if dbus_proxy
            .name_has_owner(
                zbus::names::BusName::from_static_str("org.mpris.MediaPlayer2.playerctld").unwrap(),
            )
            .await
            .unwrap_or(false)
        {
            PlayerctldProxy::new(&self.conn).await.ok()
        } else {
            None
        };

        let mut playerctld_stream = if let Some(ref p) = playerctld_proxy {
            Some(p.receive_player_names_changed().await)
        } else {
            None
        };

        tokio::select! {
            // playerctld notification of active player
            Some(_) = async {
                if let Some(ref mut stream) = playerctld_stream {
                    stream.next().await
                } else {
                    futures_util::future::pending().await
                }
            } => {
                tracing::debug!("playerctld player list changed while idle");
                let _ = self.discover_active_player().await;
            }

            // D-Bus NameOwnerChanged for new MPRIS player
            Some(signal) = name_owner_stream.next() => {
                if let Ok(args) = signal.args() {
                    let name = args.name.as_str();
                    let old_owner = args.old_owner.as_deref().unwrap_or("");
                    let new_owner = args.new_owner.as_deref().unwrap_or("");
                    if name.starts_with("org.mpris.MediaPlayer2.") && old_owner != new_owner && !new_owner.is_empty() {
                        tracing::debug!(name = %name, "New or updated MPRIS player appeared while idle");
                        let _ = self.discover_active_player().await;
                    }
                }
            }

            // Broker-level MPRIS PropertiesChanged (player was already running and started playing)
            Some(Ok(msg)) = global_prop_stream.next() => {
                let header = msg.header();
                let sender_unique = header.sender().map(|u| u.as_str());
                tracing::debug!(sender = ?sender_unique, "Global MPRIS PropertiesChanged received while idle");
                let _ = self.discover_active_player_with_hint(sender_unique).await;
            }
        }

        Ok(())
    }

    async fn handle_seek_signal(&mut self, position_microsecs: i64) {
        let position = position_microsecs as f64 / 1_000_000.0;
        self.state.position = position;
        let sampled_at = std::time::Instant::now();
        self.callback.on_seek(
            self.state.track.clone(),
            position,
            self.state.service.clone(),
            sampled_at,
        );
    }

    async fn handle_metadata_change(
        &mut self,
        proxy: &MediaPlayer2PlayerProxy<'_>,
    ) -> Result<bool, MprisError> {
        let metadata_map = proxy.metadata().await?;
        let new_track = extract_metadata(&metadata_map);

        if new_track != self.state.track {
            self.state.track = new_track;

            // Also update position when track changes via uncached query
            if let Ok(microsecs) = proxy.position().await {
                self.state.position = microsecs as f64 / 1_000_000.0;
            }

            let sampled_at = std::time::Instant::now();
            self.callback.on_track_change(
                self.state.track.clone(),
                self.state.position,
                self.state.service.clone(),
                Some(self.state.playback_status.clone()),
                sampled_at,
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn handle_status_change(
        &mut self,
        proxy: &MediaPlayer2PlayerProxy<'_>,
    ) -> Result<bool, MprisError> {
        if let Ok(status) = proxy.playback_status().await
            && status != self.state.playback_status
        {
            tracing::debug!(
                service = %self.state.service,
                old_status = %self.state.playback_status,
                new_status = %status,
                "Playback status transition detected"
            );
            self.state.playback_status = status.clone();

            // Query fresh authoritative position from D-Bus on any status transition (Playing or Paused)
            if let Ok(microsecs) = proxy.position().await {
                self.state.position = microsecs as f64 / 1_000_000.0;
            }
            let position = self.state.position;

            // Notify about the playback status change
            let sampled_at = std::time::Instant::now();
            self.callback.on_track_change(
                self.state.track.clone(),
                position,
                self.state.service.clone(),
                Some(status),
                sampled_at,
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Syncs active player state based on current service discovery.
    ///
    /// Returns `Ok(true)` if the player switched or deactivated (loop should break),
    /// or `Ok(false)` if the current player remains active or query encountered transient error.
    async fn sync_active_service(&mut self, current_service: &str) -> Result<bool, MprisError> {
        match find_best_active_service(
            &self.block_list,
            &self.target_list,
            Some(current_service),
            None,
        )
        .await
        {
            Ok(Some(new_service)) if new_service != current_service => {
                tracing::debug!(
                    old_service = %current_service,
                    new_service = %new_service,
                    "Active player changed, switching"
                );
                self.switch_to_player(&new_service).await?;
                Ok(true)
            }
            Ok(Some(_)) => Ok(false),
            Ok(None) => {
                tracing::debug!(service = %current_service, "No active players, deactivating");
                self.deactivate_player();
                Ok(true)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to query active player on state change");
                Ok(false)
            }
        }
    }

    /// Discovers and switches to the active unblocked player.
    pub async fn discover_active_player(&mut self) -> Result<(), MprisError> {
        self.discover_active_player_with_hint(None).await
    }

    /// Discovers and switches to the active unblocked player with an optional sender hint.
    pub async fn discover_active_player_with_hint(
        &mut self,
        preferred_sender_unique: Option<&str>,
    ) -> Result<(), MprisError> {
        let curr = (!self.state.service.is_empty()).then_some(self.state.service.as_str());
        let active = match find_best_active_service(
            &self.block_list,
            &self.target_list,
            curr,
            preferred_sender_unique,
        )
        .await
        {
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
            .map(|us| us as f64 / 1_000_000.0)
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

        self.state = WatcherPlayerState {
            service: service.to_string(),
            track: metadata.clone(),
            playback_status: playback_status.clone(),
            position,
        };

        let sampled_at = std::time::Instant::now();
        self.callback.on_track_change(
            metadata,
            position,
            service.to_string(),
            Some(playback_status),
            sampled_at,
        );

        Ok(())
    }

    fn deactivate_player(&mut self) {
        self.state.clear();
        let sampled_at = std::time::Instant::now();
        self.callback.on_track_change(
            TrackMetadata::default(),
            0.0,
            String::new(),
            None,
            sampled_at,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tracker_initial_state_playing() {
        let tracker = CalibrationTracker::new(true, 0.0);
        assert!(tracker.timer.is_some());
        assert_eq!(tracker.attempts, 0);
        assert!(!tracker.confirmed);
        assert_eq!(tracker.anchor_pos, 0.0);
    }

    #[tokio::test]
    async fn test_tracker_initial_state_paused() {
        let tracker = CalibrationTracker::new(false, 0.0);
        assert!(tracker.timer.is_none());
    }

    #[tokio::test]
    async fn test_tracker_buffering_and_steady_state_convergence() {
        let mut tracker = CalibrationTracker::new(true, 0.0);

        // Step 1: Still buffering at 0.0s
        tracker.on_step(0.0);
        assert_eq!(tracker.attempts, 1);
        assert!(!tracker.confirmed);
        assert!(tracker.timer.is_some());

        // Step 2: Still buffering at 0.0s
        tracker.on_step(0.0);
        assert_eq!(tracker.attempts, 2);
        assert!(!tracker.confirmed);
        assert!(tracker.timer.is_some());

        // Step 3: Audio starts playing (moved past delta threshold)
        tracker.on_step(0.4);
        assert_eq!(tracker.attempts, 3);
        assert!(tracker.confirmed);
        assert!(tracker.timer.is_some()); // Armed for final confirmation

        // Step 4: Final confirmation step verifies continuous playback
        tracker.on_step(1.6);
        assert_eq!(tracker.attempts, 4);
        assert!(tracker.timer.is_none()); // Disarmed!
    }

    #[tokio::test]
    async fn test_tracker_mid_track_resume_buffering() {
        let mut tracker = CalibrationTracker::new(true, 45.0);

        // Step 1: Mid-track buffering (authoritative pos still 45.0)
        tracker.on_step(45.0);
        assert_eq!(tracker.attempts, 1);
        assert!(!tracker.confirmed);
        assert!(tracker.timer.is_some());

        // Step 2: Resumed playback moves to 45.3s (delta 0.3 > 0.08)
        tracker.on_step(45.3);
        assert!(tracker.confirmed);
        assert!(tracker.timer.is_some());

        // Step 3: Steady-state confirmation
        tracker.on_step(46.5);
        assert!(tracker.timer.is_none()); // Disarmed
    }

    #[tokio::test]
    async fn test_tracker_max_attempts_timeout() {
        let mut tracker = CalibrationTracker::new(true, 0.0);
        for i in 1..=MAX_CALIBRATION_ATTEMPTS {
            tracker.on_step(0.0);
            if i < MAX_CALIBRATION_ATTEMPTS {
                assert!(tracker.timer.is_some());
            } else {
                assert!(tracker.timer.is_none()); // Disarmed after max attempts
            }
        }
    }

    #[tokio::test]
    async fn test_tracker_error_retry_and_timeout() {
        let mut tracker = CalibrationTracker::new(true, 0.0);
        for i in 1..=MAX_ERROR_ATTEMPTS {
            tracker.on_error();
            if i < MAX_ERROR_ATTEMPTS {
                assert!(tracker.timer.is_some());
                assert_eq!(tracker.error_attempts, i);
            } else {
                assert!(tracker.timer.is_none()); // Disarmed after max error attempts
            }
        }
    }

    #[tokio::test]
    async fn test_tracker_long_buffering_tolerance() {
        let mut tracker = CalibrationTracker::new(true, 0.0);

        // Player buffers online for 15 attempts (past fast probes into 1s interval)
        for i in 1..=15 {
            tracker.on_step(0.0);
            assert_eq!(tracker.attempts, i);
            assert_eq!(tracker.error_attempts, 0);
            assert!(tracker.timer.is_some());
        }

        // At attempt 16 (e.g. after ~10s of buffering), audio finally moves to 0.4s
        tracker.on_step(0.4);
        assert!(tracker.confirmed);
        assert!(tracker.timer.is_some());

        // Final confirmation check
        tracker.on_step(0.65);
        assert!(tracker.timer.is_none());
    }
}
