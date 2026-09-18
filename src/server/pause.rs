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

/// `POST /sync/resume` — release the hold and rescan. The rescan runs
/// whether or not this call is the one that unpaused: a caller asking to
/// resume wants the tree to match disk, and answering "it was already
/// running" without checking would be exactly the lie this endpoint exists
/// to prevent
pub async fn resume(State(state): State<AppState>) -> Json<Value> {
	let changed = state.core.set_sync_paused(false);

	let rescanned = state.core.rescan();

	info!("Sync resumed: the rescan queued {rescanned} change(s) for Studio");

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
