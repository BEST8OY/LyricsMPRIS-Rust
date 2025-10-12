//! Event watching and event handler registration for MPRIS.

use crate::mpris::connection::{MprisError, get_active_player_names, is_blocked, get_dbus_conn};
use crate::mpris::metadata::{TrackMetadata, extract_metadata};
use std::sync::Arc;
use tokio::sync::mpsc;
use futures_util::stream::StreamExt;
use zbus::message::Message;
use zbus::match_rule::MatchRule;
use zbus::MessageStream;
use zbus::Proxy;
use zvariant::OwnedValue;

const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const DBUS_PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const PLAYERCTL_SENDER: &str = "com.github.altdesktop.playerctld";

pub struct MprisEventHandler<F, G>
where
    F: FnMut(TrackMetadata, f64, String) + Send + 'static,
    G: FnMut(TrackMetadata, f64, String) + Send + 'static,
{
    on_track_change: F,
    on_seek: G,
    block_list: Arc<Vec<String>>,
    current_service: String,
    last_track: TrackMetadata,
    last_playback_status: String,
    conn: Arc<zbus::Connection>,
    msg_rx: mpsc::Receiver<Message>,
}

impl<F, G> MprisEventHandler<F, G>
where
    F: FnMut(TrackMetadata, f64, String) + Send + 'static,
    G: FnMut(TrackMetadata, f64, String) + Send + 'static,
{
    pub async fn new(
        on_track_change: F,
        on_seek: G,
        block_list: Vec<String>,
    ) -> Result<Self, MprisError> {
        // use the shared zbus connection
        let conn = get_dbus_conn().await?;

        let (tx, rx) = mpsc::channel::<Message>(8);

        // Listen for PropertiesChanged (all senders)
        let rule = {
            let b = MatchRule::builder().msg_type(zbus::message::Type::Signal);
            let b = b.interface(DBUS_PROPERTIES_INTERFACE)?;
            let b = b.member("PropertiesChanged")?;
            b.build()
        };
        Self::add_match_rule(&conn, rule, tx.clone()).await?;

        // Listen for PropertiesChanged from playerctl specifically
        let rule_playerctl = {
            let b = MatchRule::builder().msg_type(zbus::message::Type::Signal);
            let b = b.interface(DBUS_PROPERTIES_INTERFACE)?;
            let b = b.member("PropertiesChanged")?;
            let b = b.sender(PLAYERCTL_SENDER)?;
            b.build()
        };
        Self::add_match_rule(&conn, rule_playerctl, tx.clone()).await?;

        // Listen for Seeked signals from MPRIS player interface
        let rule_seeked = {
            let b = MatchRule::builder().msg_type(zbus::message::Type::Signal);
            let b = b.interface(MPRIS_PLAYER_INTERFACE)?;
            let b = b.member("Seeked")?;
            b.build()
        };
        Self::add_match_rule(&conn, rule_seeked, tx.clone()).await?;

        let mut handler = Self {
            on_track_change,
            on_seek,
            block_list: Arc::new(block_list),
            current_service: String::new(),
            last_track: TrackMetadata::default(),
            last_playback_status: String::new(),
            conn: conn.clone(),
            msg_rx: rx,
        };

        // Initial player discovery
        handler.check_player_change().await?;

        Ok(handler)
    }

    async fn add_match_rule(
        conn: &Arc<zbus::Connection>,
        rule: MatchRule<'static>,
        tx: mpsc::Sender<Message>,
    ) -> Result<(), MprisError> {
        // MessageStream::for_match_rule will register and give us a stream of messages
        let mut stream = MessageStream::for_match_rule(rule, conn, Some(8)).await?;
        // forward messages to the internal channel
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                if let Ok(msg) = item {
                    // ignore send errors
                    let _ = tx.send(msg).await;
                }
            }
        });
        Ok(())
    }

    async fn check_player_change(&mut self) -> Result<(), MprisError> {
        if let Ok(names) = get_active_player_names().await {
            if let Some(service) = names.iter().find(|s| !is_blocked(s, &self.block_list)) {
                if *service != self.current_service {
                    self.update_current_player(service).await?;
                }
            } else {
                // No active, unblocked player found
                self.current_service.clear();
                let meta = TrackMetadata::default();
                (self.on_track_change)(meta, 0.0, String::new());
            }
        }
        Ok(())
    }

    async fn update_current_player(&mut self, service: &str) -> Result<(), MprisError> {
        let proxy = Proxy::new(&self.conn, service, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player").await?;
        // Metadata is a{sv} -> HashMap<String, OwnedValue>
        let metadata: Option<std::collections::HashMap<String, OwnedValue>> = proxy.get_property("Metadata").await.ok();
        let meta = metadata.as_ref().map(extract_metadata).unwrap_or_default();
        let position: f64 = proxy.get_property::<i64>("Position").await.ok().map(|p| p as f64 / 1_000_000.0).unwrap_or(0.0);
        let playback_status: String = proxy.get_property::<String>("PlaybackStatus").await.ok().unwrap_or_else(|| "Stopped".to_string());

        self.current_service = service.to_string();
        self.last_track = meta.clone();
        self.last_playback_status = playback_status;
        (self.on_track_change)(meta, position, service.to_string());
        Ok(())
    }

    pub async fn handle_events(&mut self) -> Result<(), MprisError> {
        while let Some(msg) = self.msg_rx.recv().await {
            self.handle_message(msg).await?;
        }
        Ok(())
    }

    async fn handle_message(&mut self, msg: Message) -> Result<(), MprisError> {
        let interface = msg.header().interface().map(|s| s.as_str().to_string());
        let member = msg.header().member().map(|s| s.as_str().to_string());
        match (interface.as_deref(), member.as_deref()) {
            (Some(MPRIS_PLAYER_INTERFACE), Some("Seeked")) => self.handle_seek(msg).await?,
            (Some(DBUS_PROPERTIES_INTERFACE), _) => self.handle_properties_changed(msg).await?,
            _ => {}
        }
        Ok(())
    }

    async fn handle_seek(&mut self, msg: Message) -> Result<(), MprisError> {
        if self.current_service.is_empty() {
            return Ok(());
        }
        if let Ok((pos,)) = msg.body().deserialize::<(i64,)>() {
            let sec = pos as f64 / 1_000_000.0;
            (self.on_seek)(self.last_track.clone(), sec, self.current_service.clone());
        }
        Ok(())
    }

    async fn handle_properties_changed(&mut self, msg: Message) -> Result<(), MprisError> {
        // PropertiesChanged has args: iface_name: String, changed_properties: a{sv}, invalidated_props: as
        if let Some(pc) = zbus::fdo::PropertiesChanged::from_message(msg.clone())
            && let Ok(args) = pc.args() {
            let interface_name = args.interface_name;
            let changed = args.changed_properties;
            match interface_name.as_str() {
                "org.mpris.MediaPlayer2" | "org.freedesktop.DBus.Properties" | "com.github.altdesktop.playerctld" => {
                    if changed.contains_key("PlayerNames") {
                        self.check_player_change().await?;
                    }
                }
                MPRIS_PLAYER_INTERFACE => {
                    self.handle_player_properties_changed(msg).await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn handle_player_properties_changed(
        &mut self,
        msg: Message,
    ) -> Result<(), MprisError> {
        if self.current_service.is_empty() {
            return Ok(());
        }
        let player_proxy = Proxy::new(&self.conn, self.current_service.as_str(), "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player").await?;
        // extract changed properties from the PropertiesChanged signal
        if let Some(pc) = zbus::fdo::PropertiesChanged::from_message(msg.clone())
            && let Ok(args) = pc.args() {
            let changed = args.changed_properties;
            // changed: HashMap<_, zvariant::Value<'_>>
            let mut metadata_changed = false;
            let mut status_changed = false;

            if changed.contains_key("Metadata")
                && let Ok(metadata_map) = player_proxy.get_property::<std::collections::HashMap<String, OwnedValue>>("Metadata").await {
                let new_track = extract_metadata(&metadata_map);
                if new_track != self.last_track {
                    self.last_track = new_track;
                    metadata_changed = true;
                }
            }

            if changed.contains_key("PlaybackStatus")
                && let Ok(status) = player_proxy.get_property::<String>("PlaybackStatus").await {
                if status != self.last_playback_status {
                    self.last_playback_status = status;
                    status_changed = true;
                }
            }

            if changed.contains_key("Position")
                && let Ok(pos) = player_proxy.get_property::<i64>("Position").await {
                let sec = pos as f64 / 1_000_000.0;
                (self.on_seek)(self.last_track.clone(), sec, self.current_service.clone());
            }

            if metadata_changed || status_changed {
                let position = player_proxy.get_property::<i64>("Position").await.map(|p| p as f64 / 1_000_000.0).unwrap_or(0.0);
                (self.on_track_change)(
                    self.last_track.clone(),
                    position,
                    self.current_service.clone(),
                );
            }
        }
        Ok(())
    }
}
