//! The hub process (`wsync daemon hub`).
//!
//! Routes:
//!
//! * `GET /hello` — the hub's identity (`hub: true`, `hubProtocol`)
//! * `GET /daemons` — every registered daemon with its live `/hello`, and the
//!   desktop broker when one is registered
//! * `GET /hub/attach` — the WebSocket a daemon or broker registers over and
//!   keeps open; its registration lasts exactly as long as the socket
//! * `/p/<key>/…` — forwarded to the daemon registered under that route key
//! * `/broker/…` — forwarded to the desktop app's broker
//!
//! Browsers are kept out at the door: a request whose `Host` is not this
//! hub's own loopback address (DNS rebinding) or that carries a web page's
//! `Origin` is refused before it reaches any route.

use anyhow::{bail, Context, Result};
use axum::{
	body::Body,
	extract::{
		ws::{Message, WebSocket, WebSocketUpgrade},
		DefaultBodyLimit, Request, State,
	},
	http::{header, StatusCode},
	middleware::{self, Next},
	response::{IntoResponse, Response},
	routing::get,
	Json, Router,
};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
	process,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc, Mutex,
	},
	time::{Duration, Instant},
};
use tokio::sync::watch;

use super::{proxy, route_key, HUB_PROTOCOL};
use crate::{constants::MAX_PAYLOAD_SIZE, daemon, lock, wsync_info};

/// A hub with nothing registered exits after this long. Every daemon starts
/// one again the moment it finds none, so an idle hub only outlives the last
/// daemon long enough to see a restart through
const IDLE_EXIT: Duration = Duration::from_secs(60);

/// How long a registering peer has to send its `register` frame
const REGISTER_DEADLINE: Duration = Duration::from_secs(5);

/// A registered peer is pinged this often...
const PING_INTERVAL: Duration = Duration::from_secs(5);

/// ...and dropped once it has been silent for this long. A process that
/// exits closes its socket at once; this is for one that hangs
const SILENCE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the hub waits on a peer's `/hello`
const HELLO_TIMEOUT: Duration = Duration::from_millis(1500);

/// What a peer sends first on `/hub/attach`
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum Register {
	#[serde(rename_all = "camelCase")]
	Daemon {
		canonical_project: String,
		port: u16,
		boot_id: String,
		pid: u32,
	},
	#[serde(rename_all = "camelCase")]
	Broker { port: u16, pid: u32 },
}

#[derive(Debug, Clone)]
struct Daemon {
	session: u64,
	key: String,
	canonical_project: String,
	port: u16,
	boot_id: String,
	pid: u32,
	registered_at: u64,
}

#[derive(Debug, Clone)]
struct Broker {
	session: u64,
	port: u16,
	pid: u32,
}

#[derive(Default)]
struct Table {
	/// In registration order, which is the order `/daemons` lists them in
	daemons: Vec<Daemon>,
	broker: Option<Broker>,
	/// When the table last became empty; `None` while anything is registered
	idle_since: Option<Instant>,
}

#[derive(Clone)]
struct Hub {
	inner: Arc<Inner>,
}

struct Inner {
	port: u16,
	table: Mutex<Table>,
	sessions: AtomicU64,
	http: reqwest::Client,
	log: Option<daemon::DaemonLog>,
	shutdown: watch::Sender<bool>,
}

impl Hub {
	fn note(&self, message: &str) {
		if let Some(log) = &self.inner.log {
			log.note(message);
		}
	}

	fn hello(&self) -> Value {
		let table = lock!(self.inner.table);

		json!({
			"name": "WSync hub",
			"hub": true,
			"hubProtocol": HUB_PROTOCOL,
			"version": env!("CARGO_PKG_VERSION"),
			"commit": env!("WSYNC_BUILD_COMMIT"),
			"pid": process::id(),
			"port": self.inner.port,
			"daemons": table.daemons.len(),
			"broker": table.broker.is_some(),
		})
	}

	async fn peer_hello(&self, port: u16) -> Option<Value> {
		let response = self
			.inner
			.http
			.get(format!("http://127.0.0.1:{port}/hello"))
			.timeout(HELLO_TIMEOUT)
			.send()
			.await
			.ok()?;

		if !response.status().is_success() {
			return None;
		}

		response.json().await.ok()
	}

	/// Whether the daemon a table entry names still answers as itself
	async fn still_serving(&self, port: u16, boot_id: &str) -> bool {
		self.peer_hello(port)
			.await
			.is_some_and(|hello| hello.get("bootId").and_then(Value::as_str) == Some(boot_id))
	}

	/// Admits a peer, or says why not. The table is only written after the
	/// peer's own `/hello` has confirmed what it claims to be
	async fn admit(&self, register: &Register, session: u64) -> Result<String, String> {
		match register {
			Register::Daemon {
				canonical_project,
				port,
				boot_id,
				pid,
			} => {
				let hello = self.peer_hello(*port).await.ok_or_else(|| {
					format!("Nothing answered GET /hello on port {port}, so the registration could not be verified")
				})?;

				let confirmed = hello.get("canonicalProject").and_then(Value::as_str)
					== Some(canonical_project.as_str())
					&& hello.get("bootId").and_then(Value::as_str) == Some(boot_id.as_str());

				if !confirmed {
					return Err(format!(
						"The daemon on port {port} does not serve {canonical_project} as boot {boot_id}"
					));
				}

				let key = route_key(canonical_project);
				let existing = lock!(self.inner.table)
					.daemons
					.iter()
					.find(|daemon| daemon.key == key)
					.cloned();

				// One project, one daemon. A second registration for the same
				// project is only let in when the first no longer answers as
				// itself (it is gone, and its socket has not closed yet)
				if let Some(existing) = existing {
					if existing.boot_id != *boot_id && self.still_serving(existing.port, &existing.boot_id).await {
						return Err(format!(
							"{canonical_project} is already served by PID {} on port {}",
							existing.pid, existing.port
						));
					}
				}

				let mut table = lock!(self.inner.table);

				table.daemons.retain(|daemon| daemon.key != key);
				table.daemons.push(Daemon {
					session,
					key: key.clone(),
					canonical_project: canonical_project.clone(),
					port: *port,
					boot_id: boot_id.clone(),
					pid: *pid,
					registered_at: daemon::unix_now(),
				});
				table.idle_since = None;

				Ok(key)
			}
			Register::Broker { port, pid } => {
				let hello = self
					.peer_hello(*port)
					.await
					.ok_or_else(|| format!("Nothing answered GET /hello on port {port}"))?;

				if hello.get("broker").and_then(Value::as_bool) != Some(true) {
					return Err(format!("Port {port} does not answer as a broker"));
				}

				let existing = lock!(self.inner.table).broker.clone();

				if let Some(existing) = existing {
					if existing.port != *port && self.peer_hello(existing.port).await.is_some() {
						return Err(format!(
							"A broker is already registered (PID {} on port {})",
							existing.pid, existing.port
						));
					}
				}

				let mut table = lock!(self.inner.table);

				table.broker = Some(Broker {
					session,
					port: *port,
					pid: *pid,
				});
				table.idle_since = None;

				Ok(String::from("broker"))
			}
		}
	}

	/// Drops whatever `session` registered, unless a newer session has
	/// already replaced it
	fn release(&self, session: u64) {
		let mut table = lock!(self.inner.table);
		let before = table.daemons.len();

		table.daemons.retain(|daemon| daemon.session != session);

		let released_daemon = table.daemons.len() != before;
		let released_broker = table.broker.as_ref().is_some_and(|broker| broker.session == session);

		if released_broker {
			table.broker = None;
		}

		if table.daemons.is_empty() && table.broker.is_none() && table.idle_since.is_none() {
			table.idle_since = Some(Instant::now());
		}

		drop(table);

		if released_daemon || released_broker {
			self.note(&format!("Session {session} closed; its registration was dropped"));
		}
	}

	fn daemon_port(&self, key: &str) -> Option<u16> {
		lock!(self.inner.table)
			.daemons
			.iter()
			.find(|daemon| daemon.key == key)
			.map(|daemon| daemon.port)
	}

	fn broker_port(&self) -> Option<u16> {
		lock!(self.inner.table).broker.as_ref().map(|broker| broker.port)
	}

	/// One registration, held open until either side goes away
	async fn session(self, mut socket: WebSocket) {
		let session = self.inner.sessions.fetch_add(1, Ordering::Relaxed) + 1;

		let register = match tokio::time::timeout(REGISTER_DEADLINE, socket.recv()).await {
			Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<Register>(&text),
			_ => return,
		};

		let register = match register {
			Ok(register) => register,
			Err(err) => {
				let reply = json!({ "type": "refused", "reason": format!("Malformed registration: {err}") });

				socket.send(Message::Text(reply.to_string())).await.ok();
				return;
			}
		};

		match self.admit(&register, session).await {
			Ok(route) => {
				self.note(&format!("Session {session} registered {register:?} as {route}"));

				let reply = json!({ "type": "registered", "route": route, "hub": self.hello() });

				if socket.send(Message::Text(reply.to_string())).await.is_err() {
					self.release(session);
					return;
				}
			}
			Err(reason) => {
				self.note(&format!("Session {session} refused ({register:?}): {reason}"));

				let reply = json!({ "type": "refused", "reason": reason });

				socket.send(Message::Text(reply.to_string())).await.ok();
				socket.send(Message::Close(None)).await.ok();
				return;
			}
		}

		let mut ping = tokio::time::interval(PING_INTERVAL);
		let mut heard = Instant::now();
		let mut shutdown = self.inner.shutdown.subscribe();

		loop {
			tokio::select! {
				_ = ping.tick() => {
					if heard.elapsed() > SILENCE_TIMEOUT {
						self.note(&format!("Session {session} went silent"));
						break;
					}

					if socket.send(Message::Ping(Vec::new())).await.is_err() {
						break;
					}
				}
				frame = socket.recv() => match frame {
					Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
					Some(Ok(_)) => heard = Instant::now(),
				},
				_ = shutdown.changed() => break,
			}
		}

		self.release(session);
	}

	async fn listing(&self) -> Value {
		let (daemons, broker) = {
			let table = lock!(self.inner.table);

			(table.daemons.clone(), table.broker.clone())
		};

		let hellos = join_all(daemons.iter().map(|daemon| self.peer_hello(daemon.port))).await;

		let daemons: Vec<Value> = daemons
			.iter()
			.zip(hellos)
			.map(|(daemon, hello)| {
				json!({
					"route": daemon.key,
					"base": format!("/p/{}", daemon.key),
					"canonicalProject": daemon.canonical_project,
					"port": daemon.port,
					"pid": daemon.pid,
					"bootId": daemon.boot_id,
					"registeredAt": daemon.registered_at,
					"hello": hello,
				})
			})
			.collect();

		let broker = match broker {
			Some(broker) => {
				let hello = self.peer_hello(broker.port).await;

				json!({
					"base": "/broker",
					"port": broker.port,
					"pid": broker.pid,
					"hello": hello,
				})
			}
			None => Value::Null,
		};

		json!({ "hub": self.hello(), "daemons": daemons, "broker": broker })
	}
}

async fn hello(State(hub): State<Hub>) -> Json<Value> {
	Json(hub.hello())
}

async fn daemons(State(hub): State<Hub>) -> Json<Value> {
	Json(hub.listing().await)
}

async fn attach(State(hub): State<Hub>, upgrade: WebSocketUpgrade) -> Response {
	upgrade.on_upgrade(move |socket| hub.session(socket))
}

fn not_found(message: String) -> Response {
	(StatusCode::NOT_FOUND, Json(json!({ "error": message }))).into_response()
}

/// Everything that is not one of the hub's own routes: a daemon route or the
/// broker, forwarded with the prefix taken off
async fn dispatch(State(hub): State<Hub>, request: Request<Body>) -> Response {
	let path = request.uri().path().to_owned();
	let query = request
		.uri()
		.query()
		.map(|query| format!("?{query}"))
		.unwrap_or_default();

	if let Some(rest) = path.strip_prefix("/p/") {
		let (key, tail) = match rest.find('/') {
			Some(index) => (&rest[..index], &rest[index..]),
			None => (rest, "/"),
		};

		let Some(port) = hub.daemon_port(key) else {
			return not_found(format!(
				"No daemon is registered for route {key}. Start one with `wsync daemon start --project <path>`"
			));
		};

		return proxy::forward(request, port, &format!("{tail}{query}")).await;
	}

	if path == "/broker" || path.starts_with("/broker/") {
		let tail = match &path["/broker".len()..] {
			"" => "/",
			tail => tail,
		};

		let Some(port) = hub.broker_port() else {
			return not_found(String::from(
				"No broker is registered: the WSync desktop app is not running, or has no Projects folder",
			));
		};

		return proxy::forward(request, port, &format!("{tail}{query}")).await;
	}

	not_found(format!("{path} is not a hub route"))
}

/// Origins that may talk to the hub: none at all (Studio, the CLI, daemons),
/// the hub's own, and the desktop app's webview
fn origin_allowed(origin: &str, port: u16) -> bool {
	origin == "null"
		|| origin == format!("http://127.0.0.1:{port}")
		|| origin == format!("http://localhost:{port}")
		|| origin == "tauri://localhost"
		|| origin == "http://tauri.localhost"
		|| origin == "https://tauri.localhost"
}

fn host_allowed(host: &str, port: u16) -> bool {
	let (name, host_port) = match host.strip_prefix('[') {
		// `[::1]:7960`: the colons inside the brackets are the address
		Some(rest) => match rest.split_once(']') {
			Some((address, tail)) => (address, tail.strip_prefix(':')),
			None => return false,
		},
		None => match host.rsplit_once(':') {
			Some((name, host_port)) => (name, Some(host_port)),
			None => (host, None),
		},
	};

	let loopback = matches!(name, "127.0.0.1" | "localhost" | "::1");
	let port_matches = host_port.is_none_or(|host_port| host_port.parse::<u16>().ok() == Some(port));

	loopback && port_matches
}

async fn guard(State(hub): State<Hub>, request: Request<Body>, next: Next) -> Response {
	let port = hub.inner.port;
	let headers = request.headers();

	if let Some(host) = headers.get(header::HOST).and_then(|value| value.to_str().ok()) {
		if !host_allowed(host, port) {
			return (StatusCode::FORBIDDEN, Json(json!({ "error": "Host is not this hub" }))).into_response();
		}
	}

	if let Some(origin) = headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()) {
		if !origin_allowed(origin, port) {
			return (
				StatusCode::FORBIDDEN,
				Json(json!({ "error": "Web pages cannot use the WSync hub" })),
			)
				.into_response();
		}
	}

	next.run(request).await
}

/// Runs the hub in the foreground until it has been idle for [`IDLE_EXIT`].
/// Exits quietly when another hub already owns the port
pub fn run(port: u16) -> Result<()> {
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()?
		.block_on(serve(port))
}

async fn serve(port: u16) -> Result<()> {
	let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
		Ok(listener) => listener,
		Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
			let answered = tokio::task::spawn_blocking(super::probe).await.ok().flatten();

			if answered.is_some() {
				wsync_info!("A WSync hub already runs on 127.0.0.1:{port}");
				return Ok(());
			}

			bail!("Port {port} is taken by something that is not a WSync hub; set WSYNC_HUB_PORT to a free port");
		}
		Err(err) => return Err(err).with_context(|| format!("Failed to bind the hub to 127.0.0.1:{port}")),
	};

	let log = daemon::state_dir(None)
		.ok()
		.and_then(|dir| daemon::DaemonLog::open(&dir.join("hub.log")).ok());

	let (shutdown, _) = watch::channel(false);

	let hub = Hub {
		inner: Arc::new(Inner {
			port,
			table: Mutex::new(Table {
				idle_since: Some(Instant::now()),
				..Table::default()
			}),
			sessions: AtomicU64::new(0),
			http: reqwest::Client::builder().no_proxy().build()?,
			log,
			shutdown,
		}),
	};

	hub.note(&format!("Hub listening on 127.0.0.1:{port} (PID {})", process::id()));

	{
		let hub = hub.clone();

		tokio::spawn(async move {
			let mut tick = tokio::time::interval(Duration::from_secs(5));

			loop {
				tick.tick().await;

				let idle = lock!(hub.inner.table)
					.idle_since
					.is_some_and(|since| since.elapsed() >= IDLE_EXIT);

				if idle {
					hub.note("Nothing registered for a minute; the hub exits");
					hub.inner.shutdown.send(true).ok();
					break;
				}
			}
		});
	}

	let app = Router::new()
		.route("/hello", get(hello))
		.route("/daemons", get(daemons))
		.route("/hub/attach", get(attach))
		.fallback(dispatch)
		.layer(middleware::from_fn_with_state(hub.clone(), guard))
		.layer(DefaultBodyLimit::max(MAX_PAYLOAD_SIZE))
		.with_state(hub.clone());

	let mut stop = hub.inner.shutdown.subscribe();

	axum::serve(listener, app)
		.with_graceful_shutdown(async move {
			while stop.changed().await.is_ok() {
				if *stop.borrow() {
					break;
				}
			}
		})
		.await?;

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hosts() {
		assert!(host_allowed("127.0.0.1:7960", 7960));
		assert!(host_allowed("localhost:7960", 7960));
		assert!(host_allowed("[::1]:7960", 7960));
		assert!(host_allowed("127.0.0.1", 7960));
		assert!(!host_allowed("127.0.0.1:7961", 7960));
		assert!(!host_allowed("evil.example:7960", 7960));
		assert!(!host_allowed("127.0.0.1.evil.example", 7960));
	}

	#[test]
	fn origins() {
		assert!(origin_allowed("tauri://localhost", 7960));
		assert!(origin_allowed("http://127.0.0.1:7960", 7960));
		assert!(!origin_allowed("https://evil.example", 7960));
		assert!(!origin_allowed("http://127.0.0.1:8080", 7960));
	}
}
