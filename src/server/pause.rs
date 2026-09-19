//! `POST /sync/pause` · `POST /sync/resume` — holding the disk → Studio
//! direction of the sync engine (docs/branches.md).
//!
//! `branch finish` fast-forwards main's checkout to the branch tip in the
//! same second that main's Studio takes the instance replay. Those are two
//! halves of one merge, and the watcher must not treat the first as a stream
//! of edits to push while the second is landing — the two would race for the
//! same instances, and whichever lost would be reported as a conflict against
//! work nobody did.
//!
//! So a pause drops disk events instead of buffering them: what the tree
//! believed about disk is stale by definition once someone moves a whole
//! checkout under it, and a buffered replay would just be that stale belief
//! arriving late. A resume re-derives the tree from what is on disk *now* and
//! queues the difference, which is the same thing a connect does.

use axum::{extract::State, response::Json};
use log::info;
use serde_json::{json, Value};

use crate::server::AppState;

/// `POST /sync/pause` — hold disk → Studio pushes. Idempotent: pausing an
/// already-paused daemon is a no-op that answers `changed: false`, so a
/// retried finish never has to know whether its first attempt landed
pub async fn pause(State(state): State<AppState>) -> Json<Value> {
	let changed = state.core.set_sync_paused(true);

	if changed {
		info!("Sync paused: disk changes are no longer pushed to Studio");
	}

	Json(json!({ "ok": true, "paused": true, "changed": changed }))
}

/// `POST /sync/resume` — release the hold and rescan.
///
/// The rescan runs when this call is the one that unpaused, because that is
/// the window in which disk events were dropped and the tree may be stale. A
/// resume on a daemon that was never paused has nothing to catch up on: the
/// watcher has been delivering every event all along, and re-walking the
/// whole project there only costs a full walk — and, for a retried `branch
/// finish`, does it twice
pub async fn resume(State(state): State<AppState>) -> Json<Value> {
	let changed = state.core.set_sync_paused(false);

	let rescanned = if changed { state.core.rescan() } else { 0 };

	if changed {
		info!("Sync resumed: the rescan queued {rescanned} change(s) for Studio");
	} else {
		info!("Sync was already running; nothing to rescan");
	}

	Json(json!({
		"ok": true,
		"paused": false,
		"changed": changed,
		"rescanned": rescanned,
	}))
}

/// `GET /sync` — whether the daemon is currently holding disk pushes
pub async fn status(State(state): State<AppState>) -> Json<Value> {
	Json(json!({ "ok": true, "paused": state.core.is_sync_paused() }))
}
