use axum::{extract::State, Json};
use log::trace;
use serde::Serialize;
use std::{fs, process};

use crate::{constants::PROTOCOL_VERSION, ext::PathExt, server::AppState};

/// JSON identity for daemon discovery (Design §5.2): the Studio plugin and
/// the desktop app port-scan `GET /hello` to find the daemon that serves the
/// project matching the open place
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
	name: String,
	version: &'static str,
	/// Short commit hash the binary was built from (`unknown` outside git)
	commit: &'static str,
	/// Whether the build tree carried uncommitted changes
	dirty: bool,
	protocol: u8,
	project: String,
	canonical_project: String,
	/// Sync scope (Design §7.0): `"code"` or `"full"`; absent on older
	/// daemons, which clients read as full
	scope: &'static str,
	/// The branch this daemon serves (docs/branches.md), when its project
	/// file names one; absent on main and on every non-branch project
	#[serde(skip_serializing_if = "Option::is_none")]
	branch: Option<String>,
	game_id: Option<u64>,
	place_ids: Vec<u64>,
	/// The project's `placeKey` (docs/places.md — "Unpublished places"): the
	/// name a place with no ids is addressed by. Absent on every project that
	/// does not set one, which is most of them
	#[serde(skip_serializing_if = "Option::is_none")]
	place_key: Option<String>,
	/// A scratch place daemon that deletes itself and its project once Studio
	/// goes away (`wsync open` attach mode, and the desktop app's
	/// `POST /places/attach`). The plugin reads it to say the connection is
	/// temporary; `wsync places` reads its own runtime record instead
	ephemeral: bool,
	boot_id: String,
	pid: u32,
	port: u16,
	managed_by: String,
	project_init: bool,
	/// Resolved mutation audit log path, so `wsync status` can report
	/// write-log health (Design §6.4/§12)
	writes_log: String,
	/// The Studio plugin currently owning the live bridge, when one is
	/// connected. `wsync places` reads it to tell a daemon waiting for
	/// Studio from one already driving it, without spending a plugin op
	#[serde(skip_serializing_if = "Option::is_none")]
	plugin: Option<PluginIdentity>,
}

/// The connected plugin as `/hello` reports it
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginIdentity {
	name: String,
	transport: &'static str,
}

pub async fn main(State(state): State<AppState>) -> Json<Hello> {
	trace!("Received request: hello");

	let (name, project_path, game_id, place_ids, place_key, scope, branch) = {
		let project = state.core.project();

		(
			project.name.clone(),
			project.path.clone(),
			project.game_id,
			project.place_ids.clone(),
			project.place_key.clone(),
			project.scope(),
			project.branch.clone(),
		)
	};

	let canonical_project = fs::canonicalize(&project_path)
		.map(|path| path.to_string())
		.unwrap_or_else(|_| project_path.to_string());

	Json(Hello {
		name,
		// `/hello` and the WS hello always report the real version; only
		// msgpack `GET /details` may report the Argon compat constant
		version: env!("CARGO_PKG_VERSION"),
		commit: env!("WSYNC_BUILD_COMMIT"),
		dirty: env!("WSYNC_BUILD_DIRTY") == "true",
		protocol: PROTOCOL_VERSION,
		project: project_path.to_string(),
		canonical_project,
		scope: scope.as_str(),
		branch,
		game_id,
		place_ids,
		place_key,
		ephemeral: state.identity.ephemeral,
		boot_id: state.identity.boot_id.clone(),
		pid: process::id(),
		port: state.identity.port,
		managed_by: state.identity.managed_by.clone(),
		// TODO(phase-6): Studio-authorized project creation (the desktop
		// broker flow) is not available before the desktop app lands
		project_init: false,
		writes_log: state.audit.path().to_string(),
		plugin: state
			.ws
			.plugin_status()
			.map(|(name, transport)| PluginIdentity { name, transport }),
	})
}
