// mpris.rs: Async MPRIS client for metadata, position, and event watching

use dbus::blocking::{Connection};
use dbus::message::MatchRule;
use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::channel::MatchingReceiver;

#[derive(Debug, Clone, Default)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
}

type MetadataMap = HashMap<String, dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>>;

fn get_active_player(conn: &Connection) -> Result<String, Box<dyn Error + Send + Sync>> {
    let proxy = conn.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        Duration::from_millis(5000),
    );
    let (names,): (Vec<String>,) = proxy.method_call(
        "org.freedesktop.DBus",
        "ListNames",
        (),
    )?;
    for name in names {
        if name == "org.mpris.MediaPlayer2.playerctld" {
            return Ok(name);
        }
    }
    Err("playerctld (org.mpris.MediaPlayer2.playerctld) not found on the session bus".into())
}

pub async fn get_metadata() -> Result<TrackMetadata, Box<dyn Error + Send + Sync>> {
    tokio::task::spawn_blocking(|| {
        let conn = Connection::new_session()?;
        let player_name = get_active_player(&conn)?;
        let proxy = conn.with_proxy(
            &player_name,
            "/org/mpris/MediaPlayer2",
            Duration::from_millis(5000),
        );
        let metadata: MetadataMap = proxy.get(
            "org.mpris.MediaPlayer2.Player",
            "Metadata",
        )?;
        let title = metadata.get("xesam:title")
            .and_then(|v| v.0.as_str())
            .unwrap_or("").to_string();
        let artist = metadata.get("xesam:artist")
            .and_then(|v| v.0.as_iter())
            .and_then(|mut it| it.next())
            .and_then(|v| v.as_str())
            .unwrap_or("").to_string();
        let album = metadata.get("xesam:album")
            .and_then(|v| v.0.as_str())
            .unwrap_or("").to_string();
        Ok(TrackMetadata { title, artist, album })
    }).await?
}

pub async fn get_position() -> Result<f64, Box<dyn Error + Send + Sync>> {
    tokio::task::spawn_blocking(|| {
        let conn = Connection::new_session()?;
        let player_name = get_active_player(&conn)?;
        let proxy = conn.with_proxy(
            &player_name,
            "/org/mpris/MediaPlayer2",
            Duration::from_millis(5000),
        );
        let pos: i64 = proxy.get(
            "org.mpris.MediaPlayer2.Player",
            "Position",
        )?;
        Ok(pos as f64 / 1_000_000.0)
    }).await?
}

pub async fn get_playback_status() -> Result<String, Box<dyn Error + Send + Sync>> {
    tokio::task::spawn_blocking(|| {
        let conn = Connection::new_session()?;
        let player_name = get_active_player(&conn)?;
        let proxy = conn.with_proxy(
            &player_name,
            "/org/mpris/MediaPlayer2",
            Duration::from_millis(5000),
        );
        let status: String = proxy.get(
            "org.mpris.MediaPlayer2.Player",
            "PlaybackStatus",
        )?;
        Ok(status)
    }).await?
}

/// Watches for MPRIS property change signals and invokes the provided callbacks.
pub async fn watch_and_handle_events<F, G>(
    mut on_track_change: F,
    mut on_seek: G,
) -> Result<(), Box<dyn Error + Send + Sync>>
where
    F: FnMut(TrackMetadata, f64) + Send + 'static,
    G: FnMut(TrackMetadata, f64) + Send + 'static,
{
    // Connect to the session bus
    let (resource, conn) = dbus_tokio::connection::new_session_sync()?;
    tokio::spawn(async move { resource.await });
    let conn = Arc::new(conn);

    // Add a match rule for PropertiesChanged signals
    let rule = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
    conn.add_match(rule.clone()).await?;

    // Channel for signals
    let (tx, mut rx) = mpsc::channel::<dbus::message::Message>(8);
    let tx2 = tx.clone();
    let conn2 = conn.clone();
    // Listen for signals
    conn2.start_receive(
        rule,
        Box::new(move |msg, _| {
            let _ = tx2.try_send(msg);
            true
        }),
    );

    // Initial fetch
    if let Ok(meta) = get_metadata().await {
        if let Ok(pos) = get_position().await {
            on_track_change(meta.clone(), pos);
        }
    }
    let mut last_track = TrackMetadata::default();

    while let Some(msg) = rx.recv().await {
        // Check interface
        let iface: Option<&str> = msg.read1().ok();
        if iface != Some("org.mpris.MediaPlayer2.Player") {
            continue;
        }
        // Get changed properties
        let changed: Option<dbus::arg::PropMap> = msg.read2().ok().map(|(_, c): (String, dbus::arg::PropMap)| c);
        if let Some(changed) = changed {
            if changed.contains_key("Metadata") {
                if let Ok(meta) = get_metadata().await {
                    if let Ok(pos) = get_position().await {
                        if meta.title != last_track.title || meta.artist != last_track.artist || meta.album != last_track.album {
                            last_track = meta.clone();
                            on_track_change(meta, pos);
                        }
                    }
                }
            }
            if let Some(pos_var) = changed.get("Position") {
                if let Some(pos) = pos_var.0.as_i64() {
                    let sec = pos as f64 / 1_000_000.0;
                    on_seek(last_track.clone(), sec);
                }
            }
        }
    }
    Ok(())
}
