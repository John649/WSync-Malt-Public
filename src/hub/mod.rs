//! The hub: one well-known loopback port in front of every daemon on this
//! machine (docs/hub.md).
//!
//! A daemon binds whatever port the OS hands it and registers with the hub
//! over a WebSocket it keeps open for as long as it serves. Everything else —
//! the Studio plugin, the CLI, the desktop app — talks to the hub only, and
//! reaches a daemon through its route, `/p/<key>`, where the key is derived
//! from the daemon's canonical project path. A route therefore names a
//! project, never a process: a daemon that restarts on a different port is
//! reached at the same address, and a daemon that is gone is simply absent
//! from the hub instead of leaving a port behind for someone else to answer.
//!
//! The hub holds no project state. It keeps a table of live registrations,
//! answers `GET /daemons` with the live `/hello` of each, and forwards every
//! request under a route, WebSocket upgrades included. Any wsync process that
//! finds no hub starts one; binding the port is the only lock needed.

pub mod attach;
mod proxy;
pub mod server;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
	env,
	process::{Command, Stdio},
	thread,
	time::{Duration, Instant},
};

use crate::{daemon, util};

/// The hub's port. Deliberately outside every range a daemon or broker used
/// before the hub existed (7968–7971, 7978–7990 and the overflow above it),
/// so a hub can start while daemons from an older build still hold those
pub const DEFAULT_PORT: u16 = 7960;

/// Moves the hub for this process and everything it starts (hermetic tests,
/// or a machine where something else already owns the default port)
pub const PORT_ENV: &str = "WSYNC_HUB_PORT";

/// The hub's own wire version, reported by `GET /hello` as `hubProtocol`.
/// The hub's `/hello` deliberately carries no `protocol` field: a plugin
/// built before the hub takes anything with a numeric `protocol` for a daemon
pub const HUB_PROTOCOL: u8 = 1;

/// Hex digits of the project key a route uses — 64 bits, which no two
/// projects on one machine are going to share
const ROUTE_KEY_LENGTH: usize = 16;

/// How long a hub probe waits for `GET /hello`
const PROBE_TIMEOUT: Duration = Duration::from_millis(1000);

/// How long `GET /daemons` may take: the hub asks every daemon for its
/// `/hello` first, each within 1.5 s
const LISTING_TIMEOUT: Duration = Duration::from_secs(4);

/// How long [`ensure_running`] waits for a hub it started to answer
const START_TIMEOUT: Duration = Duration::from_secs(5);

pub fn port() -> u16 {
	env::var(PORT_ENV)
		.ok()
		.and_then(|value| value.trim().parse().ok())
		.filter(|port| *port != 0)
		.unwrap_or(DEFAULT_PORT)
}

/// The route key a daemon serving `canonical_project` is registered under
pub fn route_key(canonical_project: &str) -> String {
	let mut key = daemon::project_key(canonical_project);

	key.truncate(ROUTE_KEY_LENGTH);
	key
}

/// `http://127.0.0.1:<hub>` plus `path`
pub fn url(path: &str) -> String {
	format!("http://127.0.0.1:{}{path}", port())
}

/// The base URL every request for the daemon serving `canonical_project`
/// goes to: its daemon's routes are appended to it unchanged
pub fn route_url(canonical_project: &str) -> String {
	url(&format!("/p/{}", route_key(canonical_project)))
}

/// The hub's `GET /hello`
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HubHello {
	#[serde(default)]
	pub hub: bool,
	#[serde(default)]
	pub hub_protocol: u8,
	#[serde(default)]
	pub version: String,
	#[serde(default)]
	pub pid: u32,
	#[serde(default)]
	pub port: u16,
}

/// Whether a hub answers on this process's hub port
pub fn probe() -> Option<HubHello> {
	let hello: HubHello = reqwest::blocking::Client::builder()
		.no_proxy()
		.timeout(PROBE_TIMEOUT)
		.redirect(reqwest::redirect::Policy::none())
		.build()
		.ok()?
		.get(url("/hello"))
		.send()
		.ok()?
		.error_for_status()
		.ok()?
		.json()
		.ok()?;

	hello.hub.then_some(hello)
}

/// One daemon as `GET /daemons` lists it
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Listed {
	pub route: String,
	pub canonical_project: String,
	pub port: u16,
	#[serde(default)]
	pub pid: u32,
	pub boot_id: String,
	/// The daemon's live `/hello`; `None` when it did not answer the hub in
	/// time (registered, but wedged)
	pub hello: Option<serde_json::Value>,
}

/// `GET /daemons`
#[derive(Deserialize, Debug, Clone, Default)]
pub struct Listing {
	#[serde(default)]
	pub daemons: Vec<Listed>,
}

/// Every daemon registered with the hub, or none when no hub answers (and
/// with no hub, no daemon is reachable either)
pub fn listing() -> Listing {
	reqwest::blocking::Client::builder()
		.no_proxy()
		.timeout(LISTING_TIMEOUT)
		.redirect(reqwest::redirect::Policy::none())
		.build()
		.ok()
		.and_then(|client| client.get(url("/daemons")).send().ok())
		.and_then(|response| response.error_for_status().ok())
		.and_then(|response| response.json().ok())
		.unwrap_or_default()
}

/// Starts `wsync daemon hub` detached. Nothing waits for it: when a hub is
/// already running the new one finds the port taken and exits
pub fn spawn() -> Result<()> {
	let binary = env::current_exe().context("Failed to locate the running wsync executable")?;
	let mut command = Command::new(binary);

	command
		.args(["daemon", "hub"])
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null());

	util::detach(&mut command);
	command.spawn().context("Failed to start the WSync hub")?;

	Ok(())
}

/// Makes sure a hub answers, starting one when none does
pub fn ensure_running() -> Result<HubHello> {
	if let Some(hello) = probe() {
		return Ok(hello);
	}

	spawn()?;

	let deadline = Instant::now() + START_TIMEOUT;

	while Instant::now() < deadline {
		thread::sleep(Duration::from_millis(100));

		if let Some(hello) = probe() {
			return Ok(hello);
		}
	}

	bail!(
		"The WSync hub did not start on 127.0.0.1:{}. Its log is hub.log in the WSync state directory; \
		 if something else owns that port, set {PORT_ENV} to a free one",
		port()
	)
}
