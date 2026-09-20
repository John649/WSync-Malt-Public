//! `wsync launch` — open a place in Roblox Studio from either of the two
//! things a place can be: a file on this disk, or a place id in the cloud.
//!
//! The two launches already existed apart — `wsync studio <file>` for a
//! file, `wsync open <placeId>` for a cloud place plus the scratch project
//! and daemon it attaches — and a caller with "a place" in hand had to know
//! which command its kind wanted. This one takes either and launches only:
//! no project is created and no daemon started, so it is the right call
//! from a script that already runs its own daemon, and the wrong one when
//! the point is for the place to be addressable afterwards (that is `open`).
//!
//! An all-digit argument is a place id; anything else is a path. A path
//! that does not exist is tried with `.rbxl` and `.rbxlx` appended when
//! smart paths are on, the same way `wsync studio` resolves one.

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::json;
use std::path::PathBuf;

use crate::{
	cli::{client::print_json, places},
	config::Config,
	ext::PathExt,
	project::{self, Project},
	studio, wsync_info,
};

/// Launch a place in Roblox Studio from a place file or a place id
#[derive(Parser)]
pub struct Launch {
	/// A place file (`.rbxl` / `.rbxlx`) or an all-digit place id
	#[arg(value_name = "PLACE")]
	place: String,

	/// The universe (game id) that owns the place. Needed for a place id
	/// nothing on this machine already claims
	#[arg(long, value_name = "GAME_ID")]
	universe: Option<u64>,

	/// A project whose `gameId` supplies the universe for a place id
	#[arg(long, value_name = "PATH")]
	project: Option<PathBuf>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// What the argument named
#[derive(Debug, PartialEq, Eq)]
enum Target {
	PlaceId(u64),
	File(PathBuf),
}

/// An all-digit argument is a place id; anything else is a path. The two
/// never collide: no file is named by digits alone
fn classify(place: &str) -> Target {
	match place.parse::<u64>() {
		Ok(id) if !place.is_empty() && place.bytes().all(|byte| byte.is_ascii_digit()) => Target::PlaceId(id),
		_ => Target::File(PathBuf::from(place)),
	}
}

/// The path as given, or with `.rbxl` / `.rbxlx` appended when the given
/// one does not exist and smart paths are on
fn resolve_file(path: PathBuf, smart_paths: bool) -> PathBuf {
	if path.exists() || !smart_paths {
		return path;
	}

	for extension in ["rbxl", "rbxlx"] {
		let candidate = path.with_file_name(format!("{}.{extension}", path.get_name()));

		if candidate.exists() {
			return candidate;
		}
	}

	path
}

impl Launch {
	pub fn main(self) -> Result<()> {
		match classify(&self.place) {
			Target::File(path) => self.launch_file(path),
			Target::PlaceId(place_id) => self.launch_place(place_id),
		}
	}

	fn launch_file(self, path: PathBuf) -> Result<()> {
		let path = resolve_file(path, Config::new().smart_paths);

		if !path.is_file() {
			bail!(
				"No place file at {} — pass a `.rbxl` / `.rbxlx` path, or an all-digit place id",
				path.to_string().bold()
			);
		}

		let path = path
			.resolve()
			.with_context(|| format!("Failed to resolve {}", path.to_string()))?;

		let pid = studio::launch(Some(path.clone())).context("Failed to launch Roblox Studio")?;

		if self.raw {
			print_json(&json!({ "ok": true, "launched": true, "pid": pid, "file": path.to_string() }));
		} else {
			wsync_info!("Launching Roblox Studio on {}", path.to_string().bold());
		}

		Ok(())
	}

	fn launch_place(self, place_id: u64) -> Result<()> {
		let project = match &self.project {
			Some(path) => {
				let path = project::resolve(path.clone())?;

				Some(Project::load(&path).with_context(|| format!("Failed to load the project {}", path.to_string()))?)
			}
			None => None,
		};

		// A daemon that already claims the place knows its universe; a
		// project's `gameId` is the other place it is written down
		let claimed = places::registry()
			.ok()
			.map(|entries| places::candidates(&entries))
			.and_then(|candidates| {
				places::resolve(&candidates, &places::Selector::Id(place_id), None)
					.ok()
					.and_then(|candidate| candidate.game_id)
			});

		let universe = self
			.universe
			.or(claimed)
			.or_else(|| project.as_ref().and_then(|project| project.game_id));

		let Some(universe) = universe else {
			bail!(
				"Studio needs the universe that owns place {place_id}. Pass `{}`, or `{}` for a project whose \
				 `gameId` is set",
				"--universe <gameId>".bold(),
				"--project <path>".bold()
			);
		};

		let url = studio::edit_place_url(place_id, universe);
		let pid = studio::launch_url(&url).context("Failed to launch Roblox Studio")?;

		if self.raw {
			print_json(&json!({
				"ok": true,
				"launched": true,
				"pid": pid,
				"placeId": place_id,
				"universeId": universe,
				"url": url,
			}));
		} else {
			wsync_info!(
				"Launching Roblox Studio on place {} (universe {})",
				place_id.to_string().bold(),
				universe.to_string().bold()
			);
		}

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn digits_are_a_place_id_and_anything_else_is_a_file() {
		assert_eq!(classify("137417550355737"), Target::PlaceId(137_417_550_355_737));
		assert_eq!(classify("game.rbxl"), Target::File(PathBuf::from("game.rbxl")));
		assert_eq!(classify("./build/1234"), Target::File(PathBuf::from("./build/1234")));
		assert_eq!(classify("1234.rbxl"), Target::File(PathBuf::from("1234.rbxl")));
		assert_eq!(classify(""), Target::File(PathBuf::from("")));
	}

	#[test]
	fn a_missing_path_is_tried_with_a_place_extension() {
		let dir = tempfile::tempdir().expect("a temp dir");
		let file = dir.path().join("game.rbxlx");
		std::fs::write(&file, b"").expect("a place file");

		let bare = dir.path().join("game");

		assert_eq!(resolve_file(bare.clone(), true), file);
		assert_eq!(resolve_file(bare.clone(), false), bare);
	}

	#[test]
	fn an_existing_path_is_taken_as_given() {
		let dir = tempfile::tempdir().expect("a temp dir");
		let file = dir.path().join("game.rbxl");
		std::fs::write(&file, b"").expect("a place file");

		assert_eq!(resolve_file(file.clone(), true), file);
	}

	#[test]
	fn the_command_parses_both_forms() {
		let by_id = Launch::try_parse_from(["launch", "1234567890", "--universe", "42"]).unwrap();
		assert_eq!(by_id.universe, Some(42));
		assert_eq!(classify(&by_id.place), Target::PlaceId(1_234_567_890));

		let by_file = Launch::try_parse_from(["launch", "./build/game.rbxl", "--raw"]).unwrap();
		assert!(by_file.raw);
		assert_eq!(
			classify(&by_file.place),
			Target::File(PathBuf::from("./build/game.rbxl"))
		);
	}
}
