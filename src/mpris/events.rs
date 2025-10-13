//! Event watching and event handler registration for MPRIS.

use crate::mpris::connection::{MprisError, get_active_player_names, is_blocked, get_dbus_conn};
use crate::mpris::metadata::{TrackMetadata, extract_metadata};
use std::sync::Arc;
use futures_util::stream::{StreamExt, select_all};
use zbus::message::Message;
use zbus::match_rule::MatchRule;
use zbus::MessageStream;
use zbus::Proxy;
use zvariant::OwnedValue;
use std::collections::HashMap;

const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const DBUS_PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";

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
        self.service.clear();
        self.track = TrackMetadata::default();
        self.playback_status.clear();
        self.position = 0.0;
    }
}

/// Handles MPRIS events and manages player state
pub struct MprisEventHandler<C: MprisEventCallback> {
    callback: C,
    block_list: Arc<Vec<String>>,
    state: PlayerState,
    conn: Arc<zbus::Connection>,
    message_stream: futures_util::stream::SelectAll<MessageStream>,
}

impl<C: MprisEventCallback> MprisEventHandler<C> {
    pub async fn new(callback: C, block_list: Vec<String>) -> Result<Self, MprisError> {
        let conn = get_dbus_conn().await?;
        let message_stream = Self::setup_message_listeners(&conn).await?;

        let mut handler = Self {
            callback,
            block_list: Arc::new(block_list),
            state: PlayerState::default(),
            conn: conn.clone(),
            message_stream,
        };

        handler.discover_active_player().await?;

        Ok(handler)
    }

    /// Sets up all DBus message listeners and combines them into a single stream
    async fn setup_message_listeners(
        conn: &Arc<zbus::Connection>,
    ) -> Result<futures_util::stream::SelectAll<MessageStream>, MprisError> {
        let mut streams = Vec::new();

        // Listen for general PropertiesChanged signals
        streams.push(
            MessageStream::for_match_rule(
                Self::build_properties_changed_rule()?,
                conn,
                Some(8),
            ).await?
        );


        // Listen for Seeked signals
        streams.push(
            MessageStream::for_match_rule(
                Self::build_seeked_rule()?,
                conn,
                Some(8),
            ).await?
        );

        Ok(select_all(streams))
    }

    fn build_properties_changed_rule() -> Result<MatchRule<'static>, MprisError> {
        Ok(MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(DBUS_PROPERTIES_INTERFACE)?
            .member("PropertiesChanged")?
            .build())
    }

    fn build_seeked_rule() -> Result<MatchRule<'static>, MprisError> {
        Ok(MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(MPRIS_PLAYER_INTERFACE)?
            .member("Seeked")?
            .build())
    }

    /// Main event loop - processes incoming DBus messages
    pub async fn handle_events(&mut self) -> Result<(), MprisError> {
        while let Some(result) = self.message_stream.next().await {
            match result {
                Ok(msg) => {
                    if let Err(e) = self.process_message(msg).await {
                        eprintln!("Error processing message: {}", e);
                    }
                }
                Err(e) => {
                    eprintln!("Error receiving message: {}", e);
                }
            }
        }
        Ok(())
    }

    async fn process_message(&mut self, msg: Message) -> Result<(), MprisError> {
        let interface = msg.header().interface().map(|s| s.as_str().to_string());
        let member = msg.header().member().map(|s| s.as_str().to_string());

        match (interface.as_deref(), member.as_deref()) {
            (Some(MPRIS_PLAYER_INTERFACE), Some("Seeked")) => {
                self.handle_seek_event(msg).await
            }
            (Some(DBUS_PROPERTIES_INTERFACE), _) => {
                self.handle_properties_changed_event(msg).await
            }
            _ => Ok(()),
        }
    }

    async fn handle_seek_event(&mut self, msg: Message) -> Result<(), MprisError> {
        if !self.state.is_active() {
            return Ok(());
        }

        if let Ok((pos_microsec,)) = msg.body().deserialize::<(i64,)>() {
            let position = pos_microsec as f64 / 1_000_000.0;
            self.state.position = position;
            self.callback.on_seek(
                self.state.track.clone(),
                position,
                self.state.service.clone(),
            );
        }

        Ok(())
    }

    async fn handle_properties_changed_event(&mut self, msg: Message) -> Result<(), MprisError> {
        let Some(pc) = zbus::fdo::PropertiesChanged::from_message(msg.clone()) else {
            return Ok(());
        };

        let Ok(args) = pc.args() else {
            return Ok(());
        };

        // If PlayerNames changed on any PropertiesChanged signal, re-discover active player
        if args.changed_properties.contains_key("PlayerNames") {
            self.discover_active_player().await?;
        }

        // Only handle org.mpris.MediaPlayer2.Player property changes here -- other interfaces may still
        // send PropertiesChanged but we only care about the player properties (Metadata, PlaybackStatus, Position)
        if args.interface_name.as_str() == MPRIS_PLAYER_INTERFACE {
            // convert keys to owned Strings to match handler signature
            let changed_owned: HashMap<String, zvariant::Value<'_>> = args
                .changed_properties
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();

            self.handle_player_property_changes(changed_owned).await?;
        }

        Ok(())
    }

    async fn handle_player_property_changes(
        &mut self,
        changed: HashMap<String, zvariant::Value<'_>>,
    ) -> Result<(), MprisError> {
        if !self.state.is_active() {
            return Ok(());
        }

        let mut metadata_changed = false;
        let mut status_changed = false;

        // Only accept values present in the message body. Do not query the proxy for fallbacks.

        // Metadata
        if let Some(val) = changed.get("Metadata") {
            // Try common conversions: HashMap<String, OwnedValue>, or an OwnedValue wrapping that map
            let mut parsed = None;
            if let Ok(map) = std::convert::TryInto::<HashMap<String, OwnedValue>>::try_into(val.clone()) {
                parsed = Some(map);
            } else if let Ok(ov) = std::convert::TryInto::<OwnedValue>::try_into(val.clone()) {
                if let Ok(map) = std::convert::TryInto::<HashMap<String, OwnedValue>>::try_into(ov) {
                    parsed = Some(map);
                }
            }

            if let Some(map) = parsed {
                let new_track = extract_metadata(&map);
                if new_track != self.state.track {
                    self.state.track = new_track;
                    metadata_changed = true;
                }
            }
        }

        // PlaybackStatus
        if let Some(val) = changed.get("PlaybackStatus") {
            // Try String or a wrapped OwnedValue -> String
            if let Ok(status) = std::convert::TryInto::<String>::try_into(val.clone()) {
                if status != self.state.playback_status {
                    self.state.playback_status = status;
                    status_changed = true;
                }
            } else if let Ok(ov) = std::convert::TryInto::<OwnedValue>::try_into(val.clone()) {
                if let Ok(status) = std::convert::TryInto::<String>::try_into(ov) {
                    if status != self.state.playback_status {
                        self.state.playback_status = status;
                        status_changed = true;
                    }
                }
            }
        }

        // If PlaybackStatus or Position weren't present in the change set, query them via a
        // targeted Properties.Get call using the org.freedesktop.DBus.Properties interface.
        if !changed.contains_key("PlaybackStatus") || !changed.contains_key("Position") {
            let props_proxy = Proxy::new(&self.conn, self.state.service.as_str(), "/org/mpris/MediaPlayer2", DBUS_PROPERTIES_INTERFACE).await?;

            // PlaybackStatus via Properties.Get(interface, property)
            if !changed.contains_key("PlaybackStatus") {
                if let Ok(reply) = props_proxy.call_method("Get", &(MPRIS_PLAYER_INTERFACE, "PlaybackStatus")).await {
                    if let Ok(val) = reply.body().deserialize::<OwnedValue>() {
                        // Try to convert variant to String
                        if let Ok(status) = std::convert::TryInto::<String>::try_into(val) {
                            if status != self.state.playback_status {
                                self.state.playback_status = status;
                                status_changed = true;
                            }
                        }
                    }
                }
            }

    // Position (seek)
    if let Some(val) = changed.get("Position") {
            // Accept i64, u64, or variant-wrapped OwnedValue -> i64/u64
            let mut pos_microsec_opt: Option<i128> = None;

            if let Ok(i) = std::convert::TryInto::<i64>::try_into(val.clone()) {
                pos_microsec_opt = Some(i as i128);
            } else if let Ok(u) = std::convert::TryInto::<u64>::try_into(val.clone()) {
                pos_microsec_opt = Some(u as i128);
            } else if let Ok(ov) = std::convert::TryInto::<OwnedValue>::try_into(val.clone()) {
                if let Ok(i) = std::convert::TryInto::<i64>::try_into(ov.clone()) {
                    pos_microsec_opt = Some(i as i128);
                } else if let Ok(u) = std::convert::TryInto::<u64>::try_into(ov) {
                    pos_microsec_opt = Some(u as i128);
                }
            }

            if let Some(pos_microsec) = pos_microsec_opt {
                let position = pos_microsec as f64 / 1_000_000.0;
                self.state.position = position;
                self.callback.on_seek(
                    self.state.track.clone(),
                    position,
                    self.state.service.clone(),
                );
            }
        }

            // Position via Properties.Get(interface, property)
            if !changed.contains_key("Position") {
                if let Ok(reply) = props_proxy.call_method("Get", &(MPRIS_PLAYER_INTERFACE, "Position")).await {
                    if let Ok(val) = reply.body().deserialize::<OwnedValue>() {
                        // Attempt to convert to integer types (i64 / u64)
                        let mut pos_microsec_opt: Option<i128> = None;
                        if let Ok(i) = std::convert::TryInto::<i64>::try_into(val.clone()) {
                            pos_microsec_opt = Some(i as i128);
                        } else if let Ok(u) = std::convert::TryInto::<u64>::try_into(val.clone()) {
                            pos_microsec_opt = Some(u as i128);
                        }

                        if let Some(pos_microsec) = pos_microsec_opt {
                            let position = pos_microsec as f64 / 1_000_000.0;
                            if (position - self.state.position).abs() > f64::EPSILON {
                                self.state.position = position;
                                self.callback.on_seek(
                                    self.state.track.clone(),
                                    position,
                                    self.state.service.clone(),
                                );
                            }
                        }
                    }
                }
            }
        }

        // Notify on track or status change. Position will remain as-is unless Position was included.
        if metadata_changed || status_changed {
            let position = self.state.position;
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
        } else {
            self.deactivate_player();
        }

        Ok(())
    }

    async fn switch_to_player(&mut self, service: &str) -> Result<(), MprisError> {
        let proxy = Proxy::new(&self.conn, service, "/org/mpris/MediaPlayer2", MPRIS_PLAYER_INTERFACE).await?;
        
        let metadata = self.fetch_metadata(&proxy).await?.unwrap_or_default();
        let position = self.fetch_position(&proxy).await?.unwrap_or(0.0);
        let playback_status = self.fetch_playback_status(&proxy).await?
            .unwrap_or_else(|| "Stopped".to_string());

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

    // helper removed: Proxy::new is called inline where needed to avoid lifetime issues

    async fn fetch_metadata(&self, proxy: &Proxy<'_>) -> Result<Option<TrackMetadata>, MprisError> {
        // The incoming proxy may be created with the MPRIS interface; prefer a targeted
        // Properties.Get call to avoid GetAll. We attempt to call Get on the properties interface.
        let props = Proxy::new(&self.conn, proxy.destination().as_str(), "/org/mpris/MediaPlayer2", DBUS_PROPERTIES_INTERFACE).await?;
        if let Ok(reply) = props.call_method("Get", &(MPRIS_PLAYER_INTERFACE, "Metadata")).await {
            if let Ok(val) = reply.body().deserialize::<OwnedValue>() {
                if let Ok(map) = std::convert::TryInto::<HashMap<String, OwnedValue>>::try_into(val) {
                    return Ok(Some(extract_metadata(&map)));
                }
            }
        }
        Ok(None)
    }

    async fn fetch_position(&self, proxy: &Proxy<'_>) -> Result<Option<f64>, MprisError> {
        let props = Proxy::new(&self.conn, proxy.destination().as_str(), "/org/mpris/MediaPlayer2", DBUS_PROPERTIES_INTERFACE).await?;
        if let Ok(reply) = props.call_method("Get", &(MPRIS_PLAYER_INTERFACE, "Position")).await {
            if let Ok(val) = reply.body().deserialize::<OwnedValue>() {
                if let Ok(i) = std::convert::TryInto::<i64>::try_into(val.clone()) {
                    return Ok(Some(i as f64 / 1_000_000.0));
                }
                if let Ok(u) = std::convert::TryInto::<u64>::try_into(val.clone()) {
                    return Ok(Some(u as f64 / 1_000_000.0));
                }
            }
        }
        Ok(None)
    }

    async fn fetch_playback_status(&self, proxy: &Proxy<'_>) -> Result<Option<String>, MprisError> {
        let props = Proxy::new(&self.conn, proxy.destination().as_str(), "/org/mpris/MediaPlayer2", DBUS_PROPERTIES_INTERFACE).await?;
        if let Ok(reply) = props.call_method("Get", &(MPRIS_PLAYER_INTERFACE, "PlaybackStatus")).await {
            if let Ok(val) = reply.body().deserialize::<OwnedValue>() {
                if let Ok(status) = std::convert::TryInto::<String>::try_into(val) {
                    return Ok(Some(status));
                }
            }
        }
        Ok(None)
    }
}
// Convenience constructor for closure-based callbacks
impl<F, G> MprisEventHandler<ClosureCallback<F, G>>
where
    F: FnMut(TrackMetadata, f64, String) + Send + 'static,
    G: FnMut(TrackMetadata, f64, String) + Send + 'static,
{
    pub async fn with_closures(
        on_track_change: F,
        on_seek: G,
        block_list: Vec<String>,
    ) -> Result<Self, MprisError>
    where
        F: FnMut(TrackMetadata, f64, String) + Send + 'static,
        G: FnMut(TrackMetadata, f64, String) + Send + 'static,
    {
        let callback = ClosureCallback::new(on_track_change, on_seek);
        Self::new(callback, block_list).await
    }
}
