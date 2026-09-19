//! Place-id routing: `wsync places`, the global `--place <placeId>`
//! selector, and the scratch-project rules `wsync open` attaches with
//! (docs/places.md).
//!
//! WSync addresses a daemon by project directory (`--project`) or by port
//! (`--port`). Neither travels: porting an asset between two places, or
//! driving a place someone else's daemon serves, means naming the *place*.
//! A place id is the one identifier both sides of that work already share,
//! so this module turns one into a daemon.
//!
//! The registry is the set of runtime records under `<state>/daemons/*.json`
//! — the same files `wsync daemon status` reads. Each record names a port;
//! `GET /hello` on that port is what proves the daemon is alive, which place
//! ids it claims, and whether it serves a branch. A record whose daemon does
//! not answer is not dropped: its project file still says which places it
//! would serve, so `--place` can explain what exists instead of reporting
//! nothing at all.

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::{json, Value};
use std::{
	fs,
	path::{Path, PathBuf},
	sync::Mutex,
	thread,
	time::Duration,
};

use crate::{
	cli::client::{self, Hello},
	daemon::{self, RuntimeRecord},
	ext::PathExt,
	project::Project,
};

/// How long a `/hello` probe waits while listing or resolving
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Directory name that holds scratch place projects under the desktop app's
/// projects root
pub const SCRATCH_DIR: &str = "_places";

////////////////////////////////////////////////////////////////////////////////
// The global selector
////////////////////////////////////////////////////////////////////////////////

/// `--place <id>` / `--branch <name>` as the top-level parser saw them.
///
/// They are global options on `Cli`, not fields on [`Targeting`], for one
/// reason: every live command already flattens `Targeting`, and several
/// build one as a struct literal, so a new field there is a change to every
/// call site. A global option reaches the same commands without touching any
/// of them, and this is where the parsed value is parked for
/// [`Target::resolve`] to pick up.
static SELECTOR: Mutex<Option<(u64, Option<String>)>> = Mutex::new(None);

/// Records the parsed `--place`/`--branch`. Called once, from `Cli::main`
pub fn set_selector(place: Option<u64>, branch: Option<String>) {
	*SELECTOR.lock().unwrap_or_else(|error| error.into_inner()) = place.map(|place| (place, branch));
}

/// The place selector this invocation carries, if any
pub fn selector() -> Option<(u64, Option<String>)> {
	SELECTOR.lock().unwrap_or_else(|error| error.into_inner()).clone()
}

////////////////////////////////////////////////////////////////////////////////
// Resolution
////////////////////////////////////////////////////////////////////////////////

/// Where a candidate's place claim came from. A live claim is the daemon's
/// own answer; a file claim is only what its project file asks for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimSource {
	/// `GET /hello` answered for this record's boot
	Live,
	/// The daemon did not answer; its project file was read instead
	ProjectFile,
}

/// One daemon `--place` could resolve to
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
	/// Project file path, as the record or `/hello` spells it
	pub project: String,
	pub canonical: String,
	pub port: u16,
	pub place_ids: Vec<u64>,
	pub game_id: Option<u64>,
	/// The branch this daemon serves; `None` is main (or any non-branch
	/// project), which is what an unqualified `--place` prefers
	pub branch: Option<String>,
	pub source: ClaimSource,
}

impl Candidate {
	pub fn is_live(&self) -> bool {
		matches!(self.source, ClaimSource::Live)
	}

	fn claims(&self, place: u64) -> bool {
		self.place_ids.contains(&place)
	}

	/// `project (port 7970, branch ui-pass)` — the shape every ambiguity
	/// message lists candidates in
	fn describe(&self) -> String {
		let branch = match &self.branch {
			Some(branch) => format!(", branch {branch}"),
			None => String::from(", main"),
		};
		let live = if self.is_live() { "" } else { ", not answering" };

		format!("{} (port {}{branch}{live})", self.project, self.port)
	}
}

/// `--branch` spellings that mean "the daemon that serves no branch"
fn means_main(branch: &str) -> bool {
	matches!(branch.trim(), "main" | "-" | "")
}

/// Picks the one daemon that serves `place`.
///
/// The rules, in order:
///
/// 1. keep every candidate whose claim includes the place id;
/// 2. with `--branch <name>`, keep only that branch (`main` and `-` mean the
///    branchless daemon) — an empty result is an error naming what was found,
///    never a silent fall-through to another branch;
/// 3. otherwise, a live daemon always beats one that only claims the place in
///    its project file;
/// 4. then prefer the branchless daemon, so `--place <id>` on a place with a
///    branch open still means main;
/// 5. anything still tied is ambiguous and refuses, listing the candidates.
///
/// Pure on purpose: the probing lives in [`candidates`], the policy here.
pub fn resolve<'a>(candidates: &'a [Candidate], place: u64, branch: Option<&str>) -> Result<&'a Candidate> {
	let claiming: Vec<&Candidate> = candidates.iter().filter(|candidate| candidate.claims(place)).collect();

	if claiming.is_empty() {
		bail!(
			"No WSync daemon claims place {place}. Start one for the project that serves it \
			 (`{}`), or open the place with `{}`",
			"wsync daemon start --project <path>".bold(),
			format!("wsync open {place}").bold()
		);
	}

	let pool: Vec<&Candidate> = match branch {
		Some(wanted) if means_main(wanted) => claiming
			.iter()
			.copied()
			.filter(|candidate| candidate.branch.is_none())
			.collect(),
		Some(wanted) => claiming
			.iter()
			.copied()
			.filter(|candidate| candidate.branch.as_deref() == Some(wanted))
			.collect(),
		None => claiming.clone(),
	};

	if pool.is_empty() {
		let wanted = branch.unwrap_or_default();

		bail!(
			"No daemon serving place {place} is on branch `{wanted}`. Daemons claiming it: {}",
			describe_all(&claiming)
		);
	}

	let live: Vec<&Candidate> = pool.iter().copied().filter(|candidate| candidate.is_live()).collect();
	let pool = if live.is_empty() { pool } else { live };

	if pool.len() == 1 {
		return Ok(pool[0]);
	}

	// A branch daemon and main's both claim the same place by design (a
	// branch is a clone of that place), so "main unless asked otherwise" is
	// the only reading of a bare --place that does not surprise
	if branch.is_none() {
		let main: Vec<&Candidate> = pool
			.iter()
			.copied()
			.filter(|candidate| candidate.branch.is_none())
			.collect();

		if main.len() == 1 {
			return Ok(main[0]);
		}
	}

	bail!(
		"Place {place} is served by {} daemons: {}. Narrow it with --branch <name> (or `--branch main`), \
		 or address one directly with --project/--port",
		pool.len(),
		describe_all(&pool)
	)
}

fn describe_all(candidates: &[&Candidate]) -> String {
	candidates
		.iter()
		.map(|candidate| candidate.describe())
		.collect::<Vec<_>>()
		.join("; ")
}

////////////////////////////////////////////////////////////////////////////////
// The live registry
////////////////////////////////////////////////////////////////////////////////

/// One runtime record plus whatever the daemon it names answered
pub struct DaemonEntry {
	pub record: RuntimeRecord,
	/// `Some` only when `/hello` answered *and* its boot matches the record:
	/// a port that now serves something else is not this daemon
	pub hello: Option<Hello>,
	/// The record's project file, when it still parses
	pub project: Option<ProjectClaim>,
}

/// What a project file says about the places it serves — the fallback claim
/// for a daemon that does not answer
pub struct ProjectClaim {
	pub name: String,
	pub game_id: Option<u64>,
	pub place_ids: Vec<u64>,
	pub branch: Option<String>,
}

impl DaemonEntry {
	pub fn name(&self) -> String {
		if let Some(hello) = &self.hello {
			if !hello.name.is_empty() {
				return hello.name.clone();
			}
		}

		if let Some(project) = &self.project {
			if !project.name.is_empty() {
				return project.name.clone();
			}
		}

		Path::new(&self.record.project).get_parent().file_name().map_or_else(
			|| self.record.project.clone(),
			|name| name.to_string_lossy().into_owned(),
		)
	}

	pub fn place_ids(&self) -> Vec<u64> {
		match &self.hello {
			Some(hello) => hello.place_ids.clone(),
			None => self
				.project
				.as_ref()
				.map(|project| project.place_ids.clone())
				.unwrap_or_default(),
		}
	}

	pub fn game_id(&self) -> Option<u64> {
		match &self.hello {
			Some(hello) => hello.game_id,
			None => self.project.as_ref().and_then(|project| project.game_id),
		}
	}

	pub fn branch(&self) -> Option<String> {
		match &self.hello {
			Some(hello) => hello.branch.clone(),
			None => self.project.as_ref().and_then(|project| project.branch.clone()),
		}
	}

	/// The project directory this daemon serves
	pub fn workspace(&self) -> PathBuf {
		Path::new(&self.record.project).get_parent().to_path_buf()
	}

	/// A scratch place project created by `wsync open` attach mode
	pub fn is_attach(&self) -> bool {
		self.record.ephemeral || is_scratch_dir(&self.workspace())
	}

	pub fn candidate(&self) -> Candidate {
		Candidate {
			project: self.record.project.clone(),
			canonical: self.record.canonical_project.clone(),
			port: self.hello.as_ref().map_or(self.record.port, |hello| hello.port),
			place_ids: self.place_ids(),
			game_id: self.game_id(),
			branch: self.branch(),
			source: if self.hello.is_some() {
				ClaimSource::Live
			} else {
				ClaimSource::ProjectFile
			},
		}
	}
}

/// Reads every runtime record and probes the daemon each one names.
///
/// Probes run in parallel: a stale record costs a full probe timeout, and a
/// machine with a branch or two open has several of them.
pub fn registry() -> Result<Vec<DaemonEntry>> {
	// The CLI keeps its records under the local data dir; the desktop app
	// keeps its own under the roaming one (it hands that dir to every daemon
	// it spawns). Either kind of daemon serves a place, so both are read
	let mut dirs = vec![daemon::state_dir(None)?];

	if let Some(base) = directories::BaseDirs::new() {
		let roaming = base.data_dir().join("WSync");

		if !dirs.iter().any(|dir| dir == &roaming) {
			dirs.push(roaming);
		}
	}

	let mut records: Vec<RuntimeRecord> = Vec::new();

	for dir in &dirs {
		for record in read_records(&dir.join("daemons")) {
			if !records.iter().any(|known| known.port == record.port && known.pid == record.pid) {
				records.push(record);
			}
		}
	}

	let entries = thread::scope(|scope| {
		let probes: Vec<_> = records
			.into_iter()
			.map(|record| {
				let port = record.port;
				let handle = scope.spawn(move || probe(port));

				(record, handle)
			})
			.collect();

		probes
			.into_iter()
			.map(|(record, handle)| {
				let hello = handle
					.join()
					.ok()
					.flatten()
					// A record points at a port, not at a process: a daemon
					// that died and left its port to another project must
					// never be reported as this record's daemon
					.filter(|hello: &Hello| {
						hello.boot_id == record.boot_id && hello.canonical_project == record.canonical_project
					});

				let project = Project::load(Path::new(&record.project))
					.ok()
					.map(|project| ProjectClaim {
						name: project.name.clone(),
						game_id: project.game_id,
						place_ids: project.place_ids.clone(),
						branch: project.branch.clone(),
					});

				DaemonEntry { record, hello, project }
			})
			.collect::<Vec<_>>()
	});

	Ok(entries)
}

/// Every candidate `--place` may resolve to, newest daemon first
pub fn candidates(entries: &[DaemonEntry]) -> Vec<Candidate> {
	entries.iter().map(DaemonEntry::candidate).collect()
}

fn read_records(dir: &Path) -> Vec<RuntimeRecord> {
	let Ok(entries) = fs::read_dir(dir) else {
		return Vec::new();
	};

	let mut records: Vec<RuntimeRecord> = entries
		.flatten()
		.map(|entry| entry.path())
		.filter(|path| path.extension().is_some_and(|extension| extension == "json"))
		// `.start.lock` and the staging temp files share the directory
		.filter(|path| {
			path.file_name()
				.and_then(|name| name.to_str())
				.is_some_and(|name| !name.starts_with('.'))
		})
		.filter_map(|path| daemon::read_record(&path).ok().flatten())
		.collect();

	records.sort_by(|left, right| {
		right
			.started_at
			.cmp(&left.started_at)
			.then_with(|| left.project.cmp(&right.project))
	});

	records
}

/// `GET /hello`, tolerant: anything that is not a daemon answering for this
/// port is simply "no daemon"
pub fn probe(port: u16) -> Option<Hello> {
	reqwest::blocking::Client::builder()
		.no_proxy()
		.timeout(PROBE_TIMEOUT)
		.redirect(reqwest::redirect::Policy::none())
		.build()
		.ok()?
		.get(format!("http://localhost:{port}/hello"))
		.send()
		.ok()?
		.error_for_status()
		.ok()?
		.json()
		.ok()
}

////////////////////////////////////////////////////////////////////////////////
// Scratch place projects
////////////////////////////////////////////////////////////////////////////////

/// The desktop app's projects root, as `%APPDATA%\WSync\state.json` records
/// it. Absent whenever the app has never run here
pub fn projects_root() -> Option<PathBuf> {
	let base = directories::BaseDirs::new()?;
	let state = fs::read_to_string(base.config_dir().join("WSync").join("state.json")).ok()?;
	let state: Value = serde_json::from_str(&state).ok()?;
	let root = state.get("projectsRoot").and_then(Value::as_str)?;

	if root.is_empty() {
		return None;
	}

	Some(PathBuf::from(root))
}

/// Where scratch place projects live: `<projectsRoot>/_places` when the
/// desktop app named a projects root, else `~/.wsync/places`
pub fn scratch_root() -> Result<PathBuf> {
	if let Some(root) = projects_root() {
		return Ok(root.join(SCRATCH_DIR));
	}

	let base = directories::BaseDirs::new().context("Failed to resolve the home directory")?;

	Ok(base.home_dir().join(".wsync").join("places"))
}

/// Every directory a scratch project is allowed to live in. Both are checked
/// regardless of which one is current, so a project created before the
/// desktop app set a projects root is still recognized afterwards
fn scratch_roots() -> Vec<PathBuf> {
	let mut roots = Vec::new();

	if let Some(root) = projects_root() {
		roots.push(root.join(SCRATCH_DIR));
	}

	if let Some(base) = directories::BaseDirs::new() {
		roots.push(base.home_dir().join(".wsync").join("places"));
	}

	roots
}

/// Whether `dir` is a scratch place project this tool may delete.
///
/// The auto-delete is the only destructive thing place routing does, so the
/// rule is deliberately narrow: the directory must be named after a place id
/// (digits only) and sit *directly* inside one of the two scratch roots.
/// A project the user pointed at with `--into` never matches, and neither
/// does anything else on disk
pub fn is_scratch_dir(dir: &Path) -> bool {
	let named_after_a_place = dir
		.file_name()
		.and_then(|name| name.to_str())
		.is_some_and(|name| !name.is_empty() && name.chars().all(|character| character.is_ascii_digit()));

	if !named_after_a_place {
		return false;
	}

	let Some(parent) = dir.parent() else {
		return false;
	};

	scratch_roots().iter().any(|root| same_path(root, parent))
}

/// Path equality that survives the spellings Windows hands back (`\` vs `/`,
/// case, a trailing separator). Canonicalization is tried first and falls
/// back to a normalized comparison for paths that do not exist yet
fn same_path(left: &Path, right: &Path) -> bool {
	if let (Ok(left), Ok(right)) = (fs::canonicalize(left), fs::canonicalize(right)) {
		return left == right;
	}

	normalize(left) == normalize(right)
}

fn normalize(path: &Path) -> String {
	let text = path.to_string_lossy().replace('\\', "/");
	let text = text.trim_end_matches('/').to_owned();

	if cfg!(windows) {
		text.to_lowercase()
	} else {
		text
	}
}

/// Writes a minimal place project into `dir`, creating the source
/// directories the tree maps.
///
/// `initialSync: "keep"` is the whole point of a scratch project: it exists
/// to *reach* a place, not to reconcile with it, so a connect must move
/// nothing in either direction. Code scope keeps the projection to scripts
/// and folders
pub fn write_scratch_project(dir: &Path, place_id: u64, game_id: Option<u64>) -> Result<PathBuf> {
	let project_path = dir.join("default.project.json");

	for service in SCRATCH_SERVICES {
		fs::create_dir_all(dir.join("src").join(service))
			.with_context(|| format!("Failed to create the scratch source directory for {service}"))?;
	}

	let mut tree = serde_json::Map::new();

	tree.insert("$className".to_owned(), json!("DataModel"));

	for service in SCRATCH_SERVICES {
		if let Some((parent, child)) = NESTED.iter().find(|(_, child)| *child == service) {
			let entry = tree
				.entry((*parent).to_owned())
				.or_insert_with(|| json!({}))
				.as_object_mut()
				.expect("scratch tree nodes are objects");

			entry.insert((*child).to_owned(), json!({ "$path": format!("src/{child}") }));
		} else {
			tree.insert((*service).to_owned(), json!({ "$path": format!("src/{service}") }));
		}
	}

	let project = json!({
		"name": format!("place-{place_id}"),
		"scope": "code",
		"initialSync": "keep",
		"servePlaceIds": [place_id],
		"gameId": game_id,
		"ignoreGlobs": ["**/.git/**", "**/.wsync-*/**"],
		"tree": Value::Object(tree),
	});

	// `gameId: null` would fail the project parser's stricter readings; drop
	// the key entirely when the universe is unknown
	let mut project = project;
	if game_id.is_none() {
		project.as_object_mut().expect("object").remove("gameId");
	}

	fs::write(&project_path, format!("{}\n", serde_json::to_string_pretty(&project)?))
		.with_context(|| format!("Failed to write {}", project_path.to_string()))?;

	Ok(project_path)
}

/// Services a scratch project maps. Code scope means only scripts and
/// folders travel, so this is about having somewhere for them to land
const SCRATCH_SERVICES: [&str; 8] = [
	"ReplicatedFirst",
	"ReplicatedStorage",
	"ServerScriptService",
	"ServerStorage",
	"StarterGui",
	"StarterPlayerScripts",
	"StarterCharacterScripts",
	"Workspace",
];

/// Services that are not top-level in the DataModel
const NESTED: [(&str, &str); 2] = [
	("StarterPlayer", "StarterPlayerScripts"),
	("StarterPlayer", "StarterCharacterScripts"),
];

/// Removes a scratch project directory, refusing anything that is not one.
/// Called by the daemon process as it exits (`wsync daemon start
/// --ephemeral`), so a refusal is a warning, never a failed command
pub fn remove_scratch_dir(dir: &Path) -> Result<()> {
	if !is_scratch_dir(dir) {
		bail!(
			"Refusing to delete {}: it is not a scratch place project under {}",
			dir.to_string(),
			SCRATCH_DIR
		);
	}

	fs::remove_dir_all(dir).with_context(|| format!("Failed to remove the scratch project {}", dir.to_string()))
}

////////////////////////////////////////////////////////////////////////////////
// `wsync places`
////////////////////////////////////////////////////////////////////////////////

/// List every running daemon and the places it serves
#[derive(Parser)]
pub struct Places {
	/// Print machine-readable JSON (one array)
	#[arg(long)]
	raw: bool,
}

impl Places {
	pub fn main(self) -> Result<()> {
		let entries = registry()?;

		if self.raw {
			let rows: Vec<Value> = entries.iter().map(row_json).collect();

			client::print_json(&Value::Array(rows));

			return Ok(());
		}

		if entries.is_empty() {
			println!(
				"No WSync daemons are recorded. Start one with `{}`",
				"wsync daemon start --project <path>".bold()
			);

			return Ok(());
		}

		let rows: Vec<[String; 8]> = entries.iter().map(row_cells).collect();
		let headers = [
			"PROJECT",
			"PORT",
			"GAME",
			"PLACES",
			"BRANCH",
			"MANAGED BY",
			"PLUGIN",
			"KIND",
		];
		let mut widths = headers.map(str::len);

		for row in &rows {
			for (index, cell) in row.iter().enumerate() {
				widths[index] = widths[index].max(cell.chars().count());
			}
		}

		println!("{}", render(&headers.map(str::to_owned), &widths).bold());

		for row in &rows {
			println!("{}", render(row, &widths));
		}

		Ok(())
	}
}

fn render(cells: &[String; 8], widths: &[usize; 8]) -> String {
	cells
		.iter()
		.enumerate()
		.map(|(index, cell)| {
			let padding = widths[index].saturating_sub(cell.chars().count());

			format!("{cell}{}", " ".repeat(padding))
		})
		.collect::<Vec<_>>()
		.join("  ")
		.trim_end()
		.to_owned()
}

fn kind(entry: &DaemonEntry) -> String {
	if !entry.is_attach() {
		return String::from("project");
	}

	if entry.record.ephemeral {
		String::from("attach (ephemeral)")
	} else {
		String::from("attach")
	}
}

fn plugin_cell(entry: &DaemonEntry) -> String {
	match &entry.hello {
		None => String::from("unreachable"),
		Some(hello) => match &hello.plugin {
			Some(plugin) => format!("{} ({})", plugin.name, plugin.transport),
			None => String::from("waiting"),
		},
	}
}

fn places_cell(entry: &DaemonEntry) -> String {
	let ids = entry.place_ids();

	if ids.is_empty() {
		return String::from("-");
	}

	ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
}

fn row_cells(entry: &DaemonEntry) -> [String; 8] {
	[
		entry.name(),
		entry.record.port.to_string(),
		entry.game_id().map_or_else(|| String::from("-"), |id| id.to_string()),
		places_cell(entry),
		entry.branch().unwrap_or_else(|| String::from("main")),
		entry.record.managed_by.clone(),
		plugin_cell(entry),
		kind(entry),
	]
}

fn row_json(entry: &DaemonEntry) -> Value {
	json!({
		"name": entry.name(),
		"project": entry.record.project,
		"canonicalProject": entry.record.canonical_project,
		"port": entry.record.port,
		"pid": entry.record.pid,
		"bootId": entry.record.boot_id,
		"gameId": entry.game_id(),
		"placeIds": entry.place_ids(),
		"branch": entry.branch(),
		"managedBy": entry.record.managed_by,
		"live": entry.hello.is_some(),
		"plugin": entry.hello.as_ref().and_then(|hello| hello.plugin.as_ref()).map(|plugin| {
			json!({ "name": plugin.name, "transport": plugin.transport })
		}),
		"attach": entry.is_attach(),
		"ephemeral": entry.record.ephemeral,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn candidate(project: &str, port: u16, places: &[u64], branch: Option<&str>, source: ClaimSource) -> Candidate {
		Candidate {
			project: project.to_owned(),
			canonical: project.to_owned(),
			port,
			place_ids: places.to_vec(),
			game_id: Some(7),
			branch: branch.map(str::to_owned),
			source,
		}
	}

	fn main_daemon() -> Candidate {
		candidate("/main/default.project.json", 7970, &[111, 222], None, ClaimSource::Live)
	}

	fn branch_daemon() -> Candidate {
		candidate(
			"/branch/default.project.json",
			7971,
			&[111],
			Some("ui-pass"),
			ClaimSource::Live,
		)
	}

	#[test]
	fn a_single_claim_resolves() {
		let pool = vec![
			main_daemon(),
			candidate("/other/p.json", 7972, &[999], None, ClaimSource::Live),
		];

		assert_eq!(resolve(&pool, 222, None).unwrap().port, 7970);
	}

	#[test]
	fn no_claim_refuses_and_says_how_to_get_one() {
		let pool = vec![main_daemon()];
		let error = resolve(&pool, 42, None).unwrap_err().to_string();

		assert!(error.contains("No WSync daemon claims place 42"), "{error}");
		assert!(error.contains("wsync open 42"), "{error}");
	}

	#[test]
	fn main_wins_over_a_branch_serving_the_same_place() {
		let pool = vec![branch_daemon(), main_daemon()];

		assert_eq!(resolve(&pool, 111, None).unwrap().port, 7970);
	}

	#[test]
	fn branch_selects_that_branch() {
		let pool = vec![main_daemon(), branch_daemon()];

		assert_eq!(resolve(&pool, 111, Some("ui-pass")).unwrap().port, 7971);
		assert_eq!(resolve(&pool, 111, Some("main")).unwrap().port, 7970);
	}

	#[test]
	fn an_unknown_branch_refuses_instead_of_falling_back() {
		let pool = vec![main_daemon(), branch_daemon()];
		let error = resolve(&pool, 111, Some("nope")).unwrap_err().to_string();

		assert!(error.contains("is on branch `nope`"), "{error}");
		assert!(error.contains("ui-pass"), "{error}");
	}

	#[test]
	fn two_branches_on_one_place_are_ambiguous() {
		let pool = vec![
			branch_daemon(),
			candidate("/other/p.json", 7973, &[111], Some("fx"), ClaimSource::Live),
		];
		let error = resolve(&pool, 111, None).unwrap_err().to_string();

		assert!(error.contains("served by 2 daemons"), "{error}");
		assert!(error.contains("--branch"), "{error}");
	}

	#[test]
	fn a_live_daemon_beats_a_project_file_claim() {
		let pool = vec![
			candidate("/dead/p.json", 7974, &[111], None, ClaimSource::ProjectFile),
			main_daemon(),
		];

		assert_eq!(resolve(&pool, 111, None).unwrap().port, 7970);
	}

	#[test]
	fn a_project_file_claim_still_resolves_when_it_is_the_only_one() {
		let pool = vec![candidate("/dead/p.json", 7974, &[111], None, ClaimSource::ProjectFile)];
		let resolved = resolve(&pool, 111, None).unwrap();

		assert_eq!(resolved.port, 7974);
		assert!(!resolved.is_live());
	}

	#[test]
	fn two_mains_on_one_place_are_ambiguous() {
		let pool = vec![
			main_daemon(),
			candidate("/twin/p.json", 7975, &[111], None, ClaimSource::Live),
		];
		let error = resolve(&pool, 111, None).unwrap_err().to_string();

		assert!(error.contains("served by 2 daemons"), "{error}");
	}

	#[test]
	fn a_scratch_directory_is_recognized_only_under_a_scratch_root() {
		let root = scratch_root().unwrap();

		assert!(is_scratch_dir(&root.join("1234567")));
		assert!(!is_scratch_dir(&root.join("not-a-place")));
		assert!(!is_scratch_dir(&root.join("1234567").join("src")));
		assert!(!is_scratch_dir(Path::new("/tmp/1234567")));
	}

	#[test]
	fn a_scratch_project_keeps_both_sides_where_they_are() {
		let dir = tempfile::tempdir().unwrap();
		let path = write_scratch_project(dir.path(), 1234567, Some(42)).unwrap();
		let project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();

		assert_eq!(project["name"], json!("place-1234567"));
		assert_eq!(project["scope"], json!("code"));
		assert_eq!(project["initialSync"], json!("keep"));
		assert_eq!(project["servePlaceIds"], json!([1234567]));
		assert_eq!(project["gameId"], json!(42));
		assert_eq!(project["tree"]["$className"], json!("DataModel"));
		assert_eq!(
			project["tree"]["ReplicatedStorage"]["$path"],
			json!("src/ReplicatedStorage")
		);
		assert_eq!(
			project["tree"]["StarterPlayer"]["StarterPlayerScripts"]["$path"],
			json!("src/StarterPlayerScripts")
		);
		assert!(dir.path().join("src/ServerScriptService").is_dir());

		// It has to load as a place, or the daemon refuses to serve it
		let loaded = Project::load(&path).unwrap();

		assert!(loaded.is_place());
		assert_eq!(loaded.place_ids, vec![1234567]);
		assert_eq!(loaded.game_id, Some(42));
	}

	#[test]
	fn an_unknown_universe_leaves_the_key_out() {
		let dir = tempfile::tempdir().unwrap();
		let path = write_scratch_project(dir.path(), 99, None).unwrap();
		let project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();

		assert!(project.get("gameId").is_none());
	}
}
