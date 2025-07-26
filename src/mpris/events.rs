//! Event watching and event handler registration for MPRIS.

use dbus::nonblock::Proxy;
use dbus::nonblock::stdintf::org_freedesktop_dbus::Properties;
use dbus::message::MatchRule;
use dbus::channel::MatchingReceiver;
use std::sync::Arc;
use tokio::sync::mpsc;
use crate::mpris::connection::{get_dbus_conn, get_active_player_names, is_blocked, TIMEOUT, MprisError};
use crate::mpris::metadata::{TrackMetadata, extract_metadata};

pub async fn watch_and_handle_events<F, G>(
    mut on_track_change: F,
    mut on_seek: G,
    block_list: &[String],
) -> Result<(), MprisError>
where
    F: FnMut(TrackMetadata, f64, String) + Send + 'static,
    G: FnMut(TrackMetadata, f64, String) + Send + 'static,
{
    let (resource, conn) = dbus_tokio::connection::new_session_sync()
        .map_err(|_| MprisError::NoConnection)?;
    tokio::spawn(async move { resource.await });
    let conn = Arc::new(conn);

    let rule_prop_player = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged")
        .with_interface("org.freedesktop.DBus.Properties");
    let rule_prop_playerctld = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged")
        .with_sender("com.github.altdesktop.playerctld");
    let rule_seeked = MatchRule::new_signal("org.mpris.MediaPlayer2.Player", "Seeked");
    conn.add_match(rule_prop_player.clone()).await?;
    conn.add_match(rule_prop_playerctld.clone()).await?;
    conn.add_match(rule_seeked.clone()).await?;

    let (tx, mut rx) = mpsc::channel::<dbus::message::Message>(8);
    let conn2 = Arc::clone(&conn);
    let tx_prop = tx.clone();
    MatchingReceiver::start_receive(
        &**conn2,
        rule_prop_player,
        Box::new(move |msg, _| {
            let _ = tx_prop.try_send(msg);
            true
        }),
    );
    let conn3 = Arc::clone(&conn);
    let tx_propctld = tx.clone();
    MatchingReceiver::start_receive(
        &**conn3,
        rule_prop_playerctld,
        Box::new(move |msg, _| {
            let _ = tx_propctld.try_send(msg);
            true
        }),
    );
    let conn4 = Arc::clone(&conn);
    let tx_seeked = tx.clone();
    MatchingReceiver::start_receive(
        &**conn4,
        rule_seeked,
        Box::new(move |msg, _| {
            let _ = tx_seeked.try_send(msg);
            true
        }),
    );


    // Helper to update player state and call on_track_change
    async fn update_current_player<F>(
        service: &str,
        on_track_change: &mut F,
        current_service: &mut String,
        last_track: &mut TrackMetadata,
        last_playback_status: &mut String,
    ) -> Result<(), MprisError>
    where
        F: FnMut(TrackMetadata, f64, String),
    {
        let conn = get_dbus_conn().await?;
        let proxy = Proxy::new(service, "/org/mpris/MediaPlayer2", TIMEOUT, conn);
        let metadata: Option<dbus::arg::PropMap> = Properties::get(&proxy, "org.mpris.MediaPlayer2.Player", "Metadata").await.ok();
        let meta = metadata.map(|map| extract_metadata(&map)).unwrap_or_default();
        let position: f64 = Properties::get::<i64>(&proxy, "org.mpris.MediaPlayer2.Player", "Position").await.ok().map(|p| p as f64 / 1_000_000.0).unwrap_or(0.0);
        let playback_status: String = Properties::get::<String>(&proxy, "org.mpris.MediaPlayer2.Player", "PlaybackStatus").await.ok().unwrap_or_else(|| "Stopped".to_string());
        *current_service = service.to_string();
        *last_track = meta.clone();
        *last_playback_status = playback_status;
        on_track_change(meta, position, service.to_string());
        Ok(())
    }

    let mut current_service = String::new();
    let mut last_track = TrackMetadata::default();
    let mut last_playback_status = String::new();

    // Only query player list at startup, and filter out blocked services
    if let Ok(names) = get_active_player_names().await {
        if let Some(service) = names.iter().find(|s| !is_blocked(s, block_list)) {
            update_current_player(
                service,
                &mut on_track_change,
                &mut current_service,
                &mut last_track,
                &mut last_playback_status,
            ).await?;
        }
    }

    loop {
        if let Some(msg) = rx.recv().await {
            // Handle Seeked events as before, but only for current_service
            if msg.interface().as_deref() == Some("org.mpris.MediaPlayer2.Player") {
                if let Some(member) = msg.member() {
                    if member.to_string() == "Seeked" {
                        if current_service.is_empty() {
                            return Ok(());
                        }
                        if let Some(pos) = msg.read1::<i64>().ok() {
                            let sec = pos as f64 / 1_000_000.0;
                            on_seek(last_track.clone(), sec, current_service.clone());
                        }
                        continue;
                    }
                }
            }

            // Unified PropertiesChanged handler
            if msg.interface().as_deref() == Some("org.freedesktop.DBus.Properties") {
                if let Some(interface_name) = msg.read1::<&str>().ok() {
                    // PlayerNames (from any sender, including playerctld)
                    if interface_name == "org.mpris.MediaPlayer2" || interface_name == "org.freedesktop.DBus.Properties" || interface_name == "com.github.altdesktop.playerctld" {
                        let changed: Option<dbus::arg::PropMap> = msg.read2().ok().map(|(_, c): (String, dbus::arg::PropMap)| c);
                        if let Some(changed) = changed {
                            if changed.contains_key("PlayerNames") {
                                // Player list changed, update current_service
                                if let Ok(names) = get_active_player_names().await {
                                    if let Some(service) = names.iter().find(|s| !is_blocked(s, block_list)) {
                                        if *service != current_service {
                                            update_current_player(
                                                service,
                                                &mut on_track_change,
                                                &mut current_service,
                                                &mut last_track,
                                                &mut last_playback_status,
                                            ).await?;
                                        }
                                    }
                                }
                                // After player switch, skip further event processing for this message
                                continue;
                            }
                        }
                    }
                    // Only handle player interface for Metadata/PlaybackStatus/Position for current_service
                    if interface_name == "org.mpris.MediaPlayer2.Player" {
                        if current_service.is_empty() {
                            continue;
                        }
                        let player_proxy = Proxy::new(
                            &current_service,
                            "/org/mpris/MediaPlayer2",
                            TIMEOUT,
                            get_dbus_conn().await?.clone(),
                        );
                        let changed: Option<dbus::arg::PropMap> = msg.read2().ok().map(|(_, c): (String, dbus::arg::PropMap)| c);
                        if let Some(changed) = changed {
                            let mut metadata_changed = false;
                            let mut status_changed = false;
                            // Metadata
                            if changed.contains_key("Metadata") {
                                if let Ok(metadata) = Properties::get::<dbus::arg::PropMap>(&player_proxy, "org.mpris.MediaPlayer2.Player", "Metadata").await {
                                    let new_track = extract_metadata(&metadata);
                                    if new_track != last_track {
                                        last_track = new_track;
                                        metadata_changed = true;
                                    }
                                }
                            }
                            // PlaybackStatus
                            if changed.contains_key("PlaybackStatus") {
                                if let Ok(status) = Properties::get::<String>(&player_proxy, "org.mpris.MediaPlayer2.Player", "PlaybackStatus").await {
                                    if status != last_playback_status {
                                        last_playback_status = status;
                                        status_changed = true;
                                    }
                                }
                            }
                            // Position
                            if changed.contains_key("Position") {
                                if let Some(pos_var) = changed.get("Position") {
                                    if let Some(pos) = pos_var.0.as_i64() {
                                        let sec = pos as f64 / 1_000_000.0;
                                        on_seek(last_track.clone(), sec, current_service.clone());
                                    }
                                }
                            }
                            if metadata_changed || status_changed {
                                let position = Properties::get::<i64>(&player_proxy, "org.mpris.MediaPlayer2.Player", "Position")
                                    .await
                                    .map(|p| p as f64 / 1_000_000.0)
                                    .unwrap_or(0.0);
                                on_track_change(last_track.clone(), position, current_service.clone());
                            }
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
