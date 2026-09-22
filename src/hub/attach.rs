//! A daemon's registration with the hub.
//!
//! The daemon opens `ws://127.0.0.1:<hub>/hub/attach`, says what it serves,
//! and then holds the socket open. For as long as it does, the hub routes to
//! it. When the hub goes away, the daemon starts a new one and registers
//! again, so the hub never needs its own record of who was registered.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
	sync::Arc,
	time::{Duration, Instant},
};
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::daemon::DaemonLog;

/// How long the hub has to answer a `register` frame
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// A hub that could not be reached is started again at most this often
const SPAWN_INTERVAL: Duration = Duration::from_secs(5);

/// What a daemon registers as
pub struct Registration {
	pub canonical_project: String,
	pub port: u16,
	pub boot_id: String,
	pub pid: u32,
}

enum Failure {
	/// Nothing answered on the hub port, or the socket broke
	Unreachable,
	/// The hub answered and said no
	Refused(String),
}

async fn register(
	url: &str,
	frame: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Failure> {
	let (mut socket, _) = connect_async(url).await.map_err(|_| Failure::Unreachable)?;

	socket
		.send(Message::Text(frame.to_owned()))
		.await
		.map_err(|_| Failure::Unreachable)?;

	let reply = tokio::time::timeout(REPLY_TIMEOUT, async {
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

fn stopping(shutdown: &watch::Receiver<Option<String>>) -> bool {
	shutdown.borrow().is_some()
}

/// Keeps `registration` registered with the hub until `shutdown` fires,
/// starting a hub whenever none answers
pub async fn hold(
	registration: Registration,
	mut shutdown: watch::Receiver<Option<String>>,
	log: Option<Arc<DaemonLog>>,
) {
	let note = |message: &str| {
		log::debug!("{message}");

		if let Some(log) = &log {
			log.note(message);
		}
	};

	let port = super::port();
	let url = format!("ws://127.0.0.1:{port}/hub/attach");
	let frame = json!({
		"kind": "daemon",
		"canonicalProject": registration.canonical_project,
		"port": registration.port,
		"bootId": registration.boot_id,
		"pid": registration.pid,
	})
	.to_string();

	let mut last_spawn: Option<Instant> = None;
	let mut last_refusal: Option<String> = None;
	let mut registered_before = false;

	loop {
		if stopping(&shutdown) {
			return;
		}

		let outcome = tokio::select! {
			outcome = register(&url, &frame) => outcome,
			_ = shutdown.changed() => return,
		};

		let delay = match outcome {
			Ok(mut socket) => {
				note(&if registered_before {
					format!("Registered with the hub on 127.0.0.1:{port} again")
				} else {
					format!("Registered with the hub on 127.0.0.1:{port}")
				});

				registered_before = true;
				last_refusal = None;

				loop {
					tokio::select! {
						message = socket.next() => match message {
							Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
							Some(Ok(_)) => {}
						},
						_ = shutdown.changed() => {
							socket.close(None).await.ok();
							return;
						}
					}
				}

				note("Lost the hub; registering again");

				Duration::from_millis(100)
			}
			Err(Failure::Unreachable) => {
				if last_spawn.is_none_or(|at| at.elapsed() >= SPAWN_INTERVAL) {
					last_spawn = Some(Instant::now());

					match tokio::task::spawn_blocking(super::spawn).await {
						Ok(Ok(())) => note(&format!("No hub answered on 127.0.0.1:{port}; started one")),
						Ok(Err(err)) => note(&format!(
							"No hub answered on 127.0.0.1:{port}, and starting one failed: {err}"
						)),
						Err(_) => {}
					}
				}

				Duration::from_millis(250)
			}
			Err(Failure::Refused(reason)) => {
				if last_refusal.as_deref() != Some(reason.as_str()) {
					note(&format!("The hub refused this daemon: {reason}"));
					crate::wsync_warn!("The hub refused this daemon: {reason}");
					last_refusal = Some(reason);
				}

				Duration::from_secs(5)
			}
		};

		tokio::select! {
			_ = tokio::time::sleep(delay) => {}
			_ = shutdown.changed() => return,
		}
	}
}
