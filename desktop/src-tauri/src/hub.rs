//! The app's side of the WSync hub (the engine's `src/hub`, docs/hub.md).
//!
//! Every daemon registers with the hub on `127.0.0.1:7960` and is reached
//! through it at `/p/<route>`. The broker does the same as `kind: "broker"`,
//! so the Studio plugin reaches Create Project and attach at `/broker` on the
//! hub instead of scanning a range of ports for them. The registration is a
//! WebSocket held open for as long as the broker listens: when it closes, the
//! hub forgets the broker. When no hub answers, one is started from the
//! bundled engine, which is exactly what a daemon does.

use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use tokio::time;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// The hub's port — the engine's `hub::DEFAULT_PORT`.
const DEFAULT_PORT: u16 = 7960;

/// Same override the engine reads, so a hermetic run moves both together.
const PORT_ENV: &str = "WSYNC_HUB_PORT";

/// Hex digits of a route key — the engine's `ROUTE_KEY_LENGTH`.
const ROUTE_KEY_LENGTH: usize = 16;

/// How long the hub has to answer a `register` frame.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// A hub that could not be reached is started again at most this often.
const SPAWN_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) fn port() -> u16 {
    std::env::var(PORT_ENV)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|port| *port != 0)
        .unwrap_or(DEFAULT_PORT)
}

/// The route the daemon serving `canonical_project` is reached at — the
/// engine's `hub::route_key`: the first 16 hex digits of the SHA-256 of the
/// canonical project path. Do not change one without the other.
pub(crate) fn route_key(canonical_project: &str) -> String {
    let digest = Sha256::digest(canonical_project.as_bytes());
    let mut key: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();

    key.truncate(ROUTE_KEY_LENGTH);
    key
}

/// Starts `wsync daemon hub` from the bundled engine, detached. A hub that
/// is already running makes the new one exit at once, so this never needs to
/// know whether one is.
fn spawn_hub() -> Result<(), String> {
    let binary = crate::daemon::resolve_binary().map_err(|error| error.message)?;
    let mut command = Command::new(&binary);

    command
        .args(["daemon", "hub"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console window, and
        // it outlives the app, which the daemons registered with it need
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run {}: {error}", binary.display()))?;

    // Reaped here so an exited hub never lingers as a zombie of the app.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    Ok(())
}

enum Failure {
    Unreachable,
    Refused(String),
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn register(url: &str, frame: &str) -> Result<Socket, Failure> {
    let (mut socket, _) = connect_async(url).await.map_err(|_| Failure::Unreachable)?;

    socket
        .send(Message::Text(frame.to_owned()))
        .await
        .map_err(|_| Failure::Unreachable)?;

    let reply = time::timeout(REPLY_TIMEOUT, async {
        while let Some(message) = socket.next().await {
            match message {
                Ok(Message::Text(text)) => return serde_json::from_str::<Value>(&text).ok(),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }

        None
    })
    .await
    .ok()
    .flatten()
    .ok_or(Failure::Unreachable)?;

    match reply.get("type").and_then(Value::as_str) {
        Some("registered") => Ok(socket),
        Some("refused") => Err(Failure::Refused(
            reply
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no reason given")
                .to_owned(),
        )),
        _ => Err(Failure::Unreachable),
    }
}

/// Keeps the broker listening on `broker_port` registered with the hub,
/// starting a hub whenever none answers. Runs until the task is aborted,
/// which is how the broker stopping takes it off the hub: the socket closes
/// with the task.
pub(crate) async fn hold_broker(broker_port: u16) {
    let url = format!("ws://127.0.0.1:{}/hub/attach", port());
    let frame = json!({
        "kind": "broker",
        "port": broker_port,
        "pid": std::process::id(),
    })
    .to_string();

    let mut last_spawn: Option<Instant> = None;
    let mut last_refusal: Option<String> = None;

    loop {
        let delay = match register(&url, &frame).await {
            Ok(mut socket) => {
                eprintln!("[broker] registered with the WSync hub on port {}", port());
                last_refusal = None;

                // Read until the hub goes away; tungstenite answers its pings.
                while let Some(message) = socket.next().await {
                    if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                        break;
                    }
                }

                eprintln!("[broker] lost the WSync hub; registering again");
                Duration::from_millis(100)
            }
            Err(Failure::Unreachable) => {
                if last_spawn.is_none_or(|at| at.elapsed() >= SPAWN_INTERVAL) {
                    last_spawn = Some(Instant::now());

                    if let Err(error) = spawn_hub() {
                        eprintln!("[broker] no WSync hub answered, and starting one failed: {error}");
                    }
                }

                Duration::from_millis(250)
            }
            Err(Failure::Refused(reason)) => {
                if last_refusal.as_deref() != Some(reason.as_str()) {
                    eprintln!("[broker] the WSync hub refused the broker: {reason}");
                    last_refusal = Some(reason);
                }

                Duration::from_secs(5)
            }
        };

        time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_keys_match_the_engine() {
        // The engine's `hub::route_key` for the same path: sha256 hex, first 16
        let key = route_key("/Users/me/Projects/game/default.project.json");

        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|character| character.is_ascii_hexdigit()));
        assert_eq!(key, route_key("/Users/me/Projects/game/default.project.json"));
        assert_ne!(key, route_key("/Users/me/Projects/other/default.project.json"));
    }
}
