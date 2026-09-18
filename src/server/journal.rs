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

	/// Appends every event as its own line and returns `(appended, total)`.
	/// One `write_all` per batch, so a batch lands whole or reports why not
	/// rather than tearing a line in half
	fn append(&self, checkpoint: &str, events: &[Value]) -> Result<(usize, usize), String> {
		let _guard = lock!(self.write);

		fs::create_dir_all(&self.dir).map_err(|err| format!("Failed to create the journal directory: {err}"))?;

		let path = self.stream_path(checkpoint);
		let mut batch = String::new();

		for event in events {
			batch.push_str(&event.to_string());
			batch.push('\n');
		}

		let mut file = OpenOptions::new()
			.create(true)
			.append(true)
			.open(&path)
			.map_err(|err| format!("Failed to open the journal: {err}"))?;

		file.write_all(batch.as_bytes())
			.map_err(|err| format!("Failed to append to the journal: {err}"))?;

		Ok((events.len(), count_lines(&path)))
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
		Ok((appended, total)) => {
			debug!(
				"Journal {}: appended {appended} event(s), {total} total",
				request.checkpoint
			);

			Json(json!({
				"ok": true,
				"checkpoint": request.checkpoint,
				"appended": appended,
				"total": total,
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
