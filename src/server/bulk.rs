//! Bulk divergence application (Design §7.4).
//!
//! **Keep Studio** is a server-driven pull with a fenced apply, per synced
//! root: the daemon pulls the divergent subtree from the plugin
//! (`read_subtree` for structure with script sources elided, chunked
//! `source_read` parts verified against the claimed SHA-256), materializes
//! the projected file tree through the regular write middleware into a
//! same-volume staging directory, then — with the VFS paused — verifies the
//! fenced disk generation did not move, swaps `live → backup` and
//! `stage → live`, refreshes the daemon tree from the swapped root (echo
//! suppressed: Studio already holds this state) and stamps baselines.
//! Roots commit in sequence; a failed root terminates the transfer with a
//! replayable partial receipt and earlier committed roots are not rolled
//! back.
//!
//! **Keep Disk** replays the staged entries over the live sync channel,
//! bracketed by one explicit plugin transaction
//! (`transaction_begin`/`transaction_finish`) so a successful pull remains
//! one Studio Undo.

use axum::{
	http::StatusCode,
	response::{IntoResponse, Json, Response},
};
use base64::Engine;
use chrono::{NaiveDateTime, Utc};
use log::{debug, info, warn};
use path_clean::PathClean;
use rbx_dom_weak::{types::Variant, ustr};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	fs,
	path::{Path, PathBuf},
	time::Duration,
};

use crate::{
	config::Config,
	constants::{
		BACKUPS_DIR, BACKUP_COMPLETE_MARKER, BACKUP_KEEP_COUNT, BACKUP_KEEP_DAYS, BULK_OP_TIMEOUT_MS,
		BULK_ROOT_MAX_BYTES, BULK_SCRIPT_MAX_BYTES, BULK_TRANSFER_MAX_BYTES, REQUEST_DEFAULT_TIMEOUT_MS,
		SOURCE_READ_CHUNK_BYTES,
	},
	core::{
		changes::Changes,
		conflict::{self, ContentState},
		meta::{Context, Meta, Source, SourceEntry, SourceKind},
		processor::{read, write},
		snapshot::Snapshot,
		tree::Tree,
	},
	ext::PathExt,
	middleware::{dir, Middleware},
	server::{
		ws::frames::{self, codes, OpError, ServerFrame},
		AppState, DIRECTION_DISK_TO_STUDIO,
	},
	util,
	vfs::Vfs,
	Properties,
};

/// Filesystem-safe RFC 3339-shaped UTC stamp used for backup directory names
/// (`:` is replaced by `-` so the names stay valid on every platform)
const BACKUP_STAMP_FORMAT: &str = "%Y-%m-%dT%H-%M-%SZ";

/// Staging subdirectory inside a transfer's backup directory. Staged roots
/// are renamed out of it during the swap; leftovers mean the transfer never
/// completed and the directory is exempt from pruning
const STAGE_DIR: &str = ".stage";

// Plugin request plumbing

/// A failed plugin call, carrying the protocol error code so callers can
/// distinguish e.g. `NOT_FOUND` from transport failures
#[derive(Debug)]
pub struct PluginCallError {
	pub code: String,
	pub message: String,
}

impl PluginCallError {
	fn transport(message: impl Into<String>) -> Self {
		Self {
			code: codes::PLUGIN_ERROR.to_owned(),
			message: message.into(),
		}
	}
}

impl std::fmt::Display for PluginCallError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{} ({})", self.message, self.code)
	}
}

/// Sends one request to the connected WS plugin and awaits its response
pub async fn plugin_request(
	state: &AppState,
	op: &str,
	args: Value,
	timeout_ms: u64,
) -> Result<Value, PluginCallError> {
	let frames = state.ws.plugin_frames().map_err(PluginCallError::transport)?;

	let request_id = state.ws.next_request_id();
	let receiver = state.ws.register_request(request_id);

	let frame = ServerFrame::Request {
		request_id,
		op: op.to_owned(),
		args,
		timeout_ms,
	};

	if frames.try_send(frame.to_text()).is_err() {
		state.ws.abort_request(request_id);

		return Err(PluginCallError::transport(
			"Plugin outbound queue is overloaded or the plugin disconnected",
		));
	}

	match tokio::time::timeout(Duration::from_millis(timeout_ms), receiver).await {
		Ok(Ok(response)) => {
			if response.ok {
				Ok(response.value.unwrap_or(Value::Null))
			} else {
				let error = response.error.unwrap_or_else(|| OpError {
					code: codes::PLUGIN_ERROR.to_owned(),
					message: "Plugin reported failure without error detail".to_owned(),
				});

				Err(PluginCallError {
					code: error.code,
					message: error.message,
				})
			}
		}
		Ok(Err(_)) => Err(PluginCallError::transport(
			"Studio plugin disconnected before answering",
		)),
		Err(_) => {
			state.ws.abort_request(request_id);

			Err(PluginCallError {
				code: codes::TIMEOUT.to_owned(),
				message: format!("No plugin response to {op} within {timeout_ms} ms"),
			})
		}
	}
}

// Keep Disk: bracketed sync-channel replay

/// Sends a fire-and-forget request frame (used for the transaction bracket:
/// ordering on the single FIFO plugin channel is the guarantee; the plugin's
/// response is routed and dropped)
fn send_bracket_op(state: &AppState, frames: &tokio::sync::mpsc::Sender<String>, op: &str, args: Value) -> bool {
	let request_id = state.ws.next_request_id();

	// Register (and drop) the response slot so the plugin's answer resolves
	// silently instead of logging an unknown-request warning
	drop(state.ws.register_request(request_id));

	let frame = ServerFrame::Request {
		request_id,
		op: op.to_owned(),
		args,
		timeout_ms: REQUEST_DEFAULT_TIMEOUT_MS,
	};

	let sent = frames.try_send(frame.to_text()).is_ok();

	if !sent {
		state.ws.abort_request(request_id);
	}

	sent
}

/// Replays a Keep-Disk change set directly over the plugin's frame channel,
/// bracketed by `transaction_begin` / `transaction_finish` so the whole pull
/// lands as one Studio recording (Design §7.4-B). Any frame that fails to
/// enqueue aborts the apply with `transaction_finish {commit: false}`
pub fn replay_disk_bracketed(state: &AppState, choice_id: &str, changes: Changes) -> Result<(), String> {
	let frames = state.ws.plugin_frames().map_err(str::to_owned)?;

	if !send_bracket_op(
		state,
		&frames,
		"transaction_begin",
		json!({ "name": format!("WSync: Pull {choice_id}") }),
	) {
		return Err("Failed to enqueue transaction_begin: the plugin queue is overloaded or gone".into());
	}

	let activity = frames::Event::sync_activity(DIRECTION_DISK_TO_STUDIO, &changes);

	for frame in frames::sync_frames(changes) {
		if frames.try_send(frame).is_err() {
			send_bracket_op(state, &frames, "transaction_finish", json!({ "commit": false }));

			return Err(
				"Failed to enqueue a sync frame: the plugin queue is overloaded or gone; the transaction was aborted"
					.into(),
			);
		}
	}

	if !send_bracket_op(state, &frames, "transaction_finish", json!({ "commit": true })) {
		return Err("Failed to enqueue transaction_finish: the plugin queue is overloaded or gone".into());
	}

	state.ws.emit(activity);

	Ok(())
}

// Keep Studio: pulled snapshot decoding

/// A claimed script source from `read_subtree`'s `snapshot.sources` map
#[derive(Debug, Clone)]
struct SourceClaim {
	bytes: u64,
	sha256: String,
}

/// One node of the pulled Studio subtree (AddedSnapshot shape with script
/// sources elided)
#[derive(Debug)]
struct PulledNode {
	id: String,
	name: String,
	class: String,
	properties: Map<String, Value>,
	children: Vec<PulledNode>,
}

fn parse_pulled_node(value: &Value, at: &str) -> Result<PulledNode, String> {
	let id = value
		.get("id")
		.and_then(Value::as_str)
		.ok_or_else(|| format!("{at}: missing id"))?;

	util::parse_ref(id).map_err(|err| format!("{at}: {err}"))?;

	let name = value
		.get("name")
		.and_then(Value::as_str)
		.ok_or_else(|| format!("{at}: missing name"))?;

	let class = value
		.get("class")
		.and_then(Value::as_str)
		.ok_or_else(|| format!("{at}: missing class"))?;

	let properties = match value.get("properties") {
		None | Some(Value::Null) => Map::new(),
		Some(Value::Object(map)) => map.clone(),
		Some(_) => return Err(format!("{at}: properties must be an object")),
	};

	let mut children = Vec::new();

	if let Some(list) = value.get("children") {
		let list = list
			.as_array()
			.ok_or_else(|| format!("{at}: children must be an array"))?;

		for (index, child) in list.iter().enumerate() {
			children.push(parse_pulled_node(child, &format!("{at}.children[{index}]"))?);
		}
	}

	Ok(PulledNode {
		id: id.to_owned(),
		name: name.to_owned(),
		class: class.to_owned(),
		properties,
		children,
	})
}

/// Parses a `read_subtree` response: the pulled root node plus the sibling
/// `sources` claim map covering every script in the subtree
fn parse_read_subtree(value: &Value) -> Result<(PulledNode, HashMap<String, SourceClaim>), String> {
	let snapshot = value
		.get("snapshot")
		.ok_or("read_subtree response is missing the snapshot field")?;

	let node = parse_pulled_node(snapshot, "snapshot")?;

	let mut sources = HashMap::new();

	if let Some(map) = snapshot.get("sources") {
		let map = map.as_object().ok_or("snapshot.sources must be an object")?;

		for (reference, claim) in map {
			util::parse_ref(reference).map_err(|err| format!("snapshot.sources key: {err}"))?;

			let bytes = claim
				.get("bytes")
				.and_then(Value::as_u64)
				.ok_or_else(|| format!("snapshot.sources[{reference}]: missing bytes"))?;

			let sha256 = claim
				.get("sha256")
				.and_then(Value::as_str)
				.ok_or_else(|| format!("snapshot.sources[{reference}]: missing sha256"))?;

			sources.insert(
				reference.clone(),
				SourceClaim {
					bytes,
					sha256: sha256.to_lowercase(),
				},
			);
		}
	}

	Ok((node, sources))
}

/// Pulls one script source through chunked `source_read` calls and verifies
/// the assembled bytes against the claimed length and SHA-256 (mismatch =
/// abort, per the pinned contract)
async fn pull_source(state: &AppState, reference: &str, claim: &SourceClaim) -> Result<Vec<u8>, String> {
	let mut assembled: Vec<u8> = Vec::with_capacity(claim.bytes as usize);

	loop {
		let value = plugin_request(
			state,
			"source_read",
			json!({
				"ref": reference,
				"offset": assembled.len() as u64,
				"maxLen": SOURCE_READ_CHUNK_BYTES,
			}),
			BULK_OP_TIMEOUT_MS,
		)
		.await
		.map_err(|err| format!("source_read for {reference} failed: {err}"))?;

		let data = value
			.get("data")
			.and_then(Value::as_str)
			.ok_or_else(|| format!("source_read for {reference}: missing data"))?;

		let eof = value.get("eof").and_then(Value::as_bool).unwrap_or(false);

		let total = value
			.get("totalBytes")
			.and_then(Value::as_u64)
			.ok_or_else(|| format!("source_read for {reference}: missing totalBytes"))?;

		if total != claim.bytes {
			return Err(format!(
				"source_read for {reference}: totalBytes {total} contradicts the read_subtree claim of {} bytes",
				claim.bytes
			));
		}

		let part = base64::engine::general_purpose::STANDARD
			.decode(data)
			.map_err(|err| format!("source_read for {reference}: invalid base64: {err}"))?;

		if part.len() as u64 > SOURCE_READ_CHUNK_BYTES {
			return Err(format!(
				"source_read for {reference}: part of {} bytes exceeds the {SOURCE_READ_CHUNK_BYTES}-byte chunk bound",
				part.len()
			));
		}

		if part.is_empty() && !eof {
			return Err(format!("source_read for {reference} made no progress"));
		}

		assembled.extend_from_slice(&part);

		if assembled.len() as u64 > claim.bytes {
			return Err(format!(
				"source_read for {reference} returned more bytes than the claimed {}",
				claim.bytes
			));
		}

		if eof {
			break;
		}
	}

	if assembled.len() as u64 != claim.bytes {
		return Err(format!(
			"source_read for {reference} ended at {} bytes, expected {}",
			assembled.len(),
			claim.bytes
		));
	}

	let digest = format!("{:x}", Sha256::digest(&assembled));

	if digest != claim.sha256 {
		return Err(format!(
			"SHA-256 mismatch for {reference}: assembled {digest}, claimed {}",
			claim.sha256
		));
	}

	Ok(assembled)
}

/// Converts a pulled node into a core snapshot, attaching verified script
/// sources. The `ArgonEmpty` wire marker is stripped exactly as on every
/// other decode path
fn to_snapshot(node: &PulledNode, sources: &HashMap<String, Vec<u8>>) -> Result<Snapshot, String> {
	let id = util::parse_ref(&node.id).map_err(|err| err.to_string())?;

	let mut properties = node.properties.clone();
	properties.remove("ArgonEmpty");
	properties.remove("Source");

	let mut properties: Properties = serde_json::from_value(Value::Object(properties))
		.map_err(|err| format!("{}: failed to decode properties: {err}", node.name))?;

	if util::is_script(&node.class) {
		let bytes = sources
			.get(&node.id)
			.ok_or_else(|| format!("script {} ({}) has no source entry", node.name, node.id))?;

		let source = String::from_utf8_lossy(bytes).into_owned();

		properties.insert(ustr("Source"), Variant::String(source));
	}

	let mut children = Vec::new();

	for child in &node.children {
		children.push(to_snapshot(child, sources)?);
	}

	Ok(Snapshot::new()
		.with_id(id)
		.with_name(&node.name)
		.with_class(&node.class)
		.with_properties(properties)
		.with_children(children))
}

// Keep Studio: staging, fencing, swapping

/// Studio-first apply options (Design §7.0): the promptless code-scope
/// auto-apply overlays unclaimed live files into the staged root (disk-only
/// and out-of-projection files survive the swap in place) and preserves the
/// disk originals of `differs` entries into the review store before the swap
#[derive(Debug, Default)]
pub struct StudioFirstOptions {
	/// Absolute live file → absolute preservation target (inside the review
	/// directory). Copied under the fence, right before the swap
	pub preserve: HashMap<PathBuf, PathBuf>,
}

/// What an entry inside a directory stands for, as the walker reads it —
/// which is what "claimed" has to mean, because a path is not an identity.
#[derive(PartialEq, Eq, Hash, Debug)]
enum Claim {
	/// An `init.*` (or `init.meta.json`): this entry *is* its own directory
	Own,
	/// An ordinary instance inside the directory, by the name the walker
	/// gives it — suffix stripped, so `Foo.client.luau`, `Foo.local.luau`,
	/// `Foo.luau`, `Foo.meta.json` and the directory `Foo/` all claim `Foo`
	Named(String),
}

/// The instance an entry claims inside its directory. Out-of-projection files
/// fall back to their literal name, so they are only ever claimed by an entry
/// spelled identically
fn claim_of(path: &Path, is_dir: bool, context: &Context) -> Claim {
	if is_dir {
		return Claim::Named(path.get_name().to_owned());
	}

	let rules = context.sync_rules();

	if rules.iter().any(|rule| rule.matches_child(path)) {
		return Claim::Own;
	}

	if let Some(resolved) = rules.iter().find_map(|rule| rule.resolve(path)) {
		return Claim::Named(resolved.name);
	}

	// `SyncRule::resolve`/`resolve_child` refuse `InstanceData` by design, so
	// a sidecar is matched against its own rules here. A sidecar belongs to
	// the instance it names: leaving `Foo.meta.json` behind when the stage
	// replaced `Foo.client.luau` with `Foo.local.luau` would graft the old
	// instance's attributes onto the new file
	let name = path.get_name();

	for rule in context.sync_rules_of_type(&Middleware::InstanceData, false) {
		if let Some(child_pattern) = rule.child_pattern.as_ref() {
			if name == child_pattern.as_str() {
				return Claim::Own;
			}
		}

		if let Some(pattern) = rule.pattern.as_ref() {
			if let Some((prefix, suffix)) = pattern.as_str().split_once('*') {
				if name.len() > prefix.len() + suffix.len() && name.starts_with(prefix) && name.ends_with(suffix) {
					return Claim::Named(name[prefix.len()..name.len() - suffix.len()].to_owned());
				}
			}
		}
	}

	Claim::Named(name.to_owned())
}

/// Every claim the staged directory already makes
fn claims_in(stage: &Path, context: &Context) -> std::io::Result<HashSet<Claim>> {
	let mut claims = HashSet::new();

	if !stage.is_dir() {
		return Ok(claims);
	}

	for entry in fs::read_dir(stage)? {
		let entry = entry?;
		claims.insert(claim_of(&entry.path(), entry.file_type()?.is_dir(), context));
	}

	Ok(claims)
}

/// Carries unclaimed live entries forward into the staged root (Design §7.0):
/// any file or directory the staged projection does not claim — disk-only
/// projection files and out-of-projection files alike — is copied in, so the
/// swap cannot displace it into the backup.
///
/// "Claimed" is per *instance*, not per path. The writer's layout is not the
/// one the previous tool used — a LocalScript is `Foo.local.luau` here and was
/// `Foo.client.luau` there, a RunContext = Server script is `Foo.runserver.luau`
/// and was `Foo.server.luau`, and an instance that gained children is promoted
/// from `Foo.luau` to `Foo/init.luau`. Keyed by path, none of those old
/// spellings looked claimed, so the swap carried them in beside the file that
/// had just replaced them. The directory then held two candidates for one
/// instance, and which one the walker read decided the instance's *class* —
/// that is how `Client/Local/Emitter2D` and `InGameExplorer/.../
/// InGameExplorerClient` came back as duplicate LocalScript subtrees
fn overlay_unclaimed(live: &Path, stage: &Path, context: &Context) -> Result<(), String> {
	fn walk(live: &Path, stage: &Path, context: &Context) -> std::io::Result<()> {
		let claimed = claims_in(stage, context)?;

		for entry in fs::read_dir(live)? {
			let entry = entry?;
			let is_dir = entry.file_type()?.is_dir();
			let staged = stage.join(entry.file_name());

			// The stage already speaks for this instance, under whatever
			// spelling it chose: the live entry is claimed away, not carried
			// forward. An identical path is handled below as before
			if !staged.exists() && claimed.contains(&claim_of(&entry.path(), is_dir, context)) {
				continue;
			}

			if is_dir {
				// The stage claims a file here (folder demoted to file):
				// the live subtree is claimed-away, not carried forward
				if staged.is_file() {
					continue;
				}

				if !staged.exists() {
					fs::create_dir_all(&staged)?;
				}

				walk(&entry.path(), &staged, context)?;
			} else if !staged.exists() {
				fs::copy(entry.path(), &staged)?;
			}
		}

		Ok(())
	}

	walk(live, stage, context).map_err(|err| format!("failed to overlay unclaimed disk files into the stage: {err}"))
}

/// SHA-256 of every file under `dir`, keyed by relative path — the fence
/// generation. Any difference between two captures means the disk moved
/// Whether a filesystem error is the platform refusing to touch something
/// because another handle is open on it, rather than a real fault.
///
/// Windows fails a directory rename with `ERROR_ACCESS_DENIED` (5) or
/// `ERROR_SHARING_VIOLATION` (32) when *any* process holds a handle inside
/// it: an editor, another tool, a search indexer, or a virus scanner still
/// reading the files WSync wrote a millisecond ago. Nothing is wrong with the
/// tree, and the handle is usually gone within a few tens of milliseconds
fn is_locked(err: &std::io::Error) -> bool {
	err.kind() == std::io::ErrorKind::PermissionDenied || matches!(err.raw_os_error(), Some(5) | Some(32))
}

/// A rename that gives a transient lock a few chances to clear. The total
/// wait is under half a second, which a rare fallback can afford to spend on
/// the worker thread; a lock that outlives it is not transient, and the
/// caller falls back to the file-level path
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
	const ATTEMPTS: u32 = 5;

	let mut delay = Duration::from_millis(25);

	for attempt in 1..=ATTEMPTS {
		match fs::rename(from, to) {
			Ok(()) => return Ok(()),
			Err(err) if attempt < ATTEMPTS && is_locked(&err) => {
				debug!(
					"Rename of {} refused ({err}); retrying in {}ms",
					from.display(),
					delay.as_millis()
				);

				std::thread::sleep(delay);
				delay *= 2;
			}
			Err(err) => return Err(err),
		}
	}

	unreachable!("the loop returns on its last attempt")
}

/// Copies a directory tree. Used when the live root cannot be *moved* into
/// the backup but still has to be backed up before anything overwrites it
fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
	fs::create_dir_all(to)?;

	for entry in fs::read_dir(from)? {
		let entry = entry?;
		let target = to.join(entry.file_name());

		if entry.file_type()?.is_dir() {
			copy_dir_all(&entry.path(), &target)?;
		} else {
			fs::copy(entry.path(), &target)?;
		}
	}

	Ok(())
}

/// Makes `live` hold exactly what `stage` holds, one file at a time.
///
/// The fallback for a platform that will not move a directory while anything
/// holds a handle inside it. Renaming a single *file* needs only that file's
/// own handle, which WSync controls, so this path lands the same content
/// without ever asking to move a directory. It is slower and it is not
/// atomic — which is why it is the fallback and not the rule — but the
/// per-file temp+rename still means no reader sees half a file.
///
/// Files whose bytes already match are left alone, so the work is
/// proportional to what actually changed rather than to the size of the root
fn sync_tree_in_place(stage: &Path, live: &Path) -> Result<(), String> {
	let staged = fingerprint_dir(stage)?;
	let current = fingerprint_dir(live).unwrap_or_default();

	fs::create_dir_all(live).map_err(|err| format!("failed to create {}: {err}", live.display()))?;

	for (relative, digest) in &staged {
		if current.get(relative) == Some(digest) {
			continue;
		}

		replace_file(&stage.join(relative), &live.join(relative))?;
	}

	// Whatever the stage does not carry has left the projection
	for relative in current.keys() {
		if !staged.contains_key(relative) {
			let path = live.join(relative);

			if let Err(err) = fs::remove_file(&path) {
				return Err(format!("failed to remove {}: {err}", path.display()));
			}
		}
	}

	prune_empty_dirs(live);

	Ok(())
}

/// Removes directories the file-level apply emptied, bottom up. Best effort
/// throughout: a directory that will not go is left alone, never reported as
/// a failed apply
fn prune_empty_dirs(dir: &Path) -> bool {
	let Ok(entries) = fs::read_dir(dir) else {
		return false;
	};

	let mut empty = true;

	for entry in entries.flatten() {
		let path = entry.path();

		if path.is_dir() {
			if !prune_empty_dirs(&path) {
				empty = false;
			}
		} else {
			empty = false;
		}
	}

	empty && fs::remove_dir(dir).is_ok()
}

fn fingerprint_dir(dir: &Path) -> Result<BTreeMap<PathBuf, [u8; 32]>, String> {
	let mut fingerprint = BTreeMap::new();

	fn walk(base: &Path, current: &Path, fingerprint: &mut BTreeMap<PathBuf, [u8; 32]>) -> std::io::Result<()> {
		for entry in fs::read_dir(current)? {
			let entry = entry?;
			let path = entry.path();

			if entry.file_type()?.is_dir() {
				walk(base, &path, fingerprint)?;
			} else {
				let bytes = fs::read(&path)?;
				let relative = path.strip_prefix(base).unwrap_or(&path).to_owned();

				fingerprint.insert(relative, Sha256::digest(&bytes).into());
			}
		}

		Ok(())
	}

	walk(dir, dir, &mut fingerprint).map_err(|err| format!("failed to read {}: {err}", dir.display()))?;

	Ok(fingerprint)
}

// The incremental apply: writing only what actually differs

/// Extensions whose middleware reads through `Vfs::read_to_string`, which
/// folds line endings when `ignore_line_endings` is on. Only these may be
/// compared line-ending-insensitively: the byte readers (`.rbxm`, `.rbxmx`,
/// `.csv`, `.msgpack`) are handed every byte the file holds, so for them a
/// carriage return is content
const TEXT_READ_EXTENSIONS: [&str; 8] = ["luau", "lua", "txt", "md", "json", "toml", "yaml", "yml"];

/// The reader's own normalization (`StdBackend::read_to_string`)
fn normalize_endings(contents: &str) -> String {
	if contents.contains('\r') {
		contents.replace("\r\n", "\n").replace('\r', "\n")
	} else {
		contents.to_owned()
	}
}

/// Whether two files hold the same content *as the projection reads it*.
///
/// Equal bytes are equal content, always. When the bytes differ and
/// `ignore_line_endings` is on — the default, and exactly what the VFS
/// applies when a middleware reads a text file — a text file is compared
/// again with `\r\n` and lone `\r` folded to `\n`. Those two files project to
/// the same instance, so rewriting the live one would rewrite every line of
/// every file in a CRLF checkout to change nothing the sync can see
fn same_content(staged_file: &Path, live_file: &Path) -> Result<bool, String> {
	let read = |path: &Path| fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()));

	let staged = read(staged_file)?;
	let live = read(live_file)?;

	if staged == live {
		return Ok(true);
	}

	if !Config::new().ignore_line_endings {
		return Ok(false);
	}

	let textual = live_file
		.extension()
		.and_then(|extension| extension.to_str())
		.is_some_and(|extension| TEXT_READ_EXTENSIONS.contains(&extension.to_lowercase().as_str()));

	if !textual {
		return Ok(false);
	}

	match (String::from_utf8(staged), String::from_utf8(live)) {
		(Ok(staged), Ok(live)) => Ok(normalize_endings(&staged) == normalize_endings(&live)),
		// Not text after all, whatever the extension claims: bytes decide
		_ => Ok(false),
	}
}

/// Every file (with its digest) and every directory under `root`, keyed by
/// path relative to it.
///
/// Directories are indexed too, not just walked: code scope syncs folders
/// **including empty ones** (Design §7.0), so "the stage has a folder the
/// disk does not" is a real difference that no file-level comparison sees
#[derive(Debug, Default)]
struct TreeIndex {
	files: BTreeMap<PathBuf, [u8; 32]>,
	dirs: BTreeSet<PathBuf>,
}

fn index_tree(root: &Path) -> Result<TreeIndex, String> {
	fn walk(base: &Path, current: &Path, index: &mut TreeIndex) -> std::io::Result<()> {
		for entry in fs::read_dir(current)? {
			let entry = entry?;
			let path = entry.path();
			let relative = path.strip_prefix(base).unwrap_or(&path).to_owned();

			if entry.file_type()?.is_dir() {
				index.dirs.insert(relative);
				walk(base, &path, index)?;
			} else {
				index.files.insert(relative, Sha256::digest(fs::read(&path)?).into());
			}
		}

		Ok(())
	}

	let mut index = TreeIndex::default();

	if root.is_dir() {
		walk(root, root, &mut index).map_err(|err| format!("failed to read {}: {err}", root.display()))?;
	}

	Ok(index)
}

/// What landing the stage on the live tree would actually change, path by
/// path — the difference between an apply and a rewrite.
///
/// The whole-root swap is correct but indiscriminate: it replaced 1 273 files
/// to change none of them, gave every one a new mtime, copied every one into
/// the backup batch and handed the watcher a burst of writes that meant
/// nothing. The plan is what makes the apply proportional to the difference
#[derive(Debug, Default)]
struct ApplyPlan {
	/// Live files the stage replaces with different content
	changed: Vec<PathBuf>,
	/// Files the stage carries and the live tree has no entry for
	created: Vec<PathBuf>,
	/// Live files the stage does not carry — they have left the projection
	removed: Vec<PathBuf>,
	/// Directories the stage has and the live tree does not (an empty folder
	/// is an instance too)
	created_dirs: Vec<PathBuf>,
	/// Directories the live tree has and the stage does not
	removed_dirs: Vec<PathBuf>,
	/// Files both sides already agree on: never opened for writing, never
	/// backed up, mtime untouched, no watcher event
	unchanged: usize,
	/// A path that is a file on one side and a directory on the other.
	/// Per-file replacement cannot express that without a moment where the
	/// root holds neither shape, so the whole-tree swap runs instead
	shape_change: Option<PathBuf>,
}

impl ApplyPlan {
	/// Nothing to write, nothing to remove, nothing to back up
	fn is_empty(&self) -> bool {
		self.changed.is_empty()
			&& self.created.is_empty()
			&& self.removed.is_empty()
			&& self.created_dirs.is_empty()
			&& self.removed_dirs.is_empty()
	}

	/// Live files this apply displaces — exactly what the backup batch holds
	fn backup_entries(&self) -> usize {
		self.changed.len() + self.removed.len()
	}

	fn written(&self) -> usize {
		self.changed.len() + self.created.len()
	}
}

/// Compares the assembled stage against the live tree (Design §7.4-A). Both
/// sides are read once; only paths whose digests disagree are read again for
/// the line-ending comparison
fn plan_apply(stage: &Path, live: &Path) -> Result<ApplyPlan, String> {
	let staged = index_tree(stage)?;
	let current = index_tree(live)?;

	let mut plan = ApplyPlan::default();

	for (relative, digest) in &staged.files {
		if current.dirs.contains(relative) {
			plan.shape_change = Some(relative.clone());
			return Ok(plan);
		}

		match current.files.get(relative) {
			Some(live_digest) => {
				if live_digest == digest || same_content(&stage.join(relative), &live.join(relative))? {
					plan.unchanged += 1;
				} else {
					plan.changed.push(relative.clone());
				}
			}
			None => plan.created.push(relative.clone()),
		}
	}

	for relative in current.files.keys() {
		if staged.dirs.contains(relative) {
			plan.shape_change = Some(relative.clone());
			return Ok(plan);
		}

		if !staged.files.contains_key(relative) {
			plan.removed.push(relative.clone());
		}
	}

	for relative in &staged.dirs {
		if !current.dirs.contains(relative) {
			plan.created_dirs.push(relative.clone());
		}
	}

	for relative in &current.dirs {
		if !staged.dirs.contains(relative) {
			plan.removed_dirs.push(relative.clone());
		}
	}

	Ok(plan)
}

/// Copies one file into place through a neighbouring temp file, so a reader
/// sees either the whole old file or the whole new one
fn replace_file(from: &Path, to: &Path) -> Result<(), String> {
	if let Some(parent) = to.parent() {
		fs::create_dir_all(parent).map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
	}

	let temp = to.with_file_name(format!(
		".{}.wsync-tmp",
		to.file_name().unwrap_or_default().to_string_lossy()
	));

	fs::copy(from, &temp).map_err(|err| format!("failed to stage {}: {err}", to.display()))?;

	if let Err(err) = rename_with_retry(&temp, to) {
		fs::remove_file(&temp).ok();

		return Err(format!("failed to replace {}: {err}", to.display()));
	}

	Ok(())
}

/// What the file-level apply has already done to the live tree — the ledger
/// a failure rolls back against
#[derive(Default)]
struct Touched {
	written: Vec<PathBuf>,
	removed: Vec<PathBuf>,
	made_dirs: Vec<PathBuf>,
}

/// Lands the plan on the live tree. Every file goes through temp+rename, and
/// the plan's whole backup is written before this runs, so the caller can put
/// back anything a mid-way failure touched
fn run_plan(plan: &ApplyPlan, stage: &Path, live: &Path, touched: &mut Touched) -> Result<(), String> {
	for relative in &plan.created_dirs {
		let dir = live.join(relative);

		fs::create_dir_all(&dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
		touched.made_dirs.push(relative.clone());
	}

	for relative in plan.changed.iter().chain(&plan.created) {
		replace_file(&stage.join(relative), &live.join(relative))?;
		touched.written.push(relative.clone());
	}

	for relative in &plan.removed {
		let path = live.join(relative);

		fs::remove_file(&path).map_err(|err| format!("failed to remove {}: {err}", path.display()))?;
		touched.removed.push(relative.clone());
	}

	// Deepest first: every file a removed directory held is in `removed`, so
	// what is left here is empty
	for relative in plan.removed_dirs.iter().rev() {
		let dir = live.join(relative);

		if dir.is_dir() {
			fs::remove_dir_all(&dir).map_err(|err| format!("failed to remove {}: {err}", dir.display()))?;
		}
	}

	Ok(())
}

/// Undoes exactly what `run_plan` managed to do before it failed: originals
/// come back from the backup, paths the stage invented are removed again, and
/// directories it created go with them. Reports whether the root came back
/// whole — a half-applied root is the one outcome this path may not leave
fn rollback_plan(plan: &ApplyPlan, live: &Path, backup: &Path, touched: &Touched) -> bool {
	let created: HashSet<&PathBuf> = plan.created.iter().collect();
	let mut restored = true;

	for relative in &touched.written {
		let path = live.join(relative);

		if created.contains(relative) {
			// Nothing stood here before the apply
			restored &= fs::remove_file(&path).is_ok();
			continue;
		}

		restored &= fs::copy(backup.join(relative), &path).is_ok();
	}

	for relative in &touched.removed {
		let path = live.join(relative);

		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).ok();
		}

		restored &= fs::copy(backup.join(relative), &path).is_ok();
	}

	for relative in touched.made_dirs.iter().rev() {
		// Best effort, and only while empty: a directory the apply created
		// and then filled is carried by the files restored above
		fs::remove_dir(live.join(relative)).ok();
	}

	restored
}

/// Applies the plan with its backup: every live file the plan overwrites or
/// removes is copied into `backup` — under the same relative layout the
/// whole-tree backup uses — before anything is written. Untouched files never
/// enter the backup, so the batch is the size of the change
fn apply_plan(plan: &ApplyPlan, stage: &Path, live: &Path, backup: &Path) -> Result<(), String> {
	for relative in plan.changed.iter().chain(&plan.removed) {
		let from = live.join(relative);
		let to = backup.join(relative);

		if let Some(parent) = to.parent() {
			fs::create_dir_all(parent).map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
		}

		fs::copy(&from, &to).map_err(|err| format!("failed to back up {}: {err}", from.display()))?;
	}

	let mut touched = Touched::default();

	if let Err(err) = run_plan(plan, stage, live, &mut touched) {
		let restored = rollback_plan(plan, live, backup, &touched);

		return Err(format!(
			"{err}; the file-level apply was rolled back and the live root was {}",
			if restored { "restored" } else { "NOT restored" }
		));
	}

	Ok(())
}

/// Materializes the pulled subtree into `stage_root` through the regular
/// write middleware (file/folder promotion, sidecars, name sanitization), so
/// the staged projection is byte-for-byte what syncback would produce
fn materialize_stage(
	stage_root: &Path,
	root_name: &str,
	root_class: &str,
	pulled: &PulledNode,
	sources: &HashMap<String, Vec<u8>>,
	context: &Context,
) -> Result<(), String> {
	let stage_vfs = Vfs::new(false);

	dir::write_dir(stage_root, &stage_vfs).map_err(|err| format!("failed to create the staging root: {err}"))?;

	let root_meta = Meta::new()
		.with_context(context)
		.with_source(Source::directory(stage_root));

	let root_snapshot = Snapshot::new()
		.with_name(root_name)
		.with_class(root_class)
		.with_meta(root_meta);

	let mut scratch = Tree::new(root_snapshot);
	let scratch_root = scratch.root_ref();

	for child in &pulled.children {
		let snapshot = to_snapshot(child, sources)?;

		write::apply_addition(snapshot.as_new(scratch_root), &mut scratch, &stage_vfs)
			.map_err(|err| format!("failed to stage {}: {err:#}", child.name))?;
	}

	Ok(())
}

/// The directory an instance's metadata puts behind it, if any: an explicit
/// `$path`, a path source, or a folder among its relevant source entries
fn directory_backing(meta: &Meta) -> Option<PathBuf> {
	match meta.source.get() {
		SourceKind::Project(_, project_path, node, _) => node
			.path
			.as_ref()
			.map(|custom| project_path.with_file_name(custom.path()).clean()),
		SourceKind::Path(path) => Some(path.clone()),
		SourceKind::None => None,
	}
	.or_else(|| {
		meta.source.relevant().iter().find_map(|entry| match entry {
			SourceEntry::Folder(path) => Some(path.clone()),
			_ => None,
		})
	})
}

/// Walks a slash-separated instance path down from the tree root, returning
/// the instance it names
fn resolve_instance_path(tree: &Tree, instance_path: &str) -> Option<rbx_dom_weak::types::Ref> {
	let mut current = tree.root_ref();

	for segment in instance_path.split('/').filter(|segment| !segment.is_empty()) {
		let next = tree
			.get_instance(current)?
			.children()
			.iter()
			.copied()
			.find(|child| tree.get_instance(*child).is_some_and(|child| child.name == segment))?;

		current = next;
	}

	Some(current)
}

/// The apply unit for one divergence entry: the topmost instance on its path
/// that has a directory behind it, as an instance path.
///
/// A service does not have to be mapped itself. `StarterPlayer` routinely has
/// no `$path` of its own while `StarterPlayer/StarterPlayerScripts` maps a
/// directory — the service is a container the project file names on the way
/// to the thing it actually syncs, and a project that nests this way cannot
/// always be restructured to flatten it. Walking down from the top is the
/// same rule a mapped top-level root already follows; for a virtual container
/// it simply stops one level deeper, at the first real directory.
///
/// `None` means nothing along this path is directory-backed, so there is no
/// fenced apply to run for it and nothing of it may be written.
pub fn staged_root_for(tree: &Tree, instance_path: &str) -> Option<String> {
	let mut current = tree.root_ref();
	let mut prefix = String::new();

	for segment in instance_path.split('/').filter(|segment| !segment.is_empty()) {
		let next = tree
			.get_instance(current)?
			.children()
			.iter()
			.copied()
			.find(|child| tree.get_instance(*child).is_some_and(|child| child.name == segment))?;

		current = next;

		if !prefix.is_empty() {
			prefix.push('/');
		}

		prefix.push_str(segment);

		if tree.get_meta(current).and_then(directory_backing).is_some() {
			return Some(prefix);
		}
	}

	None
}

/// Everything needed to apply one root, resolved from the live tree up front
/// so no lock is held across plugin awaits
struct RootTarget {
	id: rbx_dom_weak::types::Ref,
	/// The instance's own name — what the staged snapshot is called. For a
	/// nested root this is the leaf (`StarterPlayerScripts`), not the path
	name: String,
	/// Filesystem-safe form of the root's instance path, used for the staging
	/// and backup directory names. A nested root would otherwise ask for a
	/// directory inside a directory nobody created
	key: String,
	class: String,
	live_dir: PathBuf,
	context: Context,
}

fn resolve_root(state: &AppState, root_path: &str) -> Result<RootTarget, String> {
	let tree = state.core.tree();

	let id = resolve_instance_path(&tree, root_path).ok_or_else(|| format!("{root_path} is not a served root"))?;

	let meta = tree
		.get_meta(id)
		.ok_or_else(|| format!("{root_path} has no sync metadata"))?;

	let live_dir = directory_backing(meta)
		.ok_or_else(|| format!("{root_path} has no directory behind it ($path), and neither does anything above it"))?;

	let instance = tree.get_instance(id);

	let name = instance
		.map(|instance| instance.name.clone())
		.unwrap_or_else(|| root_path.rsplit('/').next().unwrap_or(root_path).to_owned());

	let class = instance
		.map(|instance| instance.class.to_string())
		.unwrap_or_else(|| name.clone());

	Ok(RootTarget {
		id,
		name,
		key: root_path.replace('/', "."),
		class,
		live_dir,
		context: meta.context.clone(),
	})
}

/// Re-derives one root's subtree from what is on disk right now and stamps a
/// baseline for everything under it. The changes are dropped: the caller has
/// just put those bytes there on purpose, and replaying them at Studio would
/// be an echo of its own content.
///
/// Callers hold the VFS pause, so nothing else re-reads these files
fn refresh_root(state: &AppState, id: rbx_dom_weak::types::Ref, vfs: &crate::vfs::Vfs) {
	let mut tree = state.core.tree();

	if !tree.exists(id) {
		return;
	}

	read::process_changes(id, &mut tree, vfs);

	// Baselines stamp for every touched instance (Design §7.4): both sides
	// provably agree on the content now on disk
	let engine = state.core.conflicts();

	for id in conflict::subtree_refs(&tree, id) {
		if let Some(instance) = tree.get_instance(id) {
			let content = ContentState::new(&instance.name, &instance.class, &instance.properties);

			engine.stamp(id, content.hash);
		}
	}
}

/// Re-derives every named root after something moved files behind the tree's
/// back — the backlog sweep at the end of a Studio-first apply removes the
/// disk-only entries that lost to Studio while the VFS is paused, so those
/// deletions reach no walker on their own.
///
/// Leaving that out is how the tree ended up still describing files that no
/// longer existed: the next unrelated event re-walked the root and pushed the
/// whole accumulated difference at Studio as one burst
pub fn refresh_roots(state: &AppState, roots: &BTreeSet<String>) {
	let vfs = state.core.vfs();

	for root_name in roots {
		let id = {
			let tree = state.core.tree();
			resolve_instance_path(&tree, root_name)
		};

		match id {
			Some(id) => refresh_root(state, id, &vfs),
			None => debug!("Not refreshing {root_name} after the sweep: it is no longer in the tree"),
		}
	}
}

/// What one root's apply actually did to the disk — the receipt's facts
pub struct RootOutcome {
	/// `<transfer>/<root>`, named only when something was displaced into it.
	/// `None` means the disk already held the staged content: no backup
	/// directory was created, because there was nothing to put in it
	pub backup: Option<String>,
	/// Live files displaced into the backup (overwritten or removed)
	pub entries: usize,
	/// Files written (replaced + created)
	pub written: usize,
	/// Files left exactly as they were, mtime included
	pub unchanged: usize,
	/// The whole-tree swap ran, so the backup holds the entire former root
	pub whole_tree: bool,
}

impl RootOutcome {
	/// How a recovery puts this root back. Unchanged for every root that has
	/// a backup — `backupScope` is what says how much of the root that
	/// backup covers, because a file-level backup holds only the files the
	/// apply displaced and is restored *over* the root, not *as* it
	fn recovery_action(&self) -> &'static str {
		if self.backup.is_some() {
			"restoreBackup"
		} else {
			"none"
		}
	}

	/// What the backup covers: the whole former root, only the displaced
	/// files, or nothing at all
	fn backup_scope(&self) -> &'static str {
		if self.whole_tree {
			"root"
		} else if self.backup.is_some() {
			"files"
		} else {
			"none"
		}
	}

	fn json(&self, root_name: &str) -> Value {
		json!({
			"root": root_name,
			"backup": self.backup,
			"entries": self.entries,
			"written": self.written,
			"unchanged": self.unchanged,
			"recoveryAction": self.recovery_action(),
			"backupScope": self.backup_scope(),
		})
	}
}

/// Applies one root: pull → stage → fence → (preserve + overlay when
/// studio-first) → plan → file-level apply (or the whole-tree swap) →
/// refresh tree → stamp baselines.
///
/// The apply is incremental at the file level: a staged file the disk
/// already holds is not rewritten, not backed up and not given a new mtime,
/// and a root the disk already matches is not touched at all
async fn apply_root(
	state: &AppState,
	transfer_dir: &Path,
	transfer_name: &str,
	root_name: &str,
	transfer_bytes: &mut u64,
	studio_first: Option<&StudioFirstOptions>,
) -> Result<RootOutcome, String> {
	let target = resolve_root(state, root_name)?;

	// A missing live directory is not a failure: the root is mapped but has
	// nothing on disk yet (a fresh clone, or the folder was deleted precisely
	// so Studio could repopulate it). Refusing here would leave the only path
	// that rebuilds it — pulling Studio down — permanently blocked, so create
	// it and let the apply fill it. An empty directory fences and swaps like
	// any other.
	if !target.live_dir.is_dir() {
		fs::create_dir_all(&target.live_dir).map_err(|err| {
			format!(
				"could not create the live directory {} for {root_name}: {err}",
				target.live_dir.display()
			)
		})?;
	}

	// The fence generation: captured before anything is pulled, verified
	// again under the pause right before the swap
	let fence = fingerprint_dir(&target.live_dir)?;

	// Pull structure
	// The path rides along with the ref so the plugin has a second way to
	// find the instance. A ref only resolves if the plugin bound it, and a
	// node the plugin never adopted — a nested container, an instance created
	// since the session started — has no binding to look up. The path is
	// resolvable from `game` no matter what the session holds
	let value = plugin_request(
		state,
		"read_subtree",
		json!({ "ref": util::format_ref(target.id), "path": root_name }),
		BULK_OP_TIMEOUT_MS,
	)
	.await
	.map_err(|err| format!("read_subtree for {root_name} failed: {err}"))?;

	let (pulled, claims) = parse_read_subtree(&value)?;

	// Byte budgets (Design §7.4-A)
	let mut root_bytes = 0u64;

	for (reference, claim) in &claims {
		if claim.bytes > BULK_SCRIPT_MAX_BYTES {
			return Err(format!(
				"script {reference} claims {} bytes, over the {BULK_SCRIPT_MAX_BYTES}-byte per-script bound",
				claim.bytes
			));
		}

		root_bytes += claim.bytes;
	}

	if root_bytes > BULK_ROOT_MAX_BYTES {
		return Err(format!(
			"{root_name} carries {root_bytes} source bytes, over the {BULK_ROOT_MAX_BYTES}-byte per-root bound"
		));
	}

	*transfer_bytes += root_bytes;

	if *transfer_bytes > BULK_TRANSFER_MAX_BYTES {
		return Err(format!("the transfer exceeds the {BULK_TRANSFER_MAX_BYTES}-byte bound"));
	}

	// Pull sources (chunked, hash-verified)
	let mut sources = HashMap::new();

	for (reference, claim) in &claims {
		sources.insert(reference.clone(), pull_source(state, reference, claim).await?);
	}

	// Stage the complete same-volume copy
	let stage_root = transfer_dir.join(STAGE_DIR).join(&target.key);

	fs::create_dir_all(transfer_dir.join(STAGE_DIR))
		.map_err(|err| format!("failed to create the staging directory: {err}"))?;

	materialize_stage(
		&stage_root,
		&target.name,
		&target.class,
		&pulled,
		&sources,
		&target.context,
	)?;

	// Fence, swap and refresh — synchronous, VFS paused throughout
	let backup_path = transfer_dir.join(&target.key);
	let vfs = state.core.vfs();

	vfs.pause();

	let swapped = (|| -> Result<RootOutcome, String> {
		let current = fingerprint_dir(&target.live_dir)?;

		if current != fence {
			return Err(format!(
				"the contents of {} changed while the apply was running; refusing to overwrite this root",
				target.live_dir.display()
			));
		}

		if studio_first.is_some() {
			// Disk-only and out-of-projection files stay on the live disk:
			// the stage is overlaid so the apply carries them forward instead
			// of displacing them into the backup. Overlaying before the plan
			// is what makes them compare equal and drop out of it
			overlay_unclaimed(&target.live_dir, &stage_root, &target.context)?;
		}

		// What this apply would actually change. Files the stage and the disk
		// already agree on are not rewritten, not backed up and not given a
		// new mtime — and the watcher, which used to see a burst of writes
		// that changed nothing, hears nothing about them
		let plan = plan_apply(&stage_root, &target.live_dir)?;

		if plan.is_empty() {
			info!(
				"{root_name}: disk already matches the staged projection, {} file(s) unchanged; \
				 nothing written and no backup taken",
				plan.unchanged
			);

			return Ok(RootOutcome {
				backup: None,
				entries: 0,
				written: 0,
				unchanged: plan.unchanged,
				whole_tree: false,
			});
		}

		if let Some(options) = studio_first {
			// Preserve the disk originals of the `differs` entries this apply
			// really replaces — the fence just proved these bytes are the
			// generation the comparison ran against (Design §7.0). An entry
			// whose file the stage agrees with byte for byte was displaced by
			// nothing, so there is no original to keep and nothing to backlog.
			// A shape change takes the whole-tree swap, which displaces the
			// entire root, so there the whole preserve set stands
			let displaced: HashSet<PathBuf> = plan
				.changed
				.iter()
				.chain(&plan.removed)
				.map(|relative| target.live_dir.join(relative))
				.collect();

			for (live_path, preserve_target) in &options.preserve {
				if !live_path.starts_with(&target.live_dir) || !live_path.is_file() {
					continue;
				}

				if plan.shape_change.is_none() && !displaced.contains(live_path) {
					continue;
				}

				if let Some(parent) = preserve_target.parent() {
					fs::create_dir_all(parent)
						.map_err(|err| format!("failed to create the review preservation directory: {err}"))?;
				}

				fs::copy(live_path, preserve_target).map_err(|err| {
					format!(
						"failed to preserve {} into the review store: {err}",
						live_path.display()
					)
				})?;
			}
		}

		// The ordinary path: replace only what differs, in place. The live
		// directory is never renamed, so no watch has to move and nothing
		// outside the plan is touched
		if plan.shape_change.is_none() {
			apply_plan(&plan, &stage_root, &target.live_dir, &backup_path)?;

			return Ok(RootOutcome {
				backup: (plan.backup_entries() > 0).then(|| format!("{transfer_name}/{}", target.key)),
				entries: plan.backup_entries(),
				written: plan.written(),
				unchanged: plan.unchanged,
				whole_tree: false,
			});
		}

		// A path is a file on one side and a directory on the other. Replacing
		// that per path would pass through a state where the root holds
		// neither shape, so the whole root moves at once instead
		if let Some(path) = &plan.shape_change {
			debug!(
				"{root_name}: {} changes shape between disk and Studio; applying the whole root",
				path.display()
			);
		}

		// Counted while both trees still stand: the backup is about to hold
		// every live file, and the stage is about to become the root
		let displaced = current.len();
		let written = count_files(&stage_root);

		// The watch must move off the directory before it is renamed away
		// (inotify-style watchers follow inodes, not paths, and on Windows the
		// watcher's own handle is enough to refuse the rename outright)
		vfs.unwatch(&target.live_dir).ok();

		// Both moves give a transient lock a chance to clear, and either one
		// still failing lands on the file-level path rather than failing the
		// apply: the content is correct and already staged, and a handle
		// somebody else holds is not a reason to refuse to deliver it
		let backed_up = match rename_with_retry(&target.live_dir, &backup_path) {
			Ok(()) => true,
			Err(err) if is_locked(&err) => {
				warn!(
					"Could not move {} into the backup ({err}); copying it instead",
					target.live_dir.display()
				);

				// The backup must exist before anything is overwritten, so this
				// copy is not optional and its failure is fatal
				copy_dir_all(&target.live_dir, &backup_path)
					.map_err(|err| format!("failed to copy the live root into the backup: {err}"))?;

				false
			}
			Err(err) => return Err(format!("failed to move the live root into the backup: {err}")),
		};

		let swapped = if backed_up {
			// The live root is gone; the stage only has to take its place
			rename_with_retry(&stage_root, &target.live_dir)
		} else {
			// The live root is still in place, with a copy of it in the backup,
			// so the only way forward is the file-level one
			Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
		};

		if let Err(err) = swapped {
			if !is_locked(&err) {
				let restored = backed_up && fs::rename(&backup_path, &target.live_dir).is_ok();
				vfs.watch(&target.live_dir, true).ok();

				return Err(format!(
					"failed to swap the staged root into place: {err}; the backup was {}",
					if restored { "restored" } else { "NOT restored" }
				));
			}

			if backed_up {
				warn!(
					"Could not move the stage into {} ({err}); applying it file by file instead",
					target.live_dir.display()
				);
			}

			if let Err(fallback) = sync_tree_in_place(&stage_root, &target.live_dir) {
				// The file-level path is the last one there is. Put the original
				// back so the root is whole, and say plainly that nothing landed
				let restored = if backed_up {
					fs::remove_dir_all(&target.live_dir).ok();
					fs::rename(&backup_path, &target.live_dir).is_ok()
				} else {
					sync_tree_in_place(&backup_path, &target.live_dir).is_ok()
				};

				vfs.watch(&target.live_dir, true).ok();

				return Err(format!(
					"failed to swap the staged root into place: {err}; the file-level apply also failed ({fallback}); \
					 the backup was {}",
					if restored { "restored" } else { "NOT restored" }
				));
			}
		}

		vfs.watch(&target.live_dir, true).ok();

		Ok(RootOutcome {
			backup: Some(format!("{transfer_name}/{}", target.key)),
			entries: displaced,
			written,
			unchanged: 0,
			whole_tree: true,
		})
	})();

	let outcome = match swapped {
		Ok(outcome) => outcome,
		Err(message) => {
			vfs.resume();
			return Err(message);
		}
	};

	// Refresh the daemon tree from the swapped root. The resulting changes
	// are dropped deliberately — Studio already holds exactly this state
	// (it is where the bytes came from), so replaying them would be an echo
	refresh_root(state, target.id, &vfs);

	vfs.resume();

	info!(
		"Keep-Studio apply committed root {root_name}: {} file(s) written, {} unchanged, {} backed up ({})",
		outcome.written,
		outcome.unchanged,
		outcome.entries,
		outcome.backup.as_deref().unwrap_or("no backup needed"),
	);

	Ok(outcome)
}

/// How many files a tree holds, without reading any of them
fn count_files(dir: &Path) -> usize {
	let Ok(entries) = fs::read_dir(dir) else {
		return 0;
	};

	entries
		.flatten()
		.map(|entry| {
			let path = entry.path();

			if path.is_dir() {
				count_files(&path)
			} else {
				1
			}
		})
		.sum()
}

/// What a whole transfer did to the disk — the receipt's disk-side facts.
///
/// `backups` keeps its old shape (the names of the backup directories this
/// transfer wrote) and is simply empty when nothing was displaced
#[derive(Default)]
pub struct TransferOutcome {
	pub backups: Vec<String>,
	/// Live files displaced into the backup across every root
	pub entries: usize,
	/// Files written across every root
	pub written: usize,
	/// Files left exactly as they were
	pub unchanged: usize,
	/// Per-root detail, in commit order
	pub roots: Vec<Value>,
}

impl TransferOutcome {
	fn record(&mut self, root_name: &str, root: RootOutcome) {
		self.entries += root.entries;
		self.written += root.written;
		self.unchanged += root.unchanged;
		self.roots.push(root.json(root_name));

		if let Some(backup) = root.backup {
			self.backups.push(backup);
		}
	}
}

/// Runs the fenced per-root apply for every staged root in sequence. Returns
/// what landed on success, or the pinned failure payload with its HTTP
/// status (the terminal, replayable partial receipt — earlier committed roots
/// stand, never rolled back, and their backups are preserved indefinitely
/// because the transfer never marks complete)
pub async fn apply_roots(
	state: &AppState,
	transfer_id: &str,
	staged_roots: &BTreeSet<String>,
	studio_first: Option<&StudioFirstOptions>,
) -> Result<TransferOutcome, (StatusCode, Value)> {
	if staged_roots.is_empty() {
		return Ok(TransferOutcome::default());
	}

	let workspace_dir = state.core.project().workspace_dir.clone();
	let backups_root = workspace_dir.join(BACKUPS_DIR);

	prune_backups(&backups_root);

	let stamp = Utc::now().format(BACKUP_STAMP_FORMAT);
	let transfer_name = format!("{stamp}-{transfer_id}");
	let transfer_dir = backups_root.join(&transfer_name);

	if let Err(err) = fs::create_dir_all(&transfer_dir) {
		return Err((
			StatusCode::INTERNAL_SERVER_ERROR,
			json!({
				"ok": false,
				"error": format!("Failed to create the backup directory: {err}"),
			}),
		));
	}

	let mut outcome = TransferOutcome::default();
	let mut transfer_bytes = 0u64;

	for root_name in staged_roots {
		let applied = apply_root(
			state,
			&transfer_dir,
			&transfer_name,
			root_name,
			&mut transfer_bytes,
			studio_first,
		)
		.await;

		match applied {
			Ok(root) => outcome.record(root_name, root),
			Err(error) => {
				warn!("Keep-Studio apply failed at root {root_name}: {error}");

				return Err((
					StatusCode::OK,
					json!({
						"ok": false,
						"action": "partial",
						"recoveryRequired": true,
						"failedRoot": root_name,
						"error": error,
						"committedRoots": outcome.roots,
					}),
				));
			}
		}
	}

	// Leftover staging directory (the file-level apply copies out of it
	// rather than renaming it away, so it is normally still here)
	fs::remove_dir_all(transfer_dir.join(STAGE_DIR)).ok();

	if outcome.backups.is_empty() {
		// Nothing was displaced, so there is no batch: a transfer directory
		// holding only the emptied stage is not a backup of anything, and
		// without a completion marker it would never be pruned either
		fs::remove_dir_all(&transfer_dir).ok();

		info!(
			"{}: disk already matches, {} files unchanged",
			if studio_first.is_some() {
				"Studio-first"
			} else {
				"Keep-Studio"
			},
			outcome.unchanged
		);
	} else {
		// The completion marker is what makes this transfer's backups prunable
		let marker = json!({
			"choiceId": transfer_id,
			"completedAt": Utc::now().to_rfc3339(),
			"roots": staged_roots.iter().collect::<Vec<_>>(),
			"entries": outcome.entries,
			"written": outcome.written,
			"unchanged": outcome.unchanged,
		});

		if let Err(err) = fs::write(transfer_dir.join(BACKUP_COMPLETE_MARKER), marker.to_string()) {
			warn!("Failed to write the backup completion marker: {err}");
		}
	}

	prune_backups(&backups_root);

	Ok(outcome)
}

/// Applies the Keep-Studio decision for every staged root in sequence,
/// producing the pinned success or partial receipt (Design §7.4-A)
pub async fn apply_keep_studio(state: &AppState, choice_id: &str, staged_roots: BTreeSet<String>) -> Response {
	match apply_roots(state, choice_id, &staged_roots, None).await {
		Ok(outcome) => Json(json!({
			"ok": true,
			"applied": true,
			"direction": "studio",
			"backups": outcome.backups,
			// What the apply actually did: `entries: 0` with nothing in
			// `backups` means the disk already held the staged projection
			"entries": outcome.entries,
			"written": outcome.written,
			"unchanged": outcome.unchanged,
			"committedRoots": outcome.roots,
		}))
		.into_response(),
		Err((status, payload)) => (status, Json(payload)).into_response(),
	}
}

// Backup pruning

/// Age of a transfer directory, derived from the stamp prefix of its name
/// (`YYYY-MM-DDTHH-MM-SSZ-...`). Unparseable names count as fresh so they
/// are only ever pruned by the count rule
fn transfer_age_days(name: &str) -> Option<i64> {
	let stamp = name.get(..20)?;
	let parsed = NaiveDateTime::parse_from_str(stamp, BACKUP_STAMP_FORMAT).ok()?;

	Some((Utc::now().naive_utc() - parsed).num_days())
}

/// Prunes completed-transfer backups: older than `BACKUP_KEEP_DAYS` or
/// beyond the newest `BACKUP_KEEP_COUNT`. Directories without the completion
/// marker are partial or in-flight transfers and are never touched
pub fn prune_backups(backups_root: &Path) {
	let Ok(entries) = fs::read_dir(backups_root) else {
		return;
	};

	let mut completed: Vec<(String, PathBuf)> = entries
		.filter_map(|entry| {
			let entry = entry.ok()?;
			let path = entry.path();

			if !path.is_dir() || !path.join(BACKUP_COMPLETE_MARKER).is_file() {
				return None;
			}

			Some((entry.file_name().to_string_lossy().into_owned(), path))
		})
		.collect();

	// The fixed-width UTC stamp prefix makes name order chronological;
	// newest first
	completed.sort_by(|(a, _), (b, _)| b.cmp(a));

	for (index, (name, path)) in completed.iter().enumerate() {
		let beyond_count = index >= BACKUP_KEEP_COUNT;
		let too_old = transfer_age_days(name).is_some_and(|days| days > BACKUP_KEEP_DAYS);

		if beyond_count || too_old {
			debug!("Pruning completed backup {name}");
			fs::remove_dir_all(path).ok();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn transfer_ages_parse_from_the_stamp_prefix() {
		let old = "2000-01-02T03-04-05Z-abcdef";
		assert!(transfer_age_days(old).unwrap() > BACKUP_KEEP_DAYS);

		let fresh = format!("{}-abc", Utc::now().format(BACKUP_STAMP_FORMAT));
		assert_eq!(transfer_age_days(&fresh), Some(0));

		assert_eq!(transfer_age_days("garbage"), None);
		assert_eq!(transfer_age_days(""), None);
	}

	#[test]
	fn prune_keeps_partials_and_the_newest_completed() {
		let scratch = tempfile::tempdir().unwrap();
		let root = scratch.path();

		let make = |name: &str, complete: bool| {
			let dir = root.join(name);
			fs::create_dir_all(&dir).unwrap();
			fs::write(dir.join("payload"), b"x").unwrap();

			if complete {
				fs::write(dir.join(BACKUP_COMPLETE_MARKER), b"{}").unwrap();
			}
		};

		// An ancient completed transfer, an ancient partial one, and more
		// recent completed transfers than the keep count
		make("2000-01-01T00-00-00Z-old-complete", true);
		make("2000-01-01T00-00-00Z-old-partial", false);

		let stamp = Utc::now().format(BACKUP_STAMP_FORMAT);

		for index in 0..(BACKUP_KEEP_COUNT + 3) {
			make(&format!("{stamp}-recent-{index:03}"), true);
		}

		prune_backups(root);

		assert!(!root.join("2000-01-01T00-00-00Z-old-complete").exists(), "age rule");
		assert!(root.join("2000-01-01T00-00-00Z-old-partial").exists(), "partials stay");

		let survivors = fs::read_dir(root)
			.unwrap()
			.filter_map(|entry| entry.ok())
			.filter(|entry| entry.path().join(BACKUP_COMPLETE_MARKER).is_file())
			.count();

		assert_eq!(survivors, BACKUP_KEEP_COUNT, "count rule keeps the newest 32");

		// The newest entries survived, the oldest recent ones went
		assert!(root
			.join(format!("{stamp}-recent-{:03}", BACKUP_KEEP_COUNT + 2))
			.exists());
		assert!(!root.join(format!("{stamp}-recent-000")).exists());
	}

	/// A live tree and a stage that holds exactly the same thing
	fn twin_trees() -> (tempfile::TempDir, PathBuf, PathBuf) {
		let scratch = tempfile::tempdir().unwrap();
		let live = scratch.path().join("live");
		let stage = scratch.path().join("stage");

		for root in [&live, &stage] {
			fs::create_dir_all(root.join("Nested")).unwrap();
			fs::write(root.join("Hello.luau"), b"return \"hello\"\n").unwrap();
			fs::write(root.join("ModA.luau"), b"return \"mod-a\"\n").unwrap();
			fs::write(root.join("Nested").join("Deep.luau"), b"return \"deep\"\n").unwrap();
		}

		(scratch, live, stage)
	}

	/// Every file under `root`, by relative path, with its mtime
	fn mtimes(root: &Path) -> BTreeMap<PathBuf, std::time::SystemTime> {
		index_tree(root)
			.unwrap()
			.files
			.keys()
			.map(|relative| {
				let stamp = fs::metadata(root.join(relative)).unwrap().modified().unwrap();
				(relative.clone(), stamp)
			})
			.collect()
	}

	#[test]
	fn a_stage_the_disk_already_holds_writes_nothing_and_backs_up_nothing() {
		let (scratch, live, stage) = twin_trees();
		let backup = scratch.path().join("backup");

		let plan = plan_apply(&stage, &live).unwrap();

		assert!(plan.is_empty(), "nothing to do: {plan:?}");
		assert_eq!(plan.unchanged, 3);
		assert_eq!(plan.backup_entries(), 0);
		assert_eq!(plan.written(), 0);
		assert!(plan.shape_change.is_none());

		// Even run outright, an empty plan is a no-op: no write, no backup
		// directory, no new mtime anywhere
		let before = mtimes(&live);
		apply_plan(&plan, &stage, &live, &backup).unwrap();

		assert!(!backup.exists(), "an unchanged root creates no backup batch");
		assert_eq!(mtimes(&live), before, "untouched files keep their mtime");
	}

	#[test]
	fn one_changed_file_is_written_and_backed_up_alone() {
		let (scratch, live, stage) = twin_trees();
		let backup = scratch.path().join("backup");

		fs::write(stage.join("ModA.luau"), b"return \"studio mod-a\"\n").unwrap();

		let plan = plan_apply(&stage, &live).unwrap();

		assert_eq!(plan.changed, vec![PathBuf::from("ModA.luau")]);
		assert!(plan.created.is_empty() && plan.removed.is_empty());
		assert_eq!(plan.unchanged, 2);
		assert_eq!(plan.backup_entries(), 1);

		let before = mtimes(&live);

		apply_plan(&plan, &stage, &live, &backup).unwrap();

		// Exactly that file was written
		assert_eq!(
			fs::read_to_string(live.join("ModA.luau")).unwrap(),
			"return \"studio mod-a\"\n"
		);

		let after = mtimes(&live);

		for relative in before.keys() {
			if relative == Path::new("ModA.luau") {
				continue;
			}

			assert_eq!(
				before[relative],
				after[relative],
				"{} must not be rewritten",
				relative.display()
			);
		}

		// The backup batch holds that one displaced original, nothing else
		let backed_up = index_tree(&backup).unwrap();

		assert_eq!(
			backed_up.files.keys().collect::<Vec<_>>(),
			vec![&PathBuf::from("ModA.luau")]
		);
		assert_eq!(
			fs::read_to_string(backup.join("ModA.luau")).unwrap(),
			"return \"mod-a\"\n"
		);

		// No temp file survived the rename
		assert!(!live.join(".ModA.luau.wsync-tmp").exists());
	}

	#[test]
	fn additions_removals_and_folders_are_planned_per_path() {
		let (scratch, live, stage) = twin_trees();
		let backup = scratch.path().join("backup");

		fs::write(stage.join("Extra.luau"), b"return \"extra\"\n").unwrap();
		fs::remove_file(stage.join("Hello.luau")).unwrap();
		fs::create_dir_all(stage.join("Empty")).unwrap();
		fs::create_dir_all(live.join("Gone")).unwrap();

		let plan = plan_apply(&stage, &live).unwrap();

		assert_eq!(plan.created, vec![PathBuf::from("Extra.luau")]);
		assert_eq!(plan.removed, vec![PathBuf::from("Hello.luau")]);
		assert_eq!(plan.created_dirs, vec![PathBuf::from("Empty")]);
		assert_eq!(plan.removed_dirs, vec![PathBuf::from("Gone")]);
		assert_eq!(plan.unchanged, 2);

		let before = mtimes(&live);

		apply_plan(&plan, &stage, &live, &backup).unwrap();

		assert!(live.join("Extra.luau").is_file());
		assert!(!live.join("Hello.luau").exists());
		assert!(live.join("Empty").is_dir(), "an empty folder is an instance too");
		assert!(!live.join("Gone").exists());

		let after = mtimes(&live);

		for relative in [PathBuf::from("ModA.luau"), PathBuf::from("Nested").join("Deep.luau")] {
			assert_eq!(
				before[&relative],
				after[&relative],
				"{} must not be rewritten",
				relative.display()
			);
		}

		// A removed file is displaced and backed up; a created one displaces
		// nothing, so it has no original to keep
		let backed_up = index_tree(&backup).unwrap();

		assert_eq!(
			backed_up.files.keys().collect::<Vec<_>>(),
			vec![&PathBuf::from("Hello.luau")]
		);
	}

	#[test]
	fn line_endings_alone_are_not_a_change_for_what_the_reader_normalizes() {
		let scratch = tempfile::tempdir().unwrap();
		let live = scratch.path().join("live");
		let stage = scratch.path().join("stage");

		fs::create_dir_all(&live).unwrap();
		fs::create_dir_all(&stage).unwrap();

		// The disk checkout has CRLF, Studio hands back LF
		fs::write(live.join("Hello.luau"), b"local a = 1\r\nreturn a\r\n").unwrap();
		fs::write(stage.join("Hello.luau"), b"local a = 1\nreturn a\n").unwrap();

		// A byte reader's file: every byte it holds is content
		fs::write(live.join("Model.rbxmx"), b"<roblox>\r\n</roblox>\r\n").unwrap();
		fs::write(stage.join("Model.rbxmx"), b"<roblox>\n</roblox>\n").unwrap();

		let plan = plan_apply(&stage, &live).unwrap();

		assert_eq!(plan.unchanged, 1, "the script reads the same either way");
		assert_eq!(plan.changed, vec![PathBuf::from("Model.rbxmx")]);

		// A real edit still lands, line endings or not
		fs::write(stage.join("Hello.luau"), b"local a = 2\nreturn a\n").unwrap();

		let plan = plan_apply(&stage, &live).unwrap();

		assert!(plan.changed.contains(&PathBuf::from("Hello.luau")));
	}

	#[test]
	fn a_path_that_changes_shape_falls_back_to_the_whole_tree() {
		let (scratch, live, stage) = twin_trees();

		// `Hello` gained children: the file becomes a directory
		fs::remove_file(stage.join("Hello.luau")).unwrap();
		fs::create_dir_all(stage.join("Hello.luau")).unwrap();
		fs::write(stage.join("Hello.luau").join("init.luau"), b"return 1\n").unwrap();

		let plan = plan_apply(&stage, &live).unwrap();

		assert_eq!(plan.shape_change, Some(PathBuf::from("Hello.luau")));

		drop(scratch);
	}

	#[test]
	fn a_failed_apply_puts_the_root_back() {
		let (scratch, live, stage) = twin_trees();
		let backup = scratch.path().join("backup");

		fs::write(stage.join("ModA.luau"), b"return \"studio mod-a\"\n").unwrap();
		fs::write(stage.join("Extra.luau"), b"return \"extra\"\n").unwrap();

		let mut plan = plan_apply(&stage, &live).unwrap();

		// A removal of something that is not there fails the run after the
		// writes have already landed — the shape of any mid-apply failure
		plan.removed.push(PathBuf::from("Nowhere.luau"));

		// Its backup copy has to exist for the rollback to restore it, which
		// is exactly what the missing source denies
		let error = apply_plan(&plan, &stage, &live, &backup).unwrap_err();

		assert!(error.contains("failed to back up"), "error: {error}");

		// Nothing was written: the backup runs before the first write
		assert_eq!(
			fs::read_to_string(live.join("ModA.luau")).unwrap(),
			"return \"mod-a\"\n"
		);
		assert!(!live.join("Extra.luau").exists());

		// A failure *inside* the run — here a file the stage wants under a
		// path that is already a file — is rolled back whole, after two
		// writes have already landed
		plan.removed.clear();
		plan.created.push(PathBuf::from("Hello.luau").join("Child.luau"));

		let error = apply_plan(&plan, &stage, &live, &backup).unwrap_err();

		assert!(error.contains("rolled back"), "error: {error}");
		assert!(error.contains("was restored"), "error: {error}");
		assert_eq!(
			fs::read_to_string(live.join("ModA.luau")).unwrap(),
			"return \"mod-a\"\n"
		);
		assert!(!live.join("Extra.luau").exists(), "a created file is removed again");
	}

	#[test]
	fn fingerprints_catch_edits_additions_and_removals() {
		let scratch = tempfile::tempdir().unwrap();
		let dir = scratch.path();

		fs::create_dir_all(dir.join("nested")).unwrap();
		fs::write(dir.join("a.luau"), b"return 1").unwrap();
		fs::write(dir.join("nested").join("b.luau"), b"return 2").unwrap();

		let base = fingerprint_dir(dir).unwrap();
		assert_eq!(base.len(), 2);
		assert_eq!(fingerprint_dir(dir).unwrap(), base);

		fs::write(dir.join("a.luau"), b"return 9").unwrap();
		assert_ne!(fingerprint_dir(dir).unwrap(), base, "edit detected");

		fs::write(dir.join("a.luau"), b"return 1").unwrap();
		fs::write(dir.join("c.luau"), b"new").unwrap();
		assert_ne!(fingerprint_dir(dir).unwrap(), base, "addition detected");

		fs::remove_file(dir.join("c.luau")).unwrap();
		fs::remove_file(dir.join("nested").join("b.luau")).unwrap();
		assert_ne!(fingerprint_dir(dir).unwrap(), base, "removal detected");
	}
}
