//! `wsync open` — two jobs behind one verb (docs/places.md).
//!
//! Given Studio paths it selects them in the connected place, which is what
//! this command has always done. Given a **place id** it launches Studio on
//! that cloud place through the `roblox-studio:` protocol URL, so a place
//! that exists nowhere on this disk can still be opened, and — unless told
//! not to — attaches a throwaway project and daemon to it so the rest of the
//! CLI can address it immediately.
//!
//! The two forms cannot be confused: a Studio path always starts with a
//! service name, and a place id is digits only.
//!
//! The attach project is scratch by definition. It is created under
//! `<projectsRoot>/_places/<placeId>`, it reconciles nothing on connect
//! (`initialSync: "keep"`), and its daemon is started `--ephemeral`: once
//! Studio disconnects, the daemon stops itself and this directory is
//! removed. `--keep` opts out of that; `--into <dir>` puts the project
//! somewhere of your choosing, which is never auto-deleted.

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::json;
use std::path::PathBuf;

use crate::{
	cli::{
		client::{print_json, Targeting},
		live, places, spawn,
	},
	ext::PathExt,
	project::{self, Project},
	studio, wsync_info,
};

/// Open a place in Studio by id, or select Studio paths by path
#[derive(Parser)]
pub struct Open {
	/// Place id to launch Studio on, or Studio path(s) to select
	#[arg(value_name = "PLACE-ID | STUDIO-PATH")]
	targets: Vec<String>,

	#[command(flatten)]
	targeting: Targeting,

	/// Universe (game) id the place belongs to; taken from the serving
	/// daemon or the project file when omitted
	#[arg(long, value_name = "GAME-ID")]
	universe: Option<u64>,

	/// Create the attach project here instead of under the scratch root
	#[arg(long, value_name = "DIR")]
	into: Option<PathBuf>,

	/// Launch Studio only: create no project and start no daemon
	#[arg(long)]
	no_attach: bool,

	/// Keep the scratch attach project after Studio closes
	#[arg(long)]
	keep: bool,

	/// Write the attach project and nothing else: no Studio launch, no
	/// daemon. The desktop app's `POST /places/attach` calls this so the
	/// scratch project file has exactly one implementation (docs/places.md)
	#[arg(long, hide = true, requires = "into")]
	attach_only: bool,

	/// The place's published title, for the scratch project's name (the
	/// folder stays keyed by id); the desktop app passes what Studio reports
	#[arg(long, hide = true)]
	name: Option<String>,

	/// The `placeKey` an attach project claims, for a place with no ids
	/// (docs/places.md — "Unpublished places")
	#[arg(long, hide = true, value_name = "KEY")]
	place_key: Option<String>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// A place id is the only all-digit thing this command takes; every Studio
/// path starts with a service name
fn as_place_id(target: &str) -> Option<u64> {
	if target.is_empty() || !target.chars().all(|character| character.is_ascii_digit()) {
		return None;
	}

	target.parse().ok()
}

impl Open {
	pub fn main(self) -> Result<()> {
		let single_place = match self.targets.as_slice() {
			[only] => as_place_id(only),
			_ => None,
		};

		if self.attach_only {
			if single_place.is_none() && !self.targets.is_empty() {
				bail!("--attach-only takes a place id or nothing at all, never a Studio path");
			}

			return self.write_attach_project(single_place);
		}

		if single_place.is_none() && !self.targets.is_empty() {
			return self.select();
		}

		self.open_place(single_place)
	}

	/// `--attach-only`: create the scratch project for a place and report
	/// where it landed. The caller (the desktop app's attach route) starts
	/// the daemon itself through its own lifecycle, which is why nothing is
	/// launched or spawned here
	fn write_attach_project(self, place_id: Option<u64>) -> Result<()> {
		let workspace = self.into.clone().expect("--attach-only requires --into");

		std::fs::create_dir_all(&workspace)
			.with_context(|| format!("Failed to create the attach project directory {}", workspace.to_string()))?;

		let project = places::write_scratch_project(&workspace, place_id, self.universe, self.place_key.as_deref(), self.name.as_deref())?;
		// Whether the auto-delete rules would own this directory. The caller
		// decides on `--ephemeral` from this, rather than guessing at the
		// scratch roots from outside
		let scratch = places::is_scratch_dir(&workspace);

		print_json(&json!({
			"ok": true,
			"project": project.to_string(),
			"workspace": workspace.to_string(),
			"name": places::scratch_project_name(place_id, self.place_key.as_deref()),
			"placeId": place_id,
			"placeKey": self.place_key,
			"gameId": self.universe,
			"scratch": scratch,
		}));

		Ok(())
	}

	/// The original form: select instances in the connected place
	fn select(self) -> Result<()> {
		for (flag, given) in [
			("--universe", self.universe.is_some()),
			("--into", self.into.is_some()),
			("--no-attach", self.no_attach),
			("--keep", self.keep),
			("--attach-only", self.attach_only),
			("--place-key", self.place_key.is_some()),
		] {
			if given {
				bail!("{flag} belongs to `wsync open <placeId>`, not to selecting Studio paths");
			}
		}

		live::Open::select(self.targeting, self.targets, self.raw)
	}

	fn open_place(self, place_id: Option<u64>) -> Result<()> {
		// `--project` is both a way to name the place (its first
		// servePlaceIds entry) and the fallback source for the universe
		let project = match &self.targeting.project {
			Some(path) => {
				let path = project::resolve(path.clone())?;

				Some(Project::load(&path).with_context(|| format!("Failed to load the project {}", path.to_string()))?)
			}
			None => None,
		};

		let (selected_place, selected_branch) = match places::selector() {
			Some((places::Selector::Id(place), branch)) => (Some(place), branch),
			// A key names a place that exists only as somebody's open file;
			// there is no cloud place for Studio to be launched on
			Some((places::Selector::Key(key), _)) => bail!(
				"`--place {key}` names an unpublished place, which `{}` cannot launch. Open it in Studio yourself",
				"wsync open".bold()
			),
			None => (None, None),
		};

		let place_id = match place_id.or(selected_place) {
			Some(place_id) => place_id,
			None => match project.as_ref().and_then(|project| project.place_ids.first().copied()) {
				Some(place_id) => place_id,
				None => bail!(
					"Name the place to open: `{}`, or `{}` for a project whose `servePlaceIds` has one",
					"wsync open <placeId>".bold(),
					"wsync open --project <path>".bold()
				),
			},
		};

		let entries = places::registry()?;
		let candidates = places::candidates(&entries);
		// A place nothing serves is the normal case here, not a failure
		let serving = places::resolve(&candidates, &places::Selector::Id(place_id), selected_branch.as_deref())
			.ok()
			.cloned();

		let universe = self
			.universe
			.or_else(|| serving.as_ref().and_then(|candidate| candidate.game_id))
			.or_else(|| project.as_ref().and_then(|project| project.game_id));

		let Some(universe) = universe else {
			bail!(
				"Studio needs the universe that owns place {place_id}. Pass `{}`, or run this against a project \
				 whose `gameId` is set",
				format!("--universe <gameId>").bold()
			);
		};

		// Attach first, so the daemon is already listening when Studio comes
		// up and the plugin's discovery scan finds it on the first pass
		let attached = match &serving {
			Some(_) => None,
			None if self.no_attach => None,
			None => Some(self.attach(place_id, universe)?),
		};

		let url = studio::edit_place_url(place_id, universe);
		let pid = studio::launch_url(&url).context("Failed to launch Roblox Studio")?;

		if self.raw {
			print_json(&json!({
				"ok": true,
				"placeId": place_id,
				"universeId": universe,
				"studioPid": pid,
				"url": url,
				"daemon": serving.as_ref().map(|candidate| json!({
					"project": candidate.project,
					"port": candidate.port,
					"branch": candidate.branch,
				})),
				"attached": attached.as_ref().map(|attached| json!({
					"project": attached.project.to_string(),
					"port": attached.port,
					"pid": attached.pid,
					"ephemeral": attached.ephemeral,
				})),
			}));

			return Ok(());
		}

		wsync_info!(
			"Opening place {} (universe {}) in Studio",
			place_id.to_string().bold(),
			universe.to_string().bold()
		);

		match (&serving, &attached) {
			(Some(candidate), _) => println!(
				"  served by {} on port {}{}",
				candidate.project.bold(),
				candidate.port.to_string().bold(),
				match &candidate.branch {
					Some(branch) => format!(" (branch {branch})"),
					None => String::new(),
				}
			),
			(None, Some(attached)) => {
				println!(
					"  attached scratch project {} on port {}",
					attached.project.to_string().bold(),
					attached.port.to_string().bold()
				);
				println!(
					"  {}",
					if attached.ephemeral {
						"it disappears on its own when Studio closes".to_owned()
					} else {
						format!("delete {} to forget it", attached.workspace.to_string())
					}
				);
			}
			(None, None) => println!(
				"  no daemon serves this place; start one with {}",
				"wsync daemon start --project <path>".bold()
			),
		}

		Ok(())
	}

	/// Creates (or reuses) the scratch project for this place and starts its
	/// daemon detached
	fn attach(&self, place_id: u64, universe: u64) -> Result<Attached> {
		let workspace = match &self.into {
			Some(into) => into.clone(),
			None => places::scratch_root()?.join(place_id.to_string()),
		};

		std::fs::create_dir_all(&workspace).with_context(|| {
			format!(
				"Failed to create the attach project directory {}",
				workspace.to_string()
			)
		})?;

		let project = places::write_scratch_project(&workspace, Some(place_id), Some(universe), None, None)?;

		// Only a directory this command owns by name may be auto-deleted, so
		// `--into` is always a keep
		let ephemeral = !self.keep && places::is_scratch_dir(&workspace);
		let extra: &[&str] = if ephemeral { &["--ephemeral"] } else { &[] };
		let spawned = spawn::daemon(&project, extra)?;

		Ok(Attached {
			workspace,
			project,
			port: spawned.port,
			pid: spawned.pid,
			ephemeral,
		})
	}
}

struct Attached {
	workspace: PathBuf,
	project: PathBuf,
	port: u16,
	pid: u32,
	ephemeral: bool,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn digits_are_a_place_id_and_paths_are_not() {
		assert_eq!(as_place_id("1234567"), Some(1_234_567));
		assert_eq!(as_place_id("Workspace"), None);
		assert_eq!(as_place_id("ReplicatedStorage/Client/UI"), None);
		assert_eq!(as_place_id(""), None);
		assert_eq!(as_place_id("12a"), None);
	}

	#[test]
	fn attach_only_writes_the_project_and_starts_nothing() {
		let dir = tempfile::tempdir().unwrap();
		let command = Open::try_parse_from([
			"open",
			"--attach-only",
			"--into",
			dir.path().to_str().unwrap(),
			"--place-key",
			"my-level-ab12cd34",
		])
		.unwrap();

		command.main().unwrap();

		let project = Project::load(&dir.path().join("default.project.json")).unwrap();

		assert_eq!(project.place_key.as_deref(), Some("my-level-ab12cd34"));
		assert!(project.place_ids.is_empty());
	}

	#[test]
	fn attach_only_needs_somewhere_to_write() {
		let error = match Open::try_parse_from(["open", "--attach-only"]) {
			Ok(_) => panic!("--attach-only without --into should not parse"),
			Err(error) => error.to_string(),
		};

		assert!(error.contains("--into"), "{error}");
	}

	#[test]
	fn selecting_paths_refuses_the_place_flags() {
		let command = Open::try_parse_from(["open", "Workspace/Baseplate", "--universe", "9"]).unwrap();
		let error = command.main().unwrap_err().to_string();

		assert!(error.contains("--universe belongs to"), "{error}");
	}

	#[test]
	fn a_place_id_parses_as_one_target() {
		let command = Open::try_parse_from(["open", "1234567", "--universe", "9"]).unwrap();

		assert_eq!(command.targets, vec![String::from("1234567")]);
		assert_eq!(command.universe, Some(9));
	}
}
