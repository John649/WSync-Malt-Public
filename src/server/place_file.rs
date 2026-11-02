//! The place file (docs/place-file.md): `place.rbxl` beside the project file,
//! the connected Studio's place as a binary place file, so anything that
//! reads places offline — Lune's `roblox.deserializePlace` first of all — can
//! open the whole DataModel without Studio.
//!
//! The disk is no substitute. A code-scope project keeps only folders and
//! scripts on disk (Design §7.0), so a build from the tree is a place with no
//! parts, no UI and no models in it. The file is made from what Studio holds:
//! the plugin serializes one service at a time (`place_file_capture`, native
//! `SerializationService` rbxm), the bytes come over the same
//! `branch_blob_open`/`read`/`close` pull a branch checkpoint uses, and this
//! module grafts each service's roots under a fresh DataModel with the
//! service's own properties (`place_file_status`), then writes the result
//! atomically.
//!
//! ONLY WHAT MOVED IS SERIALIZED AGAIN. Serializing holds Studio's main thread
//! (a 250k-instance place freezes it for ~9 s end to end), so every capture is
//! kept here with the generation the plugin stamped on it, and a refresh asks
//! for a service again only when its generation moved. The generations belong
//! to one plugin session (`epoch`); a new one starts the cache over.
//!
//! Two ways in: `POST /place-file` (`wsync rbxl`) refreshes now, and — on the
//! served daemons `Server::with_place_file` turns it on for — a background
//! pass does the same every `place_file_interval` seconds while a Studio is
//! connected, never during a playtest, a paused sync or the first half-minute
//! of a connection.

use axum::{
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Json, Response},
};
use log::{debug, info, warn};
use rbx_dom_weak::{
	types::{Ref, Variant},
	InstanceBuilder, WeakDom,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
	collections::HashMap,
	fs,
	path::{Path, PathBuf},
	process::{Command, Stdio},
	time::{Duration, Instant, SystemTime},
};

use crate::{
	config::Config,
	constants::PLACE_FILE,
	server::{bulk, journal, ws::frames::codes, AppState},
	util,
};

/// How long `place_file_status` may take: property reads only
const STATUS_TIMEOUT_MS: u64 = 15_000;

/// How long one `place_file_capture` may take. The serialize itself is
/// seconds at worst, but the op can queue behind whatever else the plugin is
/// answering
const CAPTURE_TIMEOUT_MS: u64 = 120_000;

/// How long `branch_blob_close` may take; it only drops a table entry
const CLOSE_TIMEOUT_MS: u64 = 10_000;

/// How often the background pass looks at the connection
const TICK: Duration = Duration::from_secs(10);

/// How long a fresh connection is left alone before the first automatic
/// refresh: connect-time sync and doc generation get Studio to themselves
const CONNECT_GRACE: Duration = Duration::from_secs(30);

/// The shortest automatic interval honored, whatever the config says
const MIN_INTERVAL_SECS: u64 = 30;

/// What one refresh did, as `POST /place-file` reports it
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
	pub path: String,
	pub bytes: u64,
	/// False when the assembled place matched what was already on disk
	pub written: bool,
	pub services: usize,
	pub instances: usize,
	/// Serialized in Studio during this refresh
	pub captured: Vec<String>,
	/// Taken from the cache: unchanged since their last capture
	pub reused: Vec<String>,
	pub duration_ms: u64,
}

/// Why a refresh did not happen
#[derive(Debug)]
pub struct Failure {
	pub status: StatusCode,
	pub code: &'static str,
	pub message: String,
}

impl Failure {
	fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
		Self {
			status,
			code,
			message: message.into(),
		}
	}
}

/// Whether the caller asked (`wsync rbxl`) or the background pass is keeping
/// the file warm. Only the background pass steps aside for a playtest or a
/// plugin session that already lost its connection to a capture
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
	Requested,
	Background,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
	epoch: String,
	#[serde(default)]
	busy: Option<String>,
	services: Vec<ServiceStatus>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceStatus {
	name: String,
	class_name: String,
	generation: u64,
	#[serde(default)]
	properties: Map<String, Value>,
}

/// One service as it was last serialized
struct Captured {
	generation: u64,
	/// The service's roots as rbxm; none when it had nothing to serialize
	rbxm: Option<Vec<u8>>,
}

#[derive(Default)]
struct Cache {
	epoch: Option<String>,
	services: HashMap<String, Captured>,
	/// What the file on disk was assembled from: the epoch plus every
	/// service's generation and properties. A refresh that captured nothing
	/// and finds the same inputs skips the assembly outright
	inputs: Option<String>,
	/// SHA-256 of the bytes last written
	written: Option<String>,
	/// A plugin session whose connection dropped mid-capture: the background
	/// pass leaves it alone, because its captures are what cost it the socket
	suspended: Option<String>,
	/// Whether the git ignore check has run for this daemon
	ignore_checked: bool,
}

/// The daemon's place-file state. One refresh runs at a time; a
/// `wsync rbxl` that arrives mid-refresh waits for it and then finds almost
/// everything cached
#[derive(Default)]
pub struct PlaceFileStore {
	cache: tokio::sync::Mutex<Cache>,
}

impl PlaceFileStore {
	pub fn new() -> Self {
		Self::default()
	}
}

/// The place file's path for a workspace
pub fn path_in(workspace: &Path) -> PathBuf {
	workspace.join(PLACE_FILE)
}

/// Refreshes the place file from the connected Studio
pub async fn refresh(state: &AppState, mode: Mode) -> Result<Report, Failure> {
	let started = Instant::now();
	let mut cache = state.place_file.cache.lock().await;

	let workspace = state.core.project().workspace_dir.clone();
	let path = path_in(&workspace);

	let Some(connection) = state.ws.plugin_connection() else {
		return Err(Failure::new(
			StatusCode::SERVICE_UNAVAILABLE,
			"NO_PLUGIN",
			"No Studio plugin is connected over the WebSocket, so there is no place to serialize",
		));
	};

	let status = bulk::plugin_request(state, "place_file_status", json!({}), STATUS_TIMEOUT_MS)
		.await
		.map_err(|error| {
			if error.code == codes::UNKNOWN_OP {
				Failure::new(
					StatusCode::NOT_IMPLEMENTED,
					"PLUGIN_OUTDATED",
					"The connected Studio plugin predates place files; update it with `wsync plugin install` and reopen the place",
				)
			} else {
				Failure::new(StatusCode::BAD_GATEWAY, "PLUGIN_ERROR", error.to_string())
			}
		})?;

	let status: Status = serde_json::from_value(status).map_err(|error| {
		Failure::new(
			StatusCode::BAD_GATEWAY,
			"PLUGIN_ERROR",
			format!("The plugin answered place_file_status in a shape this daemon cannot read: {error}"),
		)
	})?;

	if mode == Mode::Background {
		if let Some(busy) = &status.busy {
			return Err(Failure::new(
				StatusCode::CONFLICT,
				"BUSY",
				format!("Studio is busy ({busy}); the place file waits"),
			));
		}

		if cache.suspended.as_deref() == Some(status.epoch.as_str()) {
			return Err(Failure::new(
				StatusCode::CONFLICT,
				"SUSPENDED",
				"This Studio lost its connection during a capture; only `wsync rbxl` refreshes it now",
			));
		}
	}

	if cache.epoch.as_deref() != Some(status.epoch.as_str()) {
		cache.services.clear();
		cache.inputs = None;
		cache.epoch = Some(status.epoch.clone());
	}

	cache
		.services
		.retain(|name, _| status.services.iter().any(|service| &service.name == name));

	let mut captured = Vec::new();
	let mut reused = Vec::new();

	for service in &status.services {
		if cache
			.services
			.get(&service.name)
			.is_some_and(|held| held.generation == service.generation)
		{
			reused.push(service.name.clone());
			continue;
		}

		match capture(state, &service.name).await {
			Ok(fresh) => {
				cache.services.insert(service.name.clone(), fresh);
				captured.push(service.name.clone());
			}
			Err(message) => {
				// A connection that went away under a capture is most likely
				// a capture that froze Studio past the heartbeat; the
				// background pass does not try that session again
				if state.ws.plugin_connection() != Some(connection) {
					cache.suspended = Some(status.epoch.clone());
				}

				return Err(Failure::new(
					StatusCode::BAD_GATEWAY,
					"CAPTURE_FAILED",
					format!("Serializing {} failed: {message}", service.name),
				));
			}
		}
	}

	if mode == Mode::Requested {
		cache.suspended = None;
	}

	let inputs = inputs_of(&status);
	let unchanged = captured.is_empty() && cache.inputs.as_deref() == Some(inputs.as_str()) && path.is_file();

	if unchanged {
		let bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or_default();

		return Ok(Report {
			path: path.to_string_lossy().into_owned(),
			bytes,
			written: false,
			services: status.services.len(),
			instances: 0,
			captured,
			reused,
			duration_ms: started.elapsed().as_millis() as u64,
		});
	}

	let (bytes, instances) = tokio::task::block_in_place(|| assemble(&status.services, &cache.services))
		.map_err(|message| Failure::new(StatusCode::INTERNAL_SERVER_ERROR, "ASSEMBLY_FAILED", message))?;

	let digest = format!("{:x}", Sha256::digest(&bytes));
	let written = cache.written.as_deref() != Some(digest.as_str()) || !path.is_file();

	if written {
		tokio::task::block_in_place(|| write_atomically(&path, &bytes)).map_err(|error| {
			Failure::new(
				StatusCode::INTERNAL_SERVER_ERROR,
				"WRITE_FAILED",
				format!("Failed to write {}: {error}", path.display()),
			)
		})?;

		cache.written = Some(digest);
	}

	cache.inputs = Some(inputs);

	if !cache.ignore_checked {
		cache.ignore_checked = true;
		tokio::task::block_in_place(|| keep_out_of_git(&workspace));
	}

	Ok(Report {
		path: path.to_string_lossy().into_owned(),
		bytes: bytes.len() as u64,
		written,
		services: status.services.len(),
		instances,
		captured,
		reused,
		duration_ms: started.elapsed().as_millis() as u64,
	})
}

/// `place_file_capture {service}` and the pull of the blob it pinned. The pin
/// is closed whether or not the pull landed: it was made for this one read
async fn capture(state: &AppState, service: &str) -> Result<Captured, String> {
	let answer = bulk::plugin_request(
		state,
		"place_file_capture",
		json!({ "service": service }),
		CAPTURE_TIMEOUT_MS,
	)
	.await
	.map_err(|error| error.to_string())?;

	let generation = answer
		.get("generation")
		.and_then(Value::as_u64)
		.ok_or("the plugin answered place_file_capture without a generation")?;

	let Some(artifact) = answer
		.get("blob")
		.and_then(|blob| blob.get("artifactId"))
		.and_then(Value::as_str)
	else {
		return Ok(Captured { generation, rbxm: None });
	};

	let pulled = journal::pull_blob(state, artifact).await;

	drop(
		bulk::plugin_request(
			state,
			"branch_blob_close",
			json!({ "artifactId": artifact }),
			CLOSE_TIMEOUT_MS,
		)
		.await,
	);

	Ok(Captured {
		generation,
		rbxm: Some(pulled?),
	})
}

/// A fingerprint of everything the file is assembled from. Keys are sorted
/// on the way in: serde_json keeps insertion order in this build, and the
/// plugin's encoder makes no promise about the order it walks a table in
fn inputs_of(status: &Status) -> String {
	fn canonical(value: &Value) -> Value {
		match value {
			Value::Object(map) => {
				let mut keys: Vec<&String> = map.keys().collect();
				keys.sort();

				Value::Object(keys.into_iter().map(|key| (key.clone(), canonical(&map[key]))).collect())
			}
			Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
			other => other.clone(),
		}
	}

	let services: Vec<Value> = status
		.services
		.iter()
		.map(|service| {
			json!([
				service.name,
				service.class_name,
				service.generation,
				canonical(&Value::Object(service.properties.clone())),
			])
		})
		.collect();

	json!([status.epoch, services]).to_string()
}

/// A service's properties as the plugin encoded them, minus anything this
/// build cannot decode and every `Ref`: a ref names an instance by an id
/// only that Studio knows
fn decode_properties(service: &str, properties: &Map<String, Value>) -> Vec<(String, Variant)> {
	properties
		.iter()
		.filter_map(|(name, value)| match serde_json::from_value::<Variant>(value.clone()) {
			Ok(Variant::Ref(_)) => None,
			Ok(variant) => Some((name.clone(), variant)),
			Err(error) => {
				debug!("Place file: leaving out {service}.{name}, which does not decode: {error}");
				None
			}
		})
		.collect()
}

/// A fresh DataModel with every service in status order, each carrying its
/// own properties and the roots of its latest capture, as rbxl bytes. Returns
/// the bytes and the number of instances under the services
fn assemble(services: &[ServiceStatus], captured: &HashMap<String, Captured>) -> Result<(Vec<u8>, usize), String> {
	let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
	let root = dom.root_ref();

	for service in services {
		let builder = InstanceBuilder::new(service.class_name.as_str())
			.with_name(service.name.as_str())
			.with_properties(decode_properties(&service.name, &service.properties));

		let service_ref = dom.insert(root, builder);

		let Some(bytes) = captured.get(&service.name).and_then(|held| held.rbxm.as_ref()) else {
			continue;
		};

		let mut source = rbx_binary::from_reader(bytes.as_slice())
			.map_err(|error| format!("The {} capture did not decode: {error}", service.name))?;

		let roots: Vec<Ref> = source.root().children().to_vec();

		for instance in roots {
			source.transfer(instance, &mut dom, service_ref);
		}
	}

	let services: Vec<Ref> = dom.root().children().to_vec();
	let instances = dom.descendants().count().saturating_sub(1 + services.len());

	let mut bytes = Vec::new();

	rbx_binary::to_writer(&mut bytes, &dom, &services).map_err(|error| format!("Failed to encode the place: {error}"))?;

	Ok((bytes, instances))
}

/// Writes beside the target and renames over it, so a reader never opens a
/// half-written place
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
	let temp = path.with_file_name(format!(".{PLACE_FILE}.{}.tmp", std::process::id()));

	fs::write(&temp, bytes)?;

	fs::rename(&temp, path).inspect_err(|_| {
		fs::remove_file(&temp).ok();
	})
}

/// Keeps `place.rbxl` out of git when nothing ignores it yet. A branch
/// finish stages its worktree with `git add -A`, and a place file that is not
/// ignored would ride into main as a multi-megabyte binary. The entry goes in
/// `info/exclude` of the common git dir, which main and every branch worktree
/// share, and a tracked `.gitignore` is never touched
fn keep_out_of_git(workspace: &Path) {
	let git = |args: &[&str]| -> Option<(i32, String)> {
		let mut command = Command::new("git");

		command
			.arg("-C")
			.arg(workspace)
			.args(args)
			.stdin(Stdio::null())
			.stderr(Stdio::null());
		util::hide_console(&mut command);

		let output = command.output().ok()?;

		Some((
			output.status.code().unwrap_or(-1),
			String::from_utf8_lossy(&output.stdout).trim().to_owned(),
		))
	};

	// 0: already ignored; 1: not ignored; anything else: no repository here
	match git(&["check-ignore", "-q", "--", PLACE_FILE]) {
		Some((1, _)) => {}
		_ => return,
	}

	let (Some((0, common)), Some((0, prefix))) = (
		git(&["rev-parse", "--git-common-dir"]),
		git(&["rev-parse", "--show-prefix"]),
	) else {
		return;
	};

	let info = workspace.join(common).join("info");
	let exclude = info.join("exclude");
	let entry = format!("/{prefix}{PLACE_FILE}");

	let existing = fs::read_to_string(&exclude).unwrap_or_default();

	if existing.lines().any(|line| line.trim() == entry) {
		return;
	}

	let separator = if existing.is_empty() || existing.ends_with('\n') {
		""
	} else {
		"\n"
	};

	let written = fs::create_dir_all(&info).and_then(|()| {
		fs::write(
			&exclude,
			format!("{existing}{separator}# WSync place file (docs/place-file.md)\n{entry}\n"),
		)
	});

	match written {
		Ok(()) => info!("Place file: added {entry} to {}", exclude.display()),
		Err(error) => warn!("Place file: could not add {entry} to {}: {error}", exclude.display()),
	}
}

/// The background pass: every `place_file_interval` seconds while a Studio
/// is connected, re-capture whatever changed and rewrite the file
pub fn spawn_background(state: AppState) {
	tokio::spawn(async move {
		let mut connection: Option<(u32, Instant)> = None;
		let mut outdated: Option<u32> = None;
		let mut last: Option<Instant> = None;

		loop {
			tokio::time::sleep(TICK).await;

			let (enabled, interval) = {
				let config = Config::new();

				(
					config.place_file,
					Duration::from_secs((config.place_file_interval as u64).max(MIN_INTERVAL_SECS)),
				)
			};

			let Some(current) = state.ws.plugin_connection() else {
				connection = None;
				continue;
			};

			let since = match connection {
				Some((id, since)) if id == current => since,
				_ => {
					connection = Some((current, Instant::now()));
					last = None;
					continue;
				}
			};

			if !enabled
				|| outdated == Some(current)
				|| since.elapsed() < CONNECT_GRACE
				|| last.is_some_and(|last| last.elapsed() < interval)
				|| state.core.is_sync_paused()
			{
				continue;
			}

			last = Some(Instant::now());

			match refresh(&state, Mode::Background).await {
				Ok(report) if report.written => info!(
					"Place file refreshed: {} ({} bytes; captured {}, reused {}) in {} ms",
					report.path,
					report.bytes,
					if report.captured.is_empty() {
						"none".to_owned()
					} else {
						report.captured.join(", ")
					},
					report.reused.len(),
					report.duration_ms
				),
				Ok(_) => debug!("Place file unchanged"),
				Err(failure) if failure.code == "PLUGIN_OUTDATED" => {
					info!("Place file: {}", failure.message);
					outdated = Some(current);
				}
				Err(failure) if failure.code == "BUSY" || failure.code == "SUSPENDED" => {
					debug!("Place file: {}", failure.message);
					// Look again next tick rather than a whole interval later
					last = None;
				}
				Err(failure) => warn!("Place file refresh failed: {}", failure.message),
			}
		}
	});
}

// POST /place-file

/// `POST /place-file` — refresh now (`wsync rbxl`). A failure still names the
/// file and says whether an older copy is on disk, so the caller can decide
/// whether a stale place will do
pub async fn main(State(state): State<AppState>) -> Response {
	match refresh(&state, Mode::Requested).await {
		Ok(report) => {
			let mut body = serde_json::to_value(&report).unwrap_or_else(|_| json!({}));
			body["ok"] = json!(true);

			Json(body).into_response()
		}
		Err(failure) => {
			let path = path_in(&state.core.project().workspace_dir);
			let modified = fs::metadata(&path)
				.and_then(|meta| meta.modified())
				.ok()
				.and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
				.map(|since| since.as_secs());

			(
				failure.status,
				Json(json!({
					"ok": false,
					"code": failure.code,
					"error": failure.message,
					"path": path.to_string_lossy(),
					"exists": modified.is_some(),
					"modifiedAt": modified,
				})),
			)
				.into_response()
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn rbxm(names: &[&str]) -> Vec<u8> {
		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
		let root = dom.root_ref();

		for name in names {
			dom.insert(root, InstanceBuilder::new("Folder").with_name(*name));
		}

		let roots = dom.root().children().to_vec();
		let mut bytes = Vec::new();

		rbx_binary::to_writer(&mut bytes, &dom, &roots).unwrap();

		bytes
	}

	fn service(name: &str, properties: Value) -> ServiceStatus {
		ServiceStatus {
			name: name.to_owned(),
			class_name: name.to_owned(),
			generation: 1,
			properties: properties.as_object().cloned().unwrap_or_default(),
		}
	}

	#[test]
	fn a_place_carries_every_service_its_roots_and_its_properties() {
		let services = vec![
			service("Workspace", json!({ "Gravity": { "Float32": 50.0 } })),
			service("Lighting", json!({ "ClockTime": { "Float32": 6.5 } })),
			service("ServerStorage", json!({})),
		];

		let mut captured = HashMap::new();

		captured.insert(
			"Workspace".to_owned(),
			Captured {
				generation: 1,
				rbxm: Some(rbxm(&["Map", "Spawns"])),
			},
		);
		captured.insert(
			"ServerStorage".to_owned(),
			Captured {
				generation: 1,
				rbxm: None,
			},
		);

		let (bytes, instances) = assemble(&services, &captured).unwrap();
		let dom = rbx_binary::from_reader(bytes.as_slice()).unwrap();

		let names: Vec<&str> = dom
			.root()
			.children()
			.iter()
			.map(|id| dom.get_by_ref(*id).unwrap().name.as_str())
			.collect();

		assert_eq!(names, ["Workspace", "Lighting", "ServerStorage"]);
		assert_eq!(instances, 2);

		let workspace = dom.get_by_ref(dom.root().children()[0]).unwrap();
		let children: Vec<&str> = workspace
			.children()
			.iter()
			.map(|id| dom.get_by_ref(*id).unwrap().name.as_str())
			.collect();

		assert_eq!(children, ["Map", "Spawns"]);
		assert_eq!(
			workspace.properties.get(&rbx_dom_weak::ustr("Gravity")),
			Some(&Variant::Float32(50.0))
		);

		let lighting = dom.get_by_ref(dom.root().children()[1]).unwrap();

		assert_eq!(
			lighting.properties.get(&rbx_dom_weak::ustr("ClockTime")),
			Some(&Variant::Float32(6.5))
		);
	}

	#[test]
	fn refs_and_undecodable_properties_are_left_out() {
		let properties = json!({
			"CurrentCamera": { "Ref": "00000000000000000000000000000001" },
			"Gravity": { "Float32": 196.2 },
			"Nonsense": { "NotAType": 1 },
		});

		let decoded = decode_properties("Workspace", properties.as_object().unwrap());

		assert_eq!(decoded, vec![("Gravity".to_owned(), Variant::Float32(196.2))]);
	}

	#[test]
	fn the_same_inputs_print_the_same_fingerprint() {
		let status = |properties: Value| Status {
			epoch: "e".to_owned(),
			busy: None,
			services: vec![service("Lighting", properties)],
		};

		assert_eq!(
			inputs_of(&status(json!({ "A": { "Bool": true }, "B": { "Bool": false } }))),
			inputs_of(&status(json!({ "B": { "Bool": false }, "A": { "Bool": true } })))
		);
		assert_ne!(
			inputs_of(&status(json!({ "A": { "Bool": true } }))),
			inputs_of(&status(json!({ "A": { "Bool": false } })))
		);
	}

	#[test]
	fn a_write_replaces_the_file_whole() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join(PLACE_FILE);

		write_atomically(&path, b"first").unwrap();
		write_atomically(&path, b"second").unwrap();

		assert_eq!(fs::read(&path).unwrap(), b"second");
		assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
	}
}
