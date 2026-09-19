use std::path::PathBuf;
use anyhow::Result;
use colored::Colorize;
use log::{trace, warn};
use rbx_dom_weak::{
	types::{Enum, Variant},
	ustr,
};
use serde::{Deserialize, Serialize};
use std::{
	fmt::{self, Display, Formatter},
	path::Path,
};

use self::data::DataSnapshot;
use crate::{
	constants::BLACKLISTED_PATHS,
	core::{
		meta::{Context, Source},
		snapshot::Snapshot,
	},
	ext::{PathExt, ResultExt},
	vfs::Vfs,
	wsync_warn, Properties,
};

mod helpers;

pub mod csv;
pub mod data;
pub mod dir;
pub mod json;
pub mod json_model;
pub mod luau;
pub mod md;
pub mod msgpack;
pub mod project;
pub mod rbxm;
pub mod rbxmx;
pub mod toml;
pub mod txt;
pub mod yaml;

/// The script middlewares encode class *and* RunContext in the file suffix —
/// one suffix, one meaning, no mode flag (the Argon fork's `legacyScripts`
/// flag multiplexed `.server`/`.client` onto two meanings each, which made
/// mixed-RunContext places impossible to round-trip):
///
///   `Name.luau`            ModuleScript
///   `Name.server.luau`     Script, RunContext = Legacy (the classic script)
///   `Name.client.luau`     Script, RunContext = Client
///   `Name.local.luau`      LocalScript
///   `Name.runserver.luau`  Script, RunContext = Server
///
/// RunContext = Plugin has no suffix; it falls back to `.server.luau` with
/// the RunContext preserved in the instance-data sidecar.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Middleware {
	Project,
	InstanceData,

	ServerScript,
	ClientScript,
	LocalScript,
	RunServerScript,
	ModuleScript,

	StringValue,
	RichStringValue,
	LocalizationTable,

	JsonModule,
	TomlModule,
	YamlModule,
	MsgpackModule,

	JsonModel,
	RbxmModel,
	RbxmxModel,
}

impl Display for Middleware {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		write!(f, "{self:?}")
	}
}

impl Middleware {
	fn read(&self, path: &Path, context: &Context, vfs: &Vfs) -> Result<Snapshot> {
		match self {
			Middleware::Project => project::read_project(path, Some(context), vfs),
			Middleware::InstanceData => unreachable!(),
			//
			Middleware::ServerScript
			| Middleware::ClientScript
			| Middleware::LocalScript
			| Middleware::RunServerScript
			| Middleware::ModuleScript => luau::read_luau(path, vfs, self.clone().into()),
			//
			Middleware::StringValue => txt::read_txt(path, vfs),
			Middleware::RichStringValue => md::read_md(path, vfs),
			Middleware::LocalizationTable => csv::read_csv(path, vfs),
			//
			Middleware::JsonModule => json::read_json(path, vfs),
			Middleware::TomlModule => toml::read_toml(path, vfs),
			Middleware::YamlModule => yaml::read_yaml(path, vfs),
			Middleware::MsgpackModule => msgpack::read_msgpack(path, vfs),
			//
			Middleware::JsonModel => json_model::read_json_model(path, vfs),
			Middleware::RbxmModel => rbxm::read_rbxm(path, vfs),
			Middleware::RbxmxModel => rbxmx::read_rbxmx(path, vfs),
		}
		.with_desc(|| {
			format!(
				"Failed to read {} at {}",
				self.to_string().bold(),
				path.display().to_string().bold()
			)
		})
	}

	pub fn write(&self, properties: Properties, path: &Path, vfs: &Vfs) -> Result<Properties> {
		match self {
			Middleware::ServerScript
			| Middleware::ClientScript
			| Middleware::LocalScript
			| Middleware::RunServerScript
			| Middleware::ModuleScript => luau::write_luau(properties, path, vfs),
			Middleware::StringValue => txt::write_txt(properties, path, vfs),
			Middleware::LocalizationTable => csv::write_csv(properties, path, vfs),
			// TODO: Add support for other middleware
			_ => unimplemented!(),
		}
		.with_desc(|| {
			format!(
				"Failed to write {} at {}",
				self.to_string().bold(),
				path.display().to_string().bold()
			)
		})
	}

	/// The middleware that names a file for this class — for `Script`, the
	/// RunContext is consumed from `properties` because the suffix encodes it
	/// (see the scheme on the enum). Callers that cannot supply properties get
	/// the Legacy `.server` fallback.
	pub fn from_class(class: &str, properties: Option<&mut Properties>) -> Option<Self> {
		// TODO: Implement matcher for detecting remaining middleware
		match class {
			"Script" => {
				if let Some(properties) = properties {
					if let Some(Variant::Enum(run_context)) = properties.remove(&ustr("RunContext")) {
						let run_context = run_context.to_u32();

						return Some(match run_context {
							1 => Middleware::RunServerScript,
							2 => Middleware::ClientScript,
							// Legacy (0) is the `.server` suffix itself
							0 => Middleware::ServerScript,
							_ => {
								// RunContext.Plugin (or a future value) has no
								// suffix: keep it as a property so the data
								// sidecar preserves it across the round trip
								properties.insert(ustr("RunContext"), Variant::Enum(Enum::from_u32(run_context)));

								Middleware::ServerScript
							}
						});
					}
				}

				Some(Middleware::ServerScript)
			}
			"LocalScript" => Some(Middleware::LocalScript),
			"ModuleScript" => Some(Middleware::ModuleScript),
			"StringValue" => Some(Middleware::StringValue),
			"LocalizationTable" => Some(Middleware::LocalizationTable),
			_ => None,
		}
	}
}

/// Whether a resolved sync rule may produce an instance under this context's
/// scope (Design §7.0). Code scope reads project files, `.luau` sources and
/// the instance-data sidecars that identify them — every other file is
/// outside the projection: ignored, never deleted. Full scope admits
/// everything
fn scope_allows_rule(context: &Context, middleware: &Middleware, path: &Path) -> bool {
	if !context.scope().is_code() {
		return true;
	}

	match middleware {
		Middleware::Project => true,
		Middleware::ServerScript
		| Middleware::ClientScript
		| Middleware::LocalScript
		| Middleware::RunServerScript
		| Middleware::ModuleScript => path.extension().is_some_and(|extension| extension == "luau"),
		_ => false,
	}
}

/// Returns a snapshot of the given path, `None` if path no longer exists
pub fn new_snapshot(path: &Path, context: &Context, vfs: &Vfs) -> Result<Option<Snapshot>> {
	if BLACKLISTED_PATHS.iter().any(|blacklisted| path.ends_with(blacklisted))
		|| context.ignore_rules().iter().any(|rule| rule.matches(path))
	{
		trace!("Snapshot of {} not created: ignored or blacklisted", path.display());
		return Ok(None);
	}

	if !vfs.exists(path) {
		trace!("Snapshot of {} not created: path does not exist", path.display());

		vfs.unwatch(path)?;

		return Ok(None);
	}

	trace!("Creating snapshot of {}", path.display());

	if vfs.is_file(path) {
		if let Some(snapshot) = new_snapshot_file_child(path, context, vfs)? {
			Ok(Some(snapshot))
		} else if let Some(snapshot) = new_snapshot_file(path, context, vfs)? {
			Ok(Some(snapshot))
		} else {
			trace!("Snapshot of {} not created: no middleware matched", path.display());
			Ok(None)
		}
	} else {
		// A directory has exactly one child script, and which one must not
		// depend on enumeration order: the OS hands entries back in whatever
		// order it likes and the staged in-memory tree in another, so a
		// directory holding both a legacy `init.server.luau` and the native
		// `init.runserver.luau` would flip between walks — and every flip
		// re-derives the whole subtree under a new node, which Studio sees as
		// a removal plus an addition of a duplicate. The candidate whose sync
		// rule comes first wins, then the name, always
		if let Some(chosen) = child_source_of(path, context, vfs)? {
			if let Some(snapshot) = new_snapshot_file_child(&chosen, context, vfs)? {
				return Ok(Some(snapshot));
			}
		}

		new_snapshot_dir(path, context, vfs)
	}
}

/// How much a child script's suffix looks like something *this* fork's
/// writer produced, lower being preferred.
///
/// Ranking by sync-rule order alone is not enough, because the default rules
/// list `.server` and `.client` before `.local` and `.runserver`
/// (`constants::default_sync_rules`). Those first two are also the spellings
/// the previous tool used for the two classes this fork spells differently:
/// its LocalScript was `init.client.luau`, ours is `init.local.luau`; its
/// RunContext = Server script was `init.server.luau`, ours is
/// `init.runserver.luau`. A converted directory therefore holds both for as
/// long as the old file survives the conversion, and picking by rule order
/// would pick the file the writer has just replaced — reading the directory
/// back as the *old* class, which Studio then sees as a removal plus a
/// duplicate addition of the whole subtree. Seen live on
/// `Client/Local/Emitter2D` and `InGameExplorer/.../InGameExplorerClient`.
///
/// So the suffix the writer would emit wins over the one it migrated away
/// from, and everything else keeps the rules' own order below that
fn child_source_rank(middleware: &Middleware) -> u8 {
	match middleware {
		// The two this fork writes for classes the old layout spelled
		// differently — these must outrank their legacy twins
		Middleware::LocalScript => 0,
		Middleware::RunServerScript => 1,
		Middleware::ModuleScript => 2,
		// Also native (a `Script` with RunContext Client / Legacy), but they
		// double as the old layout's LocalScript and RunContext = Server
		Middleware::ClientScript => 3,
		Middleware::ServerScript => 4,
		_ => 5,
	}
}

/// The one child script a directory is read through, chosen the same way on
/// every walk: the suffix this fork's writer would emit first
/// ([`child_source_rank`]), then the sync rule's own order, then the lexically
/// smallest name. Any other candidate is reported once per walk and never
/// synced
fn child_source_of(dir: &Path, context: &Context, vfs: &Vfs) -> Result<Option<PathBuf>> {
	let rules = context.sync_rules();
	let mut candidates: Vec<(u8, usize, PathBuf)> = Vec::new();

	for entry in vfs.read_dir(dir)? {
		if !vfs.is_file(&entry) {
			continue;
		}

		let matched = rules.iter().enumerate().find_map(|(index, rule)| {
			rule.resolve_child(&entry)
				.filter(|resolved| scope_allows_rule(context, &resolved.middleware, &entry))
				.map(|resolved| (child_source_rank(&resolved.middleware), index))
		});

		if let Some((rank, index)) = matched {
			candidates.push((rank, index, entry));
		}
	}

	candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));

	let mut chosen = candidates.into_iter();
	let first = chosen.next().map(|(_, _, path)| path);

	if let Some(first) = &first {
		for (_, _, other) in chosen {
			warn!(
				"{} and {} are both child scripts of the same directory; only {} is synced",
				first.display(),
				other.display(),
				first.get_name()
			);
		}
	}

	Ok(first)
}

/// Create a snapshot of a regular file,
/// example: `foo/bar.luau`
fn new_snapshot_file(path: &Path, context: &Context, vfs: &Vfs) -> Result<Option<Snapshot>> {
	if let Some(resolved) = context.sync_rules().iter().find_map(|rule| {
		rule.resolve(path)
			.filter(|resolved| scope_allows_rule(context, &resolved.middleware, path))
	}) {
		let middleware = resolved.middleware;
		let name = resolved.name;

		let mut snapshot = middleware.read(path, context, vfs)?;

		if middleware != Middleware::Project {
			snapshot.set_name(&name);
			snapshot.meta.set_context(context);
			snapshot.meta.set_source(Source::file(path));
		} else if snapshot.class == "Folder" && snapshot.children.is_empty() {
			return Ok(None);
		}

		if let Some(instance_data) = get_instance_data(&name, Some(&snapshot.class), path, context, vfs)? {
			snapshot.apply_data(instance_data);
		}

		Ok(Some(snapshot))
	} else {
		Ok(None)
	}
}

/// Whether this path is itself a child source for its directory — an
/// `init.*` a middleware would claim the parent through. Mirrors the
/// condition [`new_snapshot_file_child`] uses to recurse, so exactly the
/// entries that would come back here are the ones skipped
fn is_child_source(path: &Path, context: &Context) -> bool {
	context.sync_rules().iter().any(|rule| {
		rule.resolve_child(path)
			.is_some_and(|resolved| scope_allows_rule(context, &resolved.middleware, path))
	})
}

/// Create a snapshot of a directory that has a child source or data,
/// example: `foo/bar/init.luau`
fn new_snapshot_file_child(path: &Path, context: &Context, vfs: &Vfs) -> Result<Option<Snapshot>> {
	if path.contains(&[".src.luau"]) || path.contains(&[".src.lua"]) {
		wsync_warn!(
			"Your project uses legacy {} files which won't be supported in future versions of WSync. \
			Make sure to rename {} file to {} for future compatibility!",
			".src".bold(),
			path.to_string().bold(),
			path.to_string().replace(".src", "init").bold()
		);
	}

	if let Some(resolved) = context.sync_rules().iter().find_map(|rule| {
		rule.resolve_child(path)
			.filter(|resolved| scope_allows_rule(context, &resolved.middleware, path))
	}) {
		let middleware = resolved.middleware;
		let name = resolved.name;
		let parent = path.get_parent();

		let mut snapshot = middleware.read(path, context, vfs)?;

		if middleware != Middleware::Project {
			snapshot.set_name(&name);
			snapshot.meta.set_context(context);
			snapshot.meta.set_source(Source::child_file(parent, path));

			for entry in vfs.read_dir(parent)? {
				if entry == path {
					continue;
				}

				// A directory has exactly one child script. A second `init.*`
				// beside this one is not an instance *inside* it: descending
				// into it lands straight back here with the two files
				// swapped, each treating the other as its child, and the
				// recursion never ends. Every level allocates a snapshot
				// holding the file's whole source and a cloned context, so
				// the process climbs until the stack overflows — which on
				// Windows kills it without unwinding, with no panic message
				// and an exit code of 0. Seen live on a directory carrying
				// both `init.server.luau` and `init.runserver.luau`.
				if is_child_source(&entry, context) {
					warn!(
						"{} and {} are both child scripts of the same directory; only the first is synced",
						path.display(),
						entry.display()
					);

					continue;
				}

				if let Some(child_snapshot) = new_snapshot(&entry, context, vfs)? {
					snapshot.add_child(child_snapshot);
				}
			}
		} else if snapshot.class == "Folder" && snapshot.children.is_empty() {
			return Ok(None);
		}

		if let Some(instance_data) = get_instance_data(&name, Some(&snapshot.class), parent, context, vfs)? {
			snapshot.apply_data(instance_data);
		}

		Ok(Some(snapshot))
	} else {
		Ok(None)
	}
}

/// Create snapshot of a directory,
/// example: `foo/bar`
/// The class a directory takes from its own name, for the containers Studio
/// already provides inside a service — `StarterPlayerScripts` and its
/// character twin under `StarterPlayer`.
///
/// Such a directory *is* that container, not a Folder that happens to share
/// its name: syncing it as a Folder puts a second instance beside the real one,
/// and everything inside lands in the copy Roblox never runs. Only these
/// containers are inferred — an ordinary directory is still a Folder, and true
/// services are not, since a `Lighting` folder nested in code is a folder and
/// could not be parented there as a service anyway.
pub(crate) fn dir_container_class(name: &str) -> Option<&'static str> {
	match name {
		"StarterPlayerScripts" => Some("StarterPlayerScripts"),
		"StarterCharacterScripts" => Some("StarterCharacterScripts"),
		_ => None,
	}
}

fn new_snapshot_dir(path: &Path, context: &Context, vfs: &Vfs) -> Result<Option<Snapshot>> {
	let mut snapshot = dir::read_dir(path, context, vfs)?;

	if let Some(class) = dir_container_class(&snapshot.name) {
		snapshot.set_class(class);
	}

	// A data sidecar still wins: it is the explicit statement of what the
	// instance is, and this inference is only a default
	if let Some(instance_data) = get_instance_data(&snapshot.name, Some(&snapshot.class), path, context, vfs)? {
		snapshot.apply_data(instance_data);
	}

	Ok(Some(snapshot))
}

fn get_instance_data(
	name: &str,
	class: Option<&str>,
	path: &Path,
	context: &Context,
	vfs: &Vfs,
) -> Result<Option<DataSnapshot>> {
	// Data sidecars ride along in every scope: code scope needs them for the
	// state a suffix cannot encode (attributes, tags, RunContext.Plugin), or
	// those instances re-flag the connect-time review forever
	for sync_rule in context.sync_rules_of_type(&Middleware::InstanceData, false) {
		if let Some(data_path) = sync_rule.locate(path, name, vfs.is_dir(path)) {
			if vfs.exists(&data_path) {
				let data = data::read_data(&data_path, class, vfs).with_desc(|| {
					format!(
						"Failed to get instance data at {}",
						data_path.display().to_string().bold()
					)
				})?;

				return Ok(Some(data));
			}
		}
	}

	Ok(None)
}

// ------------------------------------------------------------------- tests --

#[cfg(test)]
mod tests {
	use super::*;
	use crate::constants::default_sync_rules;
	use rbx_dom_weak::HashMapExt as _;

	/// The whole suffix scheme, read direction: one suffix, one meaning.
	#[test]
	fn every_script_suffix_resolves_to_exactly_one_middleware() {
		let resolve = |file: &str| -> Option<(Middleware, String)> {
			default_sync_rules()
				.iter()
				.find_map(|rule| rule.resolve(Path::new(file)))
				.map(|resolved| (resolved.middleware, resolved.name))
		};

		for (file, middleware) in [
			("Foo.server.luau", Middleware::ServerScript),
			("Foo.client.luau", Middleware::ClientScript),
			("Foo.local.luau", Middleware::LocalScript),
			("Foo.runserver.luau", Middleware::RunServerScript),
			("Foo.luau", Middleware::ModuleScript),
			("Foo.server.lua", Middleware::ServerScript),
			("Foo.local.lua", Middleware::LocalScript),
			("Foo.runserver.lua", Middleware::RunServerScript),
		] {
			let (resolved, name) = resolve(file).unwrap_or_else(|| panic!("{file} resolves to nothing"));
			assert_eq!(resolved, middleware, "{file}");
			assert_eq!(name, "Foo", "{file} must strip its whole suffix");
		}

		// A name that merely *contains* a suffix word is not that suffix:
		// `runserver` has no dot before `server`, so the `.server` rule must
		// not shadow it, and an unrelated tail stays a module.
		let (resolved, name) = resolve("Observer.luau").unwrap();
		assert_eq!(resolved, Middleware::ModuleScript);
		assert_eq!(name, "Observer");
	}

	/// The write direction consumes RunContext into the suffix choice — the
	/// exact inverse of the table above.
	#[test]
	fn from_class_encodes_run_context_in_the_middleware() {
		use rbx_dom_weak::types::{Enum, Variant};

		let script_with = |run_context: u32| -> (Option<Middleware>, Properties) {
			let mut properties = Properties::new();
			properties.insert(ustr("RunContext"), Variant::Enum(Enum::from_u32(run_context)));
			let middleware = Middleware::from_class("Script", Some(&mut properties));
			(middleware, properties)
		};

		let (middleware, leftovers) = script_with(0);
		assert_eq!(middleware, Some(Middleware::ServerScript));
		assert!(leftovers.is_empty(), "Legacy is the suffix itself");

		let (middleware, leftovers) = script_with(1);
		assert_eq!(middleware, Some(Middleware::RunServerScript));
		assert!(leftovers.is_empty());

		let (middleware, leftovers) = script_with(2);
		assert_eq!(middleware, Some(Middleware::ClientScript));
		assert!(leftovers.is_empty());

		// RunContext.Plugin has no suffix: fall back to `.server` and keep the
		// value as a property for the data sidecar to carry.
		let (middleware, leftovers) = script_with(3);
		assert_eq!(middleware, Some(Middleware::ServerScript));
		assert!(leftovers.contains_key(&ustr("RunContext")));

		let mut empty = Properties::new();
		assert_eq!(
			Middleware::from_class("LocalScript", Some(&mut empty)),
			Some(Middleware::LocalScript)
		);
		assert_eq!(
			Middleware::from_class("Script", None),
			Some(Middleware::ServerScript),
			"callers without properties get the Legacy fallback"
		);
	}

	/// Read and write agree: reading the file a class+RunContext writes to
	/// produces that class+RunContext back.
	#[test]
	fn the_suffix_scheme_round_trips_class_and_run_context() {
		use crate::middleware::luau::ScriptType;

		for (middleware, class, run_context) in [
			(Middleware::ServerScript, "Script", Some(0)),
			(Middleware::RunServerScript, "Script", Some(1)),
			(Middleware::ClientScript, "Script", Some(2)),
			(Middleware::LocalScript, "LocalScript", None),
			(Middleware::ModuleScript, "ModuleScript", None),
		] {
			let script_type: ScriptType = middleware.clone().into();
			let (read_class, read_run_context) = match &script_type {
				ScriptType::Legacy => ("Script", Some(0)),
				ScriptType::Server => ("Script", Some(1)),
				ScriptType::Client => ("Script", Some(2)),
				ScriptType::Local => ("LocalScript", None),
				ScriptType::Module => ("ModuleScript", None),
			};
			assert_eq!(read_class, class, "{middleware:?}");
			assert_eq!(read_run_context, run_context, "{middleware:?}");

			// And the writer picks the same middleware back.
			if class == "ModuleScript" {
				continue;
			}
			let mut properties = Properties::new();
			if let Some(run_context) = run_context {
				properties.insert(
					ustr("RunContext"),
					rbx_dom_weak::types::Variant::Enum(rbx_dom_weak::types::Enum::from_u32(run_context)),
				);
			}
			assert_eq!(
				Middleware::from_class(class, Some(&mut properties)),
				Some(middleware.clone()),
				"{middleware:?} must be its own inverse"
			);
		}
	}
}
