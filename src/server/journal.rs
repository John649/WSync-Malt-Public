//! The branch journal (docs/branches.md): an append-only record of what a
//! Studio changed since its checkpoint.
//!
//! A branch is not diffed against main — it is *tracked*. The plugin sees
//! every change the moment it happens and pushes it here in batches, keyed by
//! the checkpoint numbering both plugins agreed on when the branch was cut. A
//! merge is then a replay of one journal into the other Studio, not a
//! reconstruction of what might have happened.
//!
//! Everything lives under `<workspace>/.wsync-branches/journal/`:
//!
//! - `<checkpoint>.jsonl` — one JSON line per event, in arrival order. A
//!   line's 1-based number *is* its sequence: `?after=<seq>` skips that many
//!   lines. Nothing is ever rewritten, so a reader that has seen `n` lines
//!   can never be shown a different first `n`.
//! - `marks.json` — main's daemon's record of which checkpoint each branch
//!   was cut at, so a merge knows where in main's own journal to start.
//! - `blobs/<id>` — the serialized subtrees an `add` event refers to.
//! - `blobs.json` — which of those ids have arrived and which are still owed,
//!   so a reader can tell "no blob yet" from "no blob ever".
//!
//! BLOBS TRAVEL WITH THE FLUSH. An `add` event names a blob the plugin pinned
//! in its own transfer store, and for a long time that was the only copy: a
//! merge drained it out of the branch's Studio at `finish`. A branch whose
//! Studio had been closed therefore could not be merged at all — there was
//! nobody left to answer, and the bytes went with the window. So an append
//! that references blob ids now pulls them straight away: the batch is
//! answered first (a flush has to stay fast), then a background pass drains
//! each new blob over the same `branch_blob_open`/`read`/`close` ops the CLI
//! uses and stores it under `blobs/`. An id already on disk is never pulled
//! twice, and a blob an earlier `add` for the same instance carried is
//! deleted once its replacement is stored. A pull that fails is logged and
//! left `pending` in the index for the next append to retry — it never drops
//! the event, because the event is the record and the blob is its payload.
//!
//! The store is deliberately dumb: it does not parse, validate or order the
//! events it is handed. What an event means is the plugin's and the merge
//! planner's business; what this module guarantees is that a line that was
//! accepted is on disk, whole, in the order it arrived.
//!
//! TWO OR MORE LIVE BRANCHES. Main's plugin keeps ONE event stream for the
//! whole session, named after the first checkpoint it took, because restarting
//! the stream at a second branch's checkpoint cut the first branch off from
//! every edit main made afterwards. Each live branch still needs its own
//! complete "main since my mark", so an append to main's daemon is fanned out:
//! the named stream, plus every OTHER checkpoint that has a mark in
//! `marks.json`. A branch daemon has no marks file, so a clone's stream is
//! written exactly as before; and an older plugin — which names the newest
//! checkpoint rather than the session's — is carried by the same fan-out with
//! nothing to change on its side.
//!
//! A mark is removed by `POST /journal/unmark` when a branch is finished or
//! purged, which is the only thing that ever stops the fan-out. Streams are
//! never deleted here: `<checkpoint>.jsonl` is one branch's view of main and
//! no other branch's, and nothing that removes one branch may touch another's.

use axum::{
	body::Bytes,
	extract::{Path as RoutePath, Query, State},
	http::{header, StatusCode},
	response::{IntoResponse, Json, Response},
};
use base64::Engine;
use log::{debug, trace, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
	collections::HashSet,
	fs::{self, OpenOptions},
	io::{ErrorKind, Write},
	path::{Path, PathBuf},
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc, Mutex,
	},
	time::Duration,
};

use crate::{constants::BRANCHES_DIR, lock, server::bulk, server::AppState};

/// Directory under [`BRANCHES_DIR`] that holds every journal
const JOURNAL_DIR: &str = "journal";

/// The checkpoint marks file inside [`JOURNAL_DIR`]
const MARKS_FILE: &str = "marks.json";

/// Where blobs land inside [`JOURNAL_DIR`]
const BLOBS_DIR: &str = "blobs";

/// The blob index inside [`JOURNAL_DIR`]: which referenced ids are on disk
const BLOBS_FILE: &str = "blobs.json";

/// How many events one `POST /journal/append` may carry. A bound, not a
/// policy: the plugin batches on a timer, and a batch this large means
/// something upstream stopped batching
const APPEND_MAX_EVENTS: usize = 4096;

/// An indexed blob whose bytes are on disk
pub const BLOB_STORED: &str = "stored";

/// An indexed blob the journal references but has not pulled yet
pub const BLOB_PENDING: &str = "pending";

/// How much of a blob one `branch_blob_read` carries. Mirrors the plugin's
/// own `Journal.BLOB_READ_MAX_LEN`, which is the ceiling it enforces
const BLOB_READ_MAX_LEN: usize = 512 * 1024;

/// How long one `branch_blob_*` op may take. A serialized subtree can be
/// large and the plugin serializes it on the main thread, so this is the
/// transfer surface's deadline rather than the 5 s remote-op default
const BLOB_OP_TIMEOUT_MS: u64 = 30_000;

/// Parts one blob may be read in before the pull is called broken. A bound on
/// a plugin that answers `eof: false` forever, not a policy
const BLOB_MAX_PARTS: usize = 4096;

/// Checkpoint ids, branch names and blob ids all become path components, so
/// they are restricted to a set with no way out of the journal directory: no
/// separators, no `..`, no dotfiles
fn safe_name(name: &str) -> bool {
	!name.is_empty()
		&& name.len() <= 128
		&& !name.starts_with('.')
		&& name
			.chars()
			.all(|char| char.is_ascii_alphanumeric() || matches!(char, '-' | '_' | '.'))
}

fn error(status: StatusCode, message: &str) -> Response {
	(status, Json(json!({ "ok": false, "error": message }))).into_response()
}

/// One branch's cut point in main's journal
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mark {
	pub checkpoint: String,
	pub branch: String,
	/// RFC 3339, when the mark was recorded
	pub at: String,
}

/// One blob an event references, and what the daemon has done about it.
///
/// `state` is [`BLOB_STORED`] once the bytes are under `blobs/`, and
/// [`BLOB_PENDING`] until then — with `error` carrying why the last pull
/// failed, so a retry is a decision someone can read rather than a silent
/// loop. `owner` is the instance id whose `add` carried the blob: a later
/// `add` for the same instance supersedes this one, which is how a refreshed
/// subtree stops the journal accumulating every intermediate serialization
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BlobRecord {
	pub id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub owner: Option<String>,
	pub state: String,
	#[serde(default)]
	pub bytes: u64,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
}

/// A blob id an event named, with the instance it belongs to
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
	pub id: String,
	pub owner: Option<String>,
}

/// Every blob id a batch of events references, in arrival order and
/// deduplicated: an `add` carries the serialized subtree, and a blob-only
/// refresh (`add` with `update: true`) carries a fresher one for the same
/// instance. Anything else with a plain-name `blob` field is taken at its
/// word — the store does not decide what an event means
pub fn blob_refs(events: &[Value]) -> Vec<BlobRef> {
	let mut refs: Vec<BlobRef> = Vec::new();

	for event in events {
		let Some(id) = event.get("blob").and_then(Value::as_str).filter(|id| safe_name(id)) else {
			continue;
		};

		if refs.iter().any(|blob| blob.id == id) {
			continue;
		}

		refs.push(BlobRef {
			id: id.to_owned(),
			owner: event.get("id").and_then(Value::as_str).map(str::to_owned),
		});
	}

	refs
}

/// The blob ids one instance's `add`s carried, oldest first, grouped by that
/// instance and in the order the instances first appear.
///
/// An instance's blobs are INTERCHANGEABLE. Each is a serialization of the
/// same subtree at a different moment — the `add` when it appeared, then a
/// refresh every `BLOB_REFRESH_SECONDS` while the branch keeps working — and
/// a replay reconstructs the subtree from whichever one it is handed. That is
/// what makes deleting a superseded blob safe: the merge asks by the id its
/// event names, and the CLI answers with the freshest serialization of the
/// same instance under that name.
///
/// An event with a blob but no instance id is its own group, which is the
/// conservative reading: nothing may stand in for it
pub fn blob_groups(events: &[Value]) -> Vec<(String, Vec<String>)> {
	let mut groups: Vec<(String, Vec<String>)> = Vec::new();

	for blob in blob_refs(events) {
		let owner = blob.owner.unwrap_or_else(|| blob.id.clone());

		match groups.iter_mut().find(|(key, _)| *key == owner) {
			Some((_, ids)) => ids.push(blob.id),
			None => groups.push((owner, vec![blob.id])),
		}
	}

	groups
}

/// Whether a merge can run from the journal on disk alone.
///
/// This is the question `wsync branch finish` asks when the branch's Studio
/// is closed: every added subtree has to have a serialization on disk,
/// because there is no plugin left to drain. An instance whose blobs are ALL
/// gone is not an `add` the merge may quietly skip — it is a merge that must
/// not start, because skipping it drops the instances that blob carries
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishDecision {
	Proceed,
	Refuse { missing: Vec<String> },
}

impl FinishDecision {
	/// The message a refusal prints, naming the branch to reopen. `None` when
	/// the decision is to proceed
	pub fn refusal(&self, branch: &str) -> Option<String> {
		let FinishDecision::Refuse { missing } = self else {
			return None;
		};

		Some(format!(
			"Branch {branch}'s Studio is closed and {} of its added subtree(s) never reached the journal on disk \
			 ({}). Reopen it with `wsync branch up {branch}` and run the finish again — the daemon pulls what the \
			 plugin still holds the moment it reconnects, and the finish serializes anything else again from the \
			 live tree. If that does not clear it, follow the wsync-branch-recovery skill. Merging without those \
			 blobs would drop the instances they carry",
			missing.len(),
			missing.join(", ")
		))
	}
}

/// [`FinishDecision`] for a journal's events against the ids actually on
/// disk. An instance is satisfied by ANY one of its serializations, so a
/// superseded id that has been cleaned up does not refuse a merge the newest
/// one can serve
pub fn finish_decision(events: &[Value], on_disk: &HashSet<String>) -> FinishDecision {
	let missing: Vec<String> = blob_groups(events)
		.into_iter()
		.filter(|(_, ids)| !ids.iter().any(|id| on_disk.contains(id)))
		.filter_map(|(_, ids)| ids.last().cloned())
		.collect();

	if missing.is_empty() {
		FinishDecision::Proceed
	} else {
		FinishDecision::Refuse { missing }
	}
}

/// The on-disk journal for one workspace. The mutex serializes writers
/// against each other; readers go straight to the file, which is safe
/// because a line is only ever appended whole
pub struct JournalStore {
	dir: PathBuf,
	write: Mutex<()>,
	/// A blob pull pass is running. Only one at a time reaches the plugin,
	/// because the plugin answers one request at a time anyway and a second
	/// pass would pull the same ids twice
	pulling: AtomicBool,
	/// Something was appended since the running pass took its list, so it
	/// goes round again rather than the append having to wait for it
	wanted: AtomicBool,
}

impl JournalStore {
	pub fn new(workspace_dir: &Path) -> Self {
		Self {
			dir: workspace_dir.join(BRANCHES_DIR).join(JOURNAL_DIR),
			write: Mutex::new(()),
			pulling: AtomicBool::new(false),
			wanted: AtomicBool::new(false),
		}
	}

	fn stream_path(&self, checkpoint: &str) -> PathBuf {
		self.dir.join(format!("{checkpoint}.jsonl"))
	}

	fn marks_path(&self) -> PathBuf {
		self.dir.join(MARKS_FILE)
	}

	fn blob_path(&self, id: &str) -> PathBuf {
		self.dir.join(BLOBS_DIR).join(id)
	}

	fn blobs_path(&self) -> PathBuf {
		self.dir.join(BLOBS_FILE)
	}

	/// Every stream one append has to reach: the one it names, then every
	/// other checkpoint that has a mark. Main's daemon holds a mark per live
	/// branch and each of those branches needs main's events from its own mark
	/// on, so one push lands in each of their streams; a branch daemon has no
	/// marks and gets the named stream alone
	fn fan_out(&self, checkpoint: &str) -> Vec<String> {
		let mut streams = vec![checkpoint.to_owned()];

		for mark in self.marks() {
			if mark.checkpoint != checkpoint && safe_name(&mark.checkpoint) && !streams.contains(&mark.checkpoint) {
				streams.push(mark.checkpoint);
			}
		}

		streams
	}

	/// Appends every event as its own line and returns `(appended, total,
	/// streams)` — `total` being the named stream's line count. One
	/// `write_all` per stream, so a batch lands whole or reports why not
	/// rather than tearing a line in half
	fn append(&self, checkpoint: &str, events: &[Value]) -> Result<(usize, usize, Vec<String>), String> {
		let _guard = lock!(self.write);

		fs::create_dir_all(&self.dir).map_err(|err| format!("Failed to create the journal directory: {err}"))?;

		let mut batch = String::new();

		for event in events {
			batch.push_str(&event.to_string());
			batch.push('\n');
		}

		let streams = self.fan_out(checkpoint);

		for stream in &streams {
			let path = self.stream_path(stream);

			let mut file = OpenOptions::new()
				.create(true)
				.append(true)
				.open(&path)
				.map_err(|err| format!("Failed to open the journal: {err}"))?;

			file.write_all(batch.as_bytes())
				.map_err(|err| format!("Failed to append to the journal: {err}"))?;
		}

		Ok((events.len(), count_lines(&self.stream_path(checkpoint)), streams))
	}

	/// Every event after `after` lines, in order. A missing stream is an
	/// empty one — asking for a checkpoint nothing was written under yet is a
	/// legitimate question whose answer is "nothing yet"
	fn read(&self, checkpoint: &str, after: usize) -> Result<Vec<Value>, String> {
		let contents = match fs::read_to_string(self.stream_path(checkpoint)) {
			Ok(contents) => contents,
			Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
			Err(err) => return Err(format!("Failed to read the journal: {err}")),
		};

		let mut events = Vec::new();

		for (index, line) in contents.lines().enumerate() {
			if index < after || line.is_empty() {
				continue;
			}

			match serde_json::from_str::<Value>(line) {
				Ok(event) => events.push(event),
				// A torn tail is the one damage an interrupted append can
				// leave; it is reported rather than served as a silent hole
				Err(err) => {
					warn!("Journal {checkpoint} line {} is not valid JSON: {err}", index + 1);

					return Err(format!("Journal {checkpoint} is damaged at line {}", index + 1));
				}
			}
		}

		Ok(events)
	}

	fn marks(&self) -> Vec<Mark> {
		let Ok(contents) = fs::read_to_string(self.marks_path()) else {
			return Vec::new();
		};

		match serde_json::from_str::<Vec<Mark>>(&contents) {
			Ok(marks) => marks,
			Err(err) => {
				warn!("Failed to read the journal marks: {err}");

				Vec::new()
			}
		}
	}

	/// Records where a branch was cut, replacing any earlier mark for the
	/// same branch: a branch has exactly one cut point at a time, and
	/// `branch up` moves it
	fn mark(&self, mark: Mark) -> Result<Vec<Mark>, String> {
		let _guard = lock!(self.write);

		fs::create_dir_all(&self.dir).map_err(|err| format!("Failed to create the journal directory: {err}"))?;

		let mut marks = self.marks();

		marks.retain(|existing| existing.branch != mark.branch);
		marks.push(mark);

		let path = self.marks_path();
		let temp = path.with_extension("tmp");

		let contents =
			serde_json::to_string_pretty(&marks).map_err(|err| format!("Failed to serialize the marks: {err}"))?;

		fs::write(&temp, contents).map_err(|err| format!("Failed to write the marks: {err}"))?;
		fs::rename(&temp, &path).map_err(|err| format!("Failed to move the marks into place: {err}"))?;

		Ok(marks)
	}

	/// Drops a branch's mark. Its stream file stays: `<checkpoint>.jsonl` is
	/// that branch's own view of main and no other branch's, so removing one
	/// branch must never take a file with it — what stops here is only the
	/// fan-out, which has nobody left to serve. Returns the marks that remain,
	/// and whether one was actually removed
	fn unmark(&self, branch: &str) -> Result<(Vec<Mark>, bool), String> {
		let _guard = lock!(self.write);

		let mut marks = self.marks();
		let before = marks.len();

		marks.retain(|existing| existing.branch != branch);

		let removed = marks.len() != before;

		if !removed {
			return Ok((marks, false));
		}

		fs::create_dir_all(&self.dir).map_err(|err| format!("Failed to create the journal directory: {err}"))?;

		let path = self.marks_path();
		let temp = path.with_extension("tmp");

		let contents =
			serde_json::to_string_pretty(&marks).map_err(|err| format!("Failed to serialize the marks: {err}"))?;

		fs::write(&temp, contents).map_err(|err| format!("Failed to write the marks: {err}"))?;
		fs::rename(&temp, &path).map_err(|err| format!("Failed to move the marks into place: {err}"))?;

		Ok((marks, true))
	}

	/// The bytes, with no index bookkeeping and no lock of its own
	fn write_blob_file(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
		let blobs = self.dir.join(BLOBS_DIR);

		fs::create_dir_all(&blobs).map_err(|err| format!("Failed to create the blob directory: {err}"))?;

		// Temp + rename, so a reader never opens a blob that is still arriving
		let path = self.blob_path(id);
		let temp = blobs.join(format!("{id}.tmp"));

		fs::write(&temp, bytes).map_err(|err| format!("Failed to write the blob: {err}"))?;
		fs::rename(&temp, &path).map_err(|err| format!("Failed to move the blob into place: {err}"))?;

		Ok(())
	}

	fn blob(&self, id: &str) -> Option<Vec<u8>> {
		fs::read(self.blob_path(id)).ok()
	}

	// The blob index

	/// What the index currently says. A missing or damaged index is an empty
	/// one: it is a record of intent, and the directory is the truth
	pub fn blob_index(&self) -> Vec<BlobRecord> {
		let Ok(contents) = fs::read_to_string(self.blobs_path()) else {
			return Vec::new();
		};

		match serde_json::from_str::<Vec<BlobRecord>>(&contents) {
			Ok(records) => records,
			Err(err) => {
				warn!("Failed to read the journal blob index: {err}");

				Vec::new()
			}
		}
	}

	fn write_blob_index(&self, records: &[BlobRecord]) -> Result<(), String> {
		fs::create_dir_all(&self.dir).map_err(|err| format!("Failed to create the journal directory: {err}"))?;

		let path = self.blobs_path();
		let temp = path.with_extension("tmp");

		let contents = serde_json::to_string_pretty(records)
			.map_err(|err| format!("Failed to serialize the blob index: {err}"))?;

		fs::write(&temp, contents).map_err(|err| format!("Failed to write the blob index: {err}"))?;
		fs::rename(&temp, &path).map_err(|err| format!("Failed to move the blob index into place: {err}"))?;

		Ok(())
	}

	/// Every blob id whose bytes are actually under `blobs/`. Read from the
	/// directory rather than from the index, so bytes that arrived some other
	/// way — a plugin that pushed them itself on unload, a `finish` that
	/// uploaded a checkpoint shape — count exactly as much as ones this
	/// daemon pulled
	pub fn stored_blobs(&self) -> HashSet<String> {
		let mut stored = HashSet::new();

		let Ok(entries) = fs::read_dir(self.dir.join(BLOBS_DIR)) else {
			return stored;
		};

		for entry in entries.flatten() {
			let name = entry.file_name().to_string_lossy().into_owned();

			if safe_name(&name) && !name.ends_with(".tmp") {
				stored.insert(name);
			}
		}

		stored
	}

	/// Records every blob a batch references and returns the ids that still
	/// have to be pulled — the ones with no bytes on disk, whether they are
	/// new or a pull that failed earlier. An id already stored is never
	/// returned, so a re-sent batch costs one directory listing and nothing
	/// else
	pub fn note_blobs(&self, refs: &[BlobRef]) -> Vec<String> {
		if refs.is_empty() {
			return self.pending_blobs();
		}

		let _guard = lock!(self.write);

		let stored = self.stored_blobs();
		let mut records = self.blob_index();
		let mut changed = false;

		for blob in refs {
			match records.iter_mut().find(|record| record.id == blob.id) {
				Some(record) => {
					if record.owner.is_none() && blob.owner.is_some() {
						record.owner.clone_from(&blob.owner);
						changed = true;
					}
				}
				None => {
					records.push(BlobRecord {
						id: blob.id.clone(),
						owner: blob.owner.clone(),
						state: if stored.contains(&blob.id) {
							BLOB_STORED
						} else {
							BLOB_PENDING
						}
						.to_owned(),
						bytes: 0,
						error: None,
					});

					changed = true;
				}
			}
		}

		if changed {
			if let Err(message) = self.write_blob_index(&records) {
				warn!("{message}");
			}
		}

		records
			.into_iter()
			.filter(|record| !stored.contains(&record.id))
			.map(|record| record.id)
			.collect()
	}

	/// Indexed ids with no bytes on disk, oldest first
	pub fn pending_blobs(&self) -> Vec<String> {
		self.pending_blob_records()
			.into_iter()
			.map(|record| record.id)
			.collect()
	}

	/// The same, with each record's owner: a pull that finds the plugin no
	/// longer holds an id can ask for the OWNER to be serialized again
	pub fn pending_blob_records(&self) -> Vec<BlobRecord> {
		let stored = self.stored_blobs();

		self.blob_index()
			.into_iter()
			.filter(|record| !stored.contains(&record.id))
			.collect()
	}

	/// What `POST /journal/pull` answers: every indexed blob with its state,
	/// and the ids still owed. `stored` is counted from the directory, so a
	/// blob that arrived by push rather than pull counts the same
	pub fn pull_report(&self) -> Value {
		let stored = self.stored_blobs();
		let records = self.blob_index();

		let pending: Vec<Value> = records
			.iter()
			.filter(|record| !stored.contains(&record.id))
			.map(|record| {
				json!({
					"id": record.id,
					"owner": record.owner,
					"error": record.error,
				})
			})
			.collect();

		json!({
			"ok": true,
			"indexed": records.len(),
			"stored": records.iter().filter(|record| stored.contains(&record.id)).count(),
			"blobsOnDisk": stored.len(),
			"pending": pending,
		})
	}

	/// A blob's bytes have arrived. Marks it stored and drops every blob an
	/// EARLIER `add` for the same instance carried: those are intermediate
	/// serializations of a subtree that has since been re-captured, and the
	/// replay only ever reads the last one. Returns what was superseded
	pub fn store_blob(&self, id: &str, bytes: &[u8]) -> Result<Vec<String>, String> {
		let _guard = lock!(self.write);

		self.write_blob_file(id, bytes)?;

		let mut records = self.blob_index();

		let Some(at) = records.iter().position(|record| record.id == id) else {
			// A blob nothing in the journal names — a checkpoint shape, pushed
			// here so a clone can fetch it. Stored, not indexed
			return Ok(Vec::new());
		};

		records[at].state = BLOB_STORED.to_owned();
		records[at].bytes = bytes.len() as u64;
		records[at].error = None;

		let owner = records[at].owner.clone();
		let mut superseded = Vec::new();

		if let Some(owner) = owner {
			for record in records.iter().take(at) {
				if record.owner.as_deref() == Some(owner.as_str()) && record.id != id {
					superseded.push(record.id.clone());
				}
			}
		}

		for stale in &superseded {
			if let Err(err) = fs::remove_file(self.blob_path(stale)) {
				if err.kind() != ErrorKind::NotFound {
					warn!("Failed to drop the superseded journal blob {stale}: {err}");
				}
			}
		}

		records.retain(|record| !superseded.contains(&record.id));

		self.write_blob_index(&records)?;

		Ok(superseded)
	}

	/// A pull that did not land. The event keeps its blob id and the index
	/// keeps the reason, so the next append retries it instead of the failure
	/// being a thing only the log remembers
	pub fn blob_failed(&self, id: &str, reason: &str) {
		let _guard = lock!(self.write);

		let mut records = self.blob_index();

		let Some(record) = records.iter_mut().find(|record| record.id == id) else {
			return;
		};

		record.state = BLOB_PENDING.to_owned();
		record.error = Some(reason.to_owned());

		if let Err(message) = self.write_blob_index(&records) {
			warn!("{message}");
		}
	}
}

/// Lines currently in a stream. Counted from the file rather than tracked in
/// memory so the number survives a daemon restart and always matches what a
/// reader paging with `after` will see
fn count_lines(path: &Path) -> usize {
	match fs::read(path) {
		Ok(bytes) => bytes.iter().filter(|byte| **byte == b'\n').count(),
		Err(err) => {
			warn!("Failed to count journal lines: {err}");

			0
		}
	}
}

// ---------------------------------------------------------------------------
// The blob pull
// ---------------------------------------------------------------------------

/// Drains every blob the journal still owes out of the connected plugin.
///
/// Spawned by `POST /journal/append` AFTER it has answered, because a flush
/// that waited for eleven serialized subtrees to cross the wire would stall
/// the recorder that is trying to hand over its events. One pass runs at a
/// time — the plugin answers one request at a time anyway — and an append
/// that lands mid-pass sets `wanted` so the pass goes round again instead of
/// queueing a second one.
///
/// Nothing here can fail an append. A plugin that is gone, a blob that has
/// expired, a disk that will not take the bytes: each is logged and left
/// `pending` in the index, and the next append tries again
pub async fn pump_blobs(state: AppState) {
	let store = state.journal.clone();

	store.wanted.store(true, Ordering::SeqCst);

	if store.pulling.swap(true, Ordering::SeqCst) {
		return;
	}

	loop {
		// Whatever way this pass ends — a panic in a pull, the task dropped
		// with the runtime — the flag comes back down. It used to be cleared
		// only by the last line of a pass that ran to completion, and a pass
		// that did not left every later append believing a pull was still
		// running: no blob was pulled again for the life of the daemon, and
		// every one the plugin held expired
		let pulling = PullingGuard(store.clone());

		pull_pass(&state).await;

		drop(pulling);

		// An append that landed between the pass's last `wanted` check and
		// the guard dropping saw `pulling` still set and went away expecting
		// that pass to pick its ids up. Nobody would have: go round again
		// for it, unless a newer pass has already taken the flag
		if !store.wanted.load(Ordering::SeqCst) || store.pulling.swap(true, Ordering::SeqCst) {
			return;
		}
	}
}

/// One pass over everything owed, repeated while appends keep arriving
async fn pull_pass(state: &AppState) {
	let store = state.journal.clone();

	while store.wanted.swap(false, Ordering::SeqCst) {
		let pending = store.pending_blob_records();

		if pending.is_empty() {
			continue;
		}

		debug!("Journal blobs: pulling {} blob(s) out of the plugin", pending.len());

		for record in pending {
			let id = record.id;

			let pulled = match pull_blob(state, &id).await {
				Ok(bytes) => Ok(bytes),
				Err(message) if should_recapture(&message) && record.owner.is_some() => {
					// The plugin no longer holds these bytes — its pinned copy
					// idled past the TTL while this daemon was down, wedged or
					// restarted — but the instance they describe is still in
					// its tree. Any serialization of an instance serves for
					// any other, so it is captured again and stored under the
					// id the event names
					let owner = record.owner.as_deref().unwrap_or_default();

					debug!("Journal blob {id}: {message}; asking the plugin to serialize {owner} again");

					recapture_blob(state, owner)
						.await
						.map_err(|why| format!("{message}; recapture: {why}"))
				}
				Err(message) => Err(message),
			};

			match pulled {
				Ok(bytes) => {
					let stored = bytes.len();

					match store.store_blob(&id, &bytes) {
						Ok(superseded) => {
							debug!(
								"Journal blob {id}: stored {stored} byte(s) from the plugin{}",
								if superseded.is_empty() {
									String::new()
								} else {
									format!(", superseding {}", superseded.join(", "))
								}
							);

							// The bytes are safe on disk, so the plugin does
							// not have to keep holding them: this is the same
							// drain `branch finish` used to do at merge time,
							// only seconds after the event instead of days
							drop(
								bulk::plugin_request(
									state,
									"branch_blob_close",
									json!({ "artifactId": id }),
									BLOB_OP_TIMEOUT_MS,
								)
								.await,
							);
						}
						Err(message) => {
							warn!("Journal blob {id}: {message}");
							store.blob_failed(&id, &message);
						}
					}
				}
				Err(message) => {
					debug!("Journal blob {id} could not be pulled yet: {message}");
					store.blob_failed(&id, &message);

					// A plugin that is not there will not be there for the
					// next id either, and a pass that asks anyway spends one
					// timeout per blob
					if message.contains("no Studio plugin connected") {
						break;
					}
				}
			}
		}
	}
}

/// Clears the store's `pulling` flag however the pass that took it ends
struct PullingGuard(Arc<JournalStore>);

impl Drop for PullingGuard {
	fn drop(&mut self) {
		self.0.pulling.store(false, Ordering::SeqCst);
	}
}

/// Whether a failed pull is one the plugin can answer by serializing the
/// instance again: the id is gone from its store (closed, expired or never
/// captured — `branch_blob_open`'s NOT_FOUND wording), as opposed to a
/// plugin that is absent, a timeout, or bytes that arrived damaged
pub fn should_recapture(message: &str) -> bool {
	let lower = message.to_ascii_lowercase();

	lower.contains("expired") || lower.contains("never captured") || lower.contains("no blob ")
}

/// `journal_recapture {ids: [owner]}` and a pull of the fresh id it answers
/// with. The fresh pin is closed once the bytes are in hand; a plugin too old
/// to know the op answers an error, which is reported like any other
async fn recapture_blob(state: &AppState, owner: &str) -> Result<Vec<u8>, String> {
	let answer = bulk::plugin_request(
		state,
		"journal_recapture",
		json!({ "ids": [owner] }),
		BLOB_OP_TIMEOUT_MS,
	)
	.await
	.map_err(|error| error.to_string())?;

	let fresh = answer
		.get("blobs")
		.and_then(|blobs| blobs.get(owner))
		.and_then(|blob| blob.get("artifactId"))
		.and_then(Value::as_str)
		.map(str::to_owned);

	let Some(fresh) = fresh else {
		return Err(format!("the plugin no longer has {owner} in its tree"));
	};

	let pulled = pull_blob(state, &fresh).await;

	drop(
		bulk::plugin_request(
			state,
			"branch_blob_close",
			json!({ "artifactId": fresh }),
			BLOB_OP_TIMEOUT_MS,
		)
		.await,
	);

	pulled
}

// POST /journal/pull

/// How long `POST /journal/pull` waits for a pass that is already running
/// before it answers that it could not run one of its own
const PULL_WAIT: Duration = Duration::from_secs(120);

/// `POST /journal/pull` — one blob pull pass, run to completion, and the
/// index afterwards. This is the pull `wsync branch repair` asks for: every
/// other pass is a side effect of an append, and a daemon that came back
/// after being down has no append to trigger one until the plugin next has
/// something to say. Only a branch daemon keeps blobs (see `append`)
pub async fn pull(State(state): State<AppState>) -> Response {
	trace!("Received request: journal pull");

	if state.core.project().branch.is_none() {
		return error(
			StatusCode::BAD_REQUEST,
			"Only a branch daemon keeps journal blobs; this project names no branch",
		);
	}

	let store = state.journal.clone();
	let deadline = tokio::time::Instant::now() + PULL_WAIT;

	// A pass already running is somebody else's; wait it out rather than
	// answer with an index it is about to change
	while store.pulling.load(Ordering::SeqCst) {
		if tokio::time::Instant::now() >= deadline {
			return error(
				StatusCode::SERVICE_UNAVAILABLE,
				"A blob pull pass has been running for over two minutes; try again once it finishes",
			);
		}

		tokio::time::sleep(Duration::from_millis(200)).await;
	}

	pump_blobs(state.clone()).await;

	Json(store.pull_report()).into_response()
}

/// One blob, over the ops the CLI's own pull uses: `branch_blob_open` for the
/// size and digest, `branch_blob_read` in bounded base64 parts, and the
/// digest checked before anything is called stored
async fn pull_blob(state: &AppState, id: &str) -> Result<Vec<u8>, String> {
	let opened = bulk::plugin_request(
		state,
		"branch_blob_open",
		json!({ "artifactId": id }),
		BLOB_OP_TIMEOUT_MS,
	)
	.await
	.map_err(|error| error.to_string())?;

	let total = opened.get("bytes").and_then(Value::as_u64).unwrap_or_default() as usize;
	let expected = opened.get("sha256").and_then(Value::as_str).map(str::to_owned);

	let mut bytes: Vec<u8> = Vec::with_capacity(total);

	for _ in 0..BLOB_MAX_PARTS {
		let part = bulk::plugin_request(
			state,
			"branch_blob_read",
			json!({ "artifactId": id, "offset": bytes.len(), "maxLen": BLOB_READ_MAX_LEN }),
			BLOB_OP_TIMEOUT_MS,
		)
		.await
		.map_err(|error| error.to_string())?;

		let data = part.get("data").and_then(Value::as_str).unwrap_or_default();
		let eof = part.get("eof").and_then(Value::as_bool).unwrap_or(false);

		if !data.is_empty() {
			let decoded = base64::engine::general_purpose::STANDARD
				.decode(data)
				.map_err(|err| format!("the plugin answered branch_blob_read with undecodable base64: {err}"))?;

			bytes.extend_from_slice(&decoded);
		} else if !eof {
			return Err("the plugin answered branch_blob_read with no data and no eof".to_owned());
		}

		if eof {
			if total > 0 && bytes.len() != total {
				return Err(format!("the plugin sent {} of {total} byte(s)", bytes.len()));
			}

			if let Some(expected) = expected {
				let actual = hex_digest(&bytes);

				if !expected.eq_ignore_ascii_case(&actual) {
					return Err(format!("sha256 is {actual}, the plugin named {expected}"));
				}
			}

			return Ok(bytes);
		}
	}

	Err(format!(
		"the plugin did not finish the blob within {BLOB_MAX_PARTS} part(s)"
	))
}

fn hex_digest(bytes: &[u8]) -> String {
	let mut hasher = Sha256::new();

	hasher.update(bytes);

	hasher
		.finalize()
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect::<String>()
}

// POST /journal/append

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AppendRequest {
	checkpoint: String,
	events: Vec<Value>,
}

/// `POST /journal/append` — the plugin's batched event push. Events are
/// stored verbatim, one JSON line each; what they mean is the merge planner's
/// business, not this store's
pub async fn append(State(state): State<AppState>, Json(request): Json<AppendRequest>) -> Response {
	trace!("Received request: journal append");

	if !safe_name(&request.checkpoint) {
		return error(StatusCode::BAD_REQUEST, "checkpoint must be a plain name");
	}

	if request.events.len() > APPEND_MAX_EVENTS {
		return error(
			StatusCode::BAD_REQUEST,
			&format!("An append is bounded at {APPEND_MAX_EVENTS} events"),
		);
	}

	match state.journal.append(&request.checkpoint, &request.events) {
		Ok((appended, total, streams)) => {
			debug!(
				"Journal {}: appended {appended} event(s) to {} stream(s), {total} total",
				request.checkpoint,
				streams.len()
			);

			// The events are on disk; their payloads are not. Index what this
			// batch references, then answer — and only then go and drain the
			// bytes, so the plugin's flush is never held up by a transfer it
			// is not waiting for.
			//
			// Only a BRANCH daemon keeps blobs. Main's stream exists for a
			// branch's conflict detection, which reads main's *events*; its
			// `add` payloads are never replayed anywhere, so pulling them
			// would be main's daemon writing a serialized subtree to disk for
			// every instance anyone adds, forever
			let owed = if state.core.project().branch.is_some() {
				let owed = state.journal.note_blobs(&blob_refs(&request.events));

				if !owed.is_empty() {
					tokio::spawn(pump_blobs(state.clone()));
				}

				owed
			} else {
				Vec::new()
			};

			Json(json!({
				"ok": true,
				"checkpoint": request.checkpoint,
				"appended": appended,
				"total": total,
				"streams": streams,
				"blobsPending": owed,
			}))
			.into_response()
		}
		Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
	}
}

// GET /journal

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ReadParams {
	checkpoint: Option<String>,
	after: Option<usize>,
}

/// `GET /journal?checkpoint=<c>&after=<seq>` — every event after sequence
/// `seq`, where an event's sequence is its 1-based line number in the stream
pub async fn read(State(state): State<AppState>, Query(params): Query<ReadParams>) -> Response {
	trace!("Received request: journal read");

	let Some(checkpoint) = params.checkpoint.filter(|checkpoint| safe_name(checkpoint)) else {
		return error(StatusCode::BAD_REQUEST, "checkpoint must be a plain name");
	};

	let after = params.after.unwrap_or(0);

	match state.journal.read(&checkpoint, after) {
		Ok(events) => {
			// Which of the events' payloads a merge can read from here
			// without a Studio. `blobsOnDisk` is what `branch finish` checks
			// before it starts on a closed branch; `blobsMissing` is what it
			// would have to reopen that branch for. Both are facts about the
			// directory, not about intent — main's daemon keeps no blobs at
			// all (see `append`), so on main everything an `add` names is
			// missing and nothing reads these lists
			let stored = state.journal.stored_blobs();
			let mut on_disk = Vec::new();
			let mut missing = Vec::new();

			for blob in blob_refs(&events) {
				if stored.contains(&blob.id) {
					on_disk.push(blob.id);
				} else {
					missing.push(blob.id);
				}
			}

			Json(json!({
				"ok": true,
				"checkpoint": checkpoint,
				"after": after,
				"events": events,
				"blobsOnDisk": on_disk,
				"blobsMissing": missing,
			}))
			.into_response()
		}
		Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
	}
}

// POST /journal/mark

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MarkRequest {
	checkpoint: String,
	branch: String,
}

/// `POST /journal/mark` — main's daemon records which checkpoint a branch was
/// cut at. That branch's merge reads main's own events after this mark
pub async fn mark(State(state): State<AppState>, Json(request): Json<MarkRequest>) -> Response {
	trace!("Received request: journal mark");

	if !safe_name(&request.checkpoint) {
		return error(StatusCode::BAD_REQUEST, "checkpoint must be a plain name");
	}

	if !safe_name(&request.branch) {
		return error(StatusCode::BAD_REQUEST, "branch must be a plain name");
	}

	let mark = Mark {
		checkpoint: request.checkpoint,
		branch: request.branch,
		at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
	};

	match state.journal.mark(mark.clone()) {
		Ok(_) => {
			debug!(
				"Journal mark: branch {} cut at checkpoint {}",
				mark.branch, mark.checkpoint
			);

			Json(json!({ "ok": true, "mark": mark })).into_response()
		}
		Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
	}
}

// POST /journal/unmark

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UnmarkRequest {
	branch: String,
}

/// `POST /journal/unmark` — a branch that has been merged or purged stops
/// being one of the streams an append fans out to. Nothing is deleted: the
/// branch's own `<checkpoint>.jsonl` is not another branch's to remove, and a
/// mark that is not there any more is not an error
pub async fn unmark(State(state): State<AppState>, Json(request): Json<UnmarkRequest>) -> Response {
	trace!("Received request: journal unmark");

	if !safe_name(&request.branch) {
		return error(StatusCode::BAD_REQUEST, "branch must be a plain name");
	}

	match state.journal.unmark(&request.branch) {
		Ok((marks, removed)) => {
			debug!(
				"Journal unmark: branch {} {}, {} mark(s) left",
				request.branch,
				if removed { "dropped" } else { "was not marked" },
				marks.len()
			);

			Json(json!({ "ok": true, "branch": request.branch, "removed": removed, "marks": marks })).into_response()
		}
		Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
	}
}

/// `GET /journal/marks` — every branch's cut point
pub async fn marks(State(state): State<AppState>) -> Response {
	trace!("Received request: journal marks");

	Json(json!({ "ok": true, "marks": state.journal.marks() })).into_response()
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PutBlobParams {
	/// `base64` when the body is base64 text rather than the bytes
	encoding: Option<String>,
}

/// `POST /journal/blob/:id` — the raw body becomes the blob.
///
/// Blobs are the serialized subtrees an `add` event refers to. They are kept
/// here, beside the journal that names them, rather than pulled from the
/// plugin that made them on demand: by the time a merge reads a blob the
/// Studio that produced it is usually closed, and the existing clipboard
/// upload path serves bytes *out of a live plugin*, which is exactly the
/// thing a merge cannot rely on.
///
/// `?encoding=base64` takes the same bytes as base64 text, which is how the
/// PLUGIN pushes: `HttpService:RequestAsync` sends a Lua string and a
/// serialized place is not text. It is the plugin's own last word on the way
/// out (`plugin.Unloading`), racing the window's close against a pull this
/// daemon would otherwise have to make — whichever arrives first wins, and
/// storing the same bytes twice is storing them once
pub async fn put_blob(
	State(state): State<AppState>,
	RoutePath(id): RoutePath<String>,
	Query(params): Query<PutBlobParams>,
	body: Bytes,
) -> Response {
	trace!("Received request: journal blob (put)");

	if !safe_name(&id) {
		return error(StatusCode::BAD_REQUEST, "id must be a plain name");
	}

	let bytes = match params.encoding.as_deref() {
		None | Some("") | Some("binary") => body.to_vec(),
		Some("base64") => {
			let text: Vec<u8> = body
				.iter()
				.copied()
				.filter(|byte| !byte.is_ascii_whitespace())
				.collect();

			match base64::engine::general_purpose::STANDARD.decode(&text) {
				Ok(bytes) => bytes,
				Err(err) => return error(StatusCode::BAD_REQUEST, &format!("body is not base64: {err}")),
			}
		}
		Some(other) => {
			return error(
				StatusCode::BAD_REQUEST,
				&format!("encoding must be `base64` or absent, not `{other}`"),
			)
		}
	};

	match state.journal.store_blob(&id, &bytes) {
		Ok(superseded) => {
			debug!("Journal blob {id}: stored {} byte(s)", bytes.len());

			Json(json!({ "ok": true, "id": id, "bytes": bytes.len(), "superseded": superseded })).into_response()
		}
		Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
	}
}

/// `GET /journal/blob/:id` — the stored bytes, verbatim
pub async fn get_blob(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
	trace!("Received request: journal blob (get)");

	if !safe_name(&id) {
		return error(StatusCode::BAD_REQUEST, "id must be a plain name");
	}

	match state.journal.blob(&id) {
		Some(bytes) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
		None => error(StatusCode::NOT_FOUND, "Unknown blob id"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn store() -> (tempfile::TempDir, JournalStore) {
		let dir = tempfile::tempdir().expect("a temp workspace");
		let store = JournalStore::new(dir.path());

		(dir, store)
	}

	fn mark_of(checkpoint: &str, branch: &str) -> Mark {
		Mark {
			checkpoint: checkpoint.to_owned(),
			branch: branch.to_owned(),
			at: "2026-09-18T00:00:00Z".to_owned(),
		}
	}

	fn events(count: usize) -> Vec<Value> {
		(0..count).map(|n| json!({ "seq": n, "op": "set" })).collect()
	}

	/// No marks — a branch daemon — writes the named stream and nothing else
	#[test]
	fn an_unmarked_append_writes_one_stream() {
		let (_dir, store) = store();

		let (appended, total, streams) = store.append("alpha", &events(3)).expect("the append lands");

		assert_eq!(appended, 3);
		assert_eq!(total, 3);
		assert_eq!(streams, vec!["alpha".to_owned()]);
	}

	/// Main with two live branches: one push, both branches' streams
	#[test]
	fn an_append_fans_out_to_every_marked_checkpoint() {
		let (_dir, store) = store();

		store.mark(mark_of("alpha", "alpha")).expect("alpha is marked");
		store.mark(mark_of("beta", "beta")).expect("beta is marked");

		// The plugin names its session stream, which is alpha's checkpoint
		let (_, _, streams) = store.append("alpha", &events(2)).expect("the append lands");

		assert_eq!(streams, vec!["alpha".to_owned(), "beta".to_owned()]);
		assert_eq!(store.read("alpha", 0).expect("alpha reads").len(), 2);
		assert_eq!(store.read("beta", 0).expect("beta reads").len(), 2);
	}

	/// An older plugin names the NEWEST checkpoint rather than the session's.
	/// The fan-out carries the older branch either way, and nothing is written
	/// twice
	#[test]
	fn an_append_naming_a_marked_checkpoint_is_not_written_twice() {
		let (_dir, store) = store();

		store.mark(mark_of("alpha", "alpha")).expect("alpha is marked");
		store.mark(mark_of("beta", "beta")).expect("beta is marked");

		let (_, total, streams) = store.append("beta", &events(4)).expect("the append lands");

		assert_eq!(total, 4);
		assert_eq!(streams, vec!["beta".to_owned(), "alpha".to_owned()]);
		assert_eq!(store.read("beta", 0).expect("beta reads").len(), 4);
		assert_eq!(store.read("alpha", 0).expect("alpha reads").len(), 4);
	}

	/// Finishing or purging one branch stops its fan-out and leaves every
	/// other branch's stream exactly where it was
	#[test]
	fn unmark_drops_one_branch_and_keeps_the_others_stream() {
		let (_dir, store) = store();

		store.mark(mark_of("alpha", "alpha")).expect("alpha is marked");
		store.mark(mark_of("beta", "beta")).expect("beta is marked");
		store.append("alpha", &events(2)).expect("the first append lands");

		let (left, removed) = store.unmark("alpha").expect("alpha is unmarked");

		assert!(removed);
		assert_eq!(left.len(), 1);
		assert_eq!(left[0].branch, "beta");

		// alpha's own stream is untouched by its unmark
		assert_eq!(store.read("alpha", 0).expect("alpha reads").len(), 2);

		let (_, _, streams) = store.append("beta", &events(1)).expect("the second append lands");

		assert_eq!(streams, vec!["beta".to_owned()]);
		assert_eq!(store.read("beta", 0).expect("beta reads").len(), 3);
		assert_eq!(store.read("alpha", 0).expect("alpha reads").len(), 2);
	}

	/// A branch that was never marked, or was unmarked already, is not an error
	#[test]
	fn unmark_is_idempotent() {
		let (_dir, store) = store();

		store.mark(mark_of("alpha", "alpha")).expect("alpha is marked");

		assert!(store.unmark("alpha").expect("the first unmark").1);
		assert!(!store.unmark("alpha").expect("the second unmark").1);
		assert!(!store.unmark("never").expect("an unknown branch").1);
	}

	/// A mark whose checkpoint would escape the journal directory is not a
	/// stream anything appends to
	#[test]
	fn a_mark_with_an_unsafe_checkpoint_is_not_fanned_out_to() {
		let (_dir, store) = store();

		store.mark(mark_of("alpha", "alpha")).expect("alpha is marked");

		// Written straight into the file: `POST /journal/mark` would refuse it
		let marks = vec![mark_of("alpha", "alpha"), mark_of("../escape", "bad")];
		fs::create_dir_all(&store.dir).expect("the journal directory");
		fs::write(
			store.marks_path(),
			serde_json::to_string(&marks).expect("marks serialize"),
		)
		.expect("the marks are written");

		let (_, _, streams) = store.append("session", &events(1)).expect("the append lands");

		assert_eq!(streams, vec!["session".to_owned(), "alpha".to_owned()]);
	}

	// The blob index

	fn add(id: &str, blob: &str) -> Value {
		json!({ "id": id, "op": "add", "blob": blob })
	}

	fn on_disk(ids: &[&str]) -> HashSet<String> {
		ids.iter().map(|id| (*id).to_owned()).collect()
	}

	/// An `add` names a blob nobody has yet, so the index owes it and the
	/// append reports it as the thing the pull pass has to go and fetch
	#[test]
	fn a_referenced_blob_starts_out_pending() {
		let (_dir, store) = store();

		let owed = store.note_blobs(&blob_refs(&[add("new:1", "aa"), add("new:2", "bb")]));

		assert_eq!(owed, vec!["aa".to_owned(), "bb".to_owned()]);

		let index = store.blob_index();

		assert_eq!(index.len(), 2);
		assert!(index.iter().all(|record| record.state == BLOB_PENDING));
		assert_eq!(index[0].owner.as_deref(), Some("new:1"));
	}

	/// Once the bytes are on disk the id is stored, is not owed any more, and
	/// is not pulled a second time however often its event is re-appended
	#[test]
	fn a_stored_blob_is_not_owed_again() {
		let (_dir, store) = store();

		store.note_blobs(&blob_refs(&[add("new:1", "aa")]));
		store.store_blob("aa", b"rbxm").expect("the blob is stored");

		let record = store
			.blob_index()
			.into_iter()
			.find(|record| record.id == "aa")
			.expect("aa is indexed");

		assert_eq!(record.state, BLOB_STORED);
		assert_eq!(record.bytes, 4);
		assert_eq!(store.blob("aa").as_deref(), Some(&b"rbxm"[..]));

		assert!(store.pending_blobs().is_empty());
		assert!(store.note_blobs(&blob_refs(&[add("new:1", "aa")])).is_empty());
	}

	/// A refreshed subtree is a second blob for the same instance. Storing it
	/// drops the first one's bytes and its index entry — the replay only ever
	/// reads the last `add` — and leaves another instance's blob alone
	#[test]
	fn a_newer_blob_supersedes_the_one_its_instance_carried_before() {
		let (_dir, store) = store();

		store.note_blobs(&blob_refs(&[add("new:1", "aa"), add("new:2", "cc")]));
		store.store_blob("aa", b"first").expect("aa is stored");
		store.store_blob("cc", b"other").expect("cc is stored");

		store.note_blobs(&blob_refs(&[add("new:1", "bb")]));

		let superseded = store.store_blob("bb", b"second").expect("bb is stored");

		assert_eq!(superseded, vec!["aa".to_owned()]);
		assert!(store.blob("aa").is_none());
		assert_eq!(store.blob("bb").as_deref(), Some(&b"second"[..]));
		assert_eq!(store.blob("cc").as_deref(), Some(&b"other"[..]));

		let ids: Vec<String> = store.blob_index().into_iter().map(|record| record.id).collect();

		assert_eq!(ids, vec!["cc".to_owned(), "bb".to_owned()]);
	}

	/// A pull that failed stays owed, with the reason on it, so the next
	/// append retries rather than the event quietly losing its payload
	#[test]
	fn a_failed_pull_stays_pending_and_is_retried() {
		let (_dir, store) = store();

		store.note_blobs(&blob_refs(&[add("new:1", "aa")]));
		store.blob_failed("aa", "no Studio plugin connected");

		let record = store
			.blob_index()
			.into_iter()
			.find(|record| record.id == "aa")
			.expect("aa is indexed");

		assert_eq!(record.state, BLOB_PENDING);
		assert_eq!(record.error.as_deref(), Some("no Studio plugin connected"));
		assert_eq!(store.pending_blobs(), vec!["aa".to_owned()]);
		assert_eq!(store.note_blobs(&[]), vec!["aa".to_owned()]);
	}

	/// Bytes that arrive by the other route — the plugin pushing them itself
	/// on unload — count exactly as much as ones this daemon pulled
	#[test]
	fn a_pushed_blob_settles_the_index_it_was_never_pulled_for() {
		let (_dir, store) = store();

		store.note_blobs(&blob_refs(&[add("new:1", "aa")]));
		store.store_blob("aa", b"pushed").expect("aa is stored");

		assert!(store.stored_blobs().contains("aa"));
		assert!(store.pending_blobs().is_empty());
	}

	/// Every serialization of one instance is one group; an event with a blob
	/// but no instance id stands alone, because nothing may stand in for it
	#[test]
	fn a_re_serialized_instance_groups_its_blobs_together() {
		let events = vec![
			add("new:1", "aa"),
			add("new:2", "cc"),
			add("new:1", "bb"),
			json!({ "op": "add", "blob": "dd" }),
		];

		assert_eq!(
			blob_groups(&events),
			vec![
				("new:1".to_owned(), vec!["aa".to_owned(), "bb".to_owned()]),
				("new:2".to_owned(), vec!["cc".to_owned()]),
				("dd".to_owned(), vec!["dd".to_owned()]),
			]
		);
	}

	/// A blob id that would escape the journal directory is not a blob
	#[test]
	fn an_unsafe_blob_id_is_not_referenced() {
		assert!(blob_refs(&[json!({ "id": "new:1", "op": "add", "blob": "../escape" })]).is_empty());
		assert!(blob_refs(&[json!({ "id": "new:1", "op": "add", "blob": "" })]).is_empty());
		assert!(blob_refs(&[json!({ "id": "new:1", "op": "set" })]).is_empty());
	}

	// The finish decision

	/// Every payload is on disk, so the merge runs without the branch's
	/// Studio — which is the whole point
	#[test]
	fn finish_proceeds_when_every_blob_is_on_disk() {
		let events = vec![
			add("new:1", "aa"),
			json!({ "id": "7", "op": "set" }),
			add("new:2", "bb"),
		];

		let decision = finish_decision(&events, &on_disk(&["aa", "bb", "unrelated"]));

		assert_eq!(decision, FinishDecision::Proceed);
		assert_eq!(decision.refusal("alpha"), None);
	}

	/// One payload missing is not an `add` the merge may skip: it is a merge
	/// that must not start, and the refusal has to say which branch to reopen
	#[test]
	fn finish_refuses_when_one_blob_is_missing() {
		let events = vec![add("new:1", "aa"), add("new:2", "bb")];

		let decision = finish_decision(&events, &on_disk(&["aa"]));

		assert_eq!(
			decision,
			FinishDecision::Refuse {
				missing: vec!["bb".to_owned()]
			}
		);

		let refusal = decision.refusal("alpha").expect("a refusal names the branch");

		assert!(refusal.contains("alpha"), "{refusal}");
		assert!(refusal.contains("wsync branch up alpha"), "{refusal}");
		assert!(refusal.contains("bb"), "{refusal}");
	}

	/// A journal with no `add` at all merges from disk with nothing to fetch
	#[test]
	fn finish_proceeds_on_a_journal_with_no_blobs() {
		let events = events(3);

		assert_eq!(finish_decision(&events, &HashSet::new()), FinishDecision::Proceed);
	}

	/// The same blob named twice is one blob, and one missing id is reported
	/// once rather than per event
	#[test]
	fn a_repeated_blob_id_is_reported_once() {
		let events = vec![add("new:1", "aa"), add("new:1", "aa")];

		assert_eq!(
			finish_decision(&events, &HashSet::new()),
			FinishDecision::Refuse {
				missing: vec!["aa".to_owned()]
			}
		);
	}

	/// A re-serialized instance keeps only its newest blob on disk, and its
	/// older `add` still names the one that was deleted. That must not refuse
	/// a merge: the CLI ships the survivor's bytes under both ids, so the
	/// instance is served either way
	#[test]
	fn a_superseded_id_does_not_refuse_a_merge_the_newest_blob_can_serve() {
		let events = vec![add("new:1", "aa"), add("new:1", "bb")];

		assert_eq!(finish_decision(&events, &on_disk(&["bb"])), FinishDecision::Proceed);
	}

	/// An instance whose serializations are ALL gone is named by its newest
	/// id, which is the one worth looking for
	#[test]
	fn an_instance_with_no_serialization_left_refuses_by_its_newest_id() {
		let events = vec![add("new:1", "aa"), add("new:1", "bb"), add("new:2", "cc")];

		assert_eq!(
			finish_decision(&events, &on_disk(&["cc"])),
			FinishDecision::Refuse {
				missing: vec!["bb".to_owned()]
			}
		);
	}

	/// The refusal names the reopen — the one remedy that needs no manual
	/// work — and the skill for everything past it
	#[test]
	fn a_refusal_names_the_reopen_and_the_recovery_skill() {
		let decision = finish_decision(&[add("new:1", "aa")], &HashSet::new());
		let refusal = decision.refusal("alpha").expect("a refusal");

		assert!(refusal.contains("wsync branch up alpha"), "{refusal}");
		assert!(refusal.contains("wsync-branch-recovery"), "{refusal}");
	}

	/// Only the plugin saying it no longer HOLDS the id is worth a second
	/// serialization. An absent plugin, a timeout or a bad digest are not
	#[test]
	fn only_a_blob_the_plugin_has_lost_is_recaptured() {
		assert!(should_recapture(
			"no blob 0f3a (closed, expired, or never captured) — re-run branch_checkpoint [NOT_FOUND]"
		));
		assert!(should_recapture("blob 0f3a is closed, expired, or was never captured"));

		assert!(!should_recapture("no Studio plugin connected"));
		assert!(!should_recapture(
			"No plugin response to branch_blob_read within 30000 ms"
		));
		assert!(!should_recapture("sha256 is aa, the plugin named bb"));
		assert!(!should_recapture("the plugin sent 10 of 20 byte(s)"));
	}

	/// The pull report separates what is on disk from what is still owed,
	/// and carries the owner and last failure of each owed id
	#[test]
	fn the_pull_report_lists_what_is_owed_with_its_owner() {
		let (_dir, store) = store();

		store.note_blobs(&blob_refs(&[add("new:1", "aa"), add("new:2", "bb")]));
		store.store_blob("aa", b"bytes").expect("aa is stored");
		store.blob_failed("bb", "no blob bb (closed, expired, or never captured)");

		let report = store.pull_report();

		assert_eq!(report["ok"], json!(true));
		assert_eq!(report["indexed"], json!(2));
		assert_eq!(report["stored"], json!(1));
		assert_eq!(report["blobsOnDisk"], json!(1));
		assert_eq!(report["pending"].as_array().map(Vec::len), Some(1));
		assert_eq!(report["pending"][0]["id"], json!("bb"));
		assert_eq!(report["pending"][0]["owner"], json!("new:2"));
		assert!(report["pending"][0]["error"]
			.as_str()
			.is_some_and(|error| error.contains("expired")));
	}

	/// Pending records carry their owner, which is what a recapture asks for
	#[test]
	fn pending_records_name_their_owner() {
		let (_dir, store) = store();

		store.note_blobs(&blob_refs(&[add("new:7", "aa")]));

		let pending = store.pending_blob_records();

		assert_eq!(pending.len(), 1);
		assert_eq!(pending[0].id, "aa");
		assert_eq!(pending[0].owner.as_deref(), Some("new:7"));
	}

	/// The guard is what clears `pulling`, so a pass that unwinds early
	/// leaves the store able to start another
	#[test]
	fn the_pulling_guard_releases_the_pass_when_dropped() {
		let (_dir, store) = store();
		let store = Arc::new(store);

		assert!(!store.pulling.swap(true, Ordering::SeqCst));

		{
			let _guard = PullingGuard(store.clone());
		}

		assert!(!store.pulling.load(Ordering::SeqCst));
	}
}
