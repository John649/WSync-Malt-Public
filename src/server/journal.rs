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
use log::{debug, trace, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
	fs::{self, OpenOptions},
	io::{ErrorKind, Write},
	path::{Path, PathBuf},
	sync::Mutex,
};

use crate::{constants::BRANCHES_DIR, lock, server::AppState};

/// Directory under [`BRANCHES_DIR`] that holds every journal
const JOURNAL_DIR: &str = "journal";

/// The checkpoint marks file inside [`JOURNAL_DIR`]
const MARKS_FILE: &str = "marks.json";

/// Where blobs land inside [`JOURNAL_DIR`]
const BLOBS_DIR: &str = "blobs";

/// How many events one `POST /journal/append` may carry. A bound, not a
/// policy: the plugin batches on a timer, and a batch this large means
/// something upstream stopped batching
const APPEND_MAX_EVENTS: usize = 4096;

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

/// The on-disk journal for one workspace. The mutex serializes writers
/// against each other; readers go straight to the file, which is safe
/// because a line is only ever appended whole
pub struct JournalStore {
	dir: PathBuf,
	write: Mutex<()>,
}

impl JournalStore {
	pub fn new(workspace_dir: &Path) -> Self {
		Self {
			dir: workspace_dir.join(BRANCHES_DIR).join(JOURNAL_DIR),
			write: Mutex::new(()),
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

	fn put_blob(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
		let _guard = lock!(self.write);

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

			Json(json!({
				"ok": true,
				"checkpoint": request.checkpoint,
				"appended": appended,
				"total": total,
				"streams": streams,
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
		Ok(events) => Json(json!({
			"ok": true,
			"checkpoint": checkpoint,
			"after": after,
			"events": events,
		}))
		.into_response(),
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

/// `POST /journal/blob/:id` — the raw body becomes the blob.
///
/// Blobs are the serialized subtrees an `add` event refers to. They are kept
/// here, beside the journal that names them, rather than pulled from the
/// plugin that made them on demand: by the time a merge reads a blob the
/// Studio that produced it is usually closed, and the existing clipboard
/// upload path serves bytes *out of a live plugin*, which is exactly the
/// thing a merge cannot rely on
pub async fn put_blob(State(state): State<AppState>, RoutePath(id): RoutePath<String>, body: Bytes) -> Response {
	trace!("Received request: journal blob (put)");

	if !safe_name(&id) {
		return error(StatusCode::BAD_REQUEST, "id must be a plain name");
	}

	match state.journal.put_blob(&id, &body) {
		Ok(()) => {
			debug!("Journal blob {id}: stored {} byte(s)", body.len());

			Json(json!({ "ok": true, "id": id, "bytes": body.len() })).into_response()
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
}
