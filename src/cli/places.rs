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
static SELECTOR: Mutex<Option<(Selector, Option<String>)>> = Mutex::new(None);

/// What `--place` named: a place id, or the `placeKey` of a place that has no
/// ids to be named by (docs/places.md — "Unpublished places").
///
/// The two cannot collide: a place id is digits only, and a key never is —
/// every key the broker mints carries a `-<hex>` suffix, and one written by
/// hand is refused as a key only if it happens to be all digits, in which case
/// it *is* a place id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
	Id(u64),
	Key(String),
}

impl Selector {
	/// A single all-digit argument is a place id; anything else is a key
	pub fn parse(text: &str) -> Self {
		let text = text.trim();

		if !text.is_empty() && text.chars().all(|character| character.is_ascii_digit()) {
			if let Ok(id) = text.parse() {
				return Self::Id(id);
			}
		}

		Self::Key(text.to_owned())
	}

	/// `place 128374926` / `place key `my-level-ab12cd34`` — the phrase every
	/// message about this selector uses
	pub fn describe(&self) -> String {
		match self {
			Self::Id(id) => format!("place {id}"),
			Self::Key(key) => format!("place key `{key}`"),
		}
	}
}

impl std::fmt::Display for Selector {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Id(id) => write!(formatter, "{id}"),
			Self::Key(key) => write!(formatter, "{key}"),
		}
	}
}

/// Records the parsed `--place`/`--branch`. Called once, from `Cli::main`
pub fn set_selector(place: Option<String>, branch: Option<String>) {
	*SELECTOR.lock().unwrap_or_else(|error| error.into_inner()) =
		place.map(|place| (Selector::parse(&place), branch));
}

/// The place selector this invocation carries, if any
pub fn selector() -> Option<(Selector, Option<String>)> {
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
	/// The `placeKey` this daemon's project claims, when it sets one
	/// (docs/places.md — "Unpublished places")
	pub place_key: Option<String>,
	/// The branch this daemon serves; `None` is main (or any non-branch
	/// project), which is what an unqualified `--place` prefers
	pub branch: Option<String>,
	pub source: ClaimSource,
}

impl Candidate {
	pub fn is_live(&self) -> bool {
		matches!(self.source, ClaimSource::Live)
	}

	fn claims(&self, place: &Selector) -> bool {
		match place {
			Selector::Id(id) => self.place_ids.contains(id),
			Selector::Key(key) => self.place_key.as_deref() == Some(key.as_str()),
		}
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
pub fn resolve<'a>(candidates: &'a [Candidate], place: &Selector, branch: Option<&str>) -> Result<&'a Candidate> {
	let claiming: Vec<&Candidate> = candidates.iter().filter(|candidate| candidate.claims(place)).collect();

	if claiming.is_empty() {
		let open = match place {
			Selector::Id(id) => format!("wsync open {id}"),
			// There is nothing to launch Studio on for a key: the place it
			// names exists only as a file somebody already has open
			Selector::Key(_) => String::from("wsync daemon start --project <path>"),
		};

		bail!(
			"No WSync daemon claims {}. Start one for the project that serves it \
			 (`{}`), or open the place with `{}`",
			place.describe(),
			"wsync daemon start --project <path>".bold(),
			open.bold()
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
			"No daemon serving {} is on branch `{wanted}`. Daemons claiming it: {}",
			place.describe(),
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
		"The {} is served by {} daemons: {}. Narrow it with --branch <name> (or `--branch main`), \
		 or address one directly with --project/--port",
		place.describe(),
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
	pub place_key: Option<String>,
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

	/// The `placeKey` this daemon claims, its own live answer first
	/// (docs/places.md — "Unpublished places")
	pub fn place_key(&self) -> Option<String> {
		match &self.hello {
			Some(hello) => hello.place_key.clone(),
			None => self.project.as_ref().and_then(|project| project.place_key.clone()),
		}
	}

	/// The project directory this daemon serves
	pub fn workspace(&self) -> PathBuf {
		Path::new(&self.record.project).get_parent().to_path_buf()
	}

	/// Whether this daemon stops itself once Studio goes away. The daemon's
	/// own `/hello` is the authority; the runtime record answers for one that
	/// is no longer there to ask
	pub fn ephemeral(&self) -> bool {
		match &self.hello {
			Some(hello) => hello.ephemeral,
			None => self.record.ephemeral,
		}
	}

	/// A scratch place project created by `wsync open` attach mode, or by the
	/// desktop app's `POST /places/attach`
	pub fn is_attach(&self) -> bool {
		self.ephemeral() || self.record.ephemeral || is_scratch_dir(&self.workspace())
	}

	pub fn candidate(&self) -> Candidate {
		Candidate {
			project: self.record.project.clone(),
			canonical: self.record.canonical_project.clone(),
			port: self.hello.as_ref().map_or(self.record.port, |hello| hello.port),
			place_ids: self.place_ids(),
			game_id: self.game_id(),
			place_key: self.place_key(),
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
/// The loopback range a daemon may take, mirroring the plugin's own scan
const PORT_SCAN_MIN: u16 = 7968;
const PORT_SCAN_MAX: u16 = 7990;

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

		let mut entries: Vec<DaemonEntry> = probes
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
						place_key: project.place_key.clone(),
						branch: project.branch.clone(),
					});

				DaemonEntry { record, hello, project }
			})
			.collect::<Vec<_>>();

		// A record is written by whoever started the daemon, and it can be
		// missing while the daemon is very much alive — a crash before it was
		// written, a cleanup that removed it, a daemon started by something
		// that keeps its records elsewhere. Listing only records makes this
		// command answer "nothing is running" to an agent looking straight at
		// a working Studio, so the port range is swept for the rest
		let known: Vec<u16> = entries.iter().map(|entry| entry.record.port).collect();
		let sweep: Vec<u16> = (PORT_SCAN_MIN..=PORT_SCAN_MAX)
			.filter(|port| !known.contains(port))
			.collect();
		let found: Vec<_> = sweep
			.into_iter()
			.map(|port| (port, scope.spawn(move || probe(port))))
			.collect();

		for (port, handle) in found {
			let Some(hello) = handle.join().ok().flatten() else {
				continue;
			};

			// The desktop app's broker answers on this range too, and it is
			// not a daemon: it serves no project and nothing can be addressed
			// through it
			if hello.canonical_project.trim().is_empty() {
				continue;
			}

			let project = Project::load(Path::new(&hello.canonical_project))
				.ok()
				.map(|project| ProjectClaim {
					name: project.name.clone(),
					game_id: project.game_id,
					place_ids: project.place_ids.clone(),
					place_key: project.place_key.clone(),
					branch: project.branch.clone(),
				});

			entries.push(DaemonEntry {
				record: RuntimeRecord::unrecorded(&hello.canonical_project, port, &hello.boot_id),
				hello: Some(hello),
				project,
			});
		}

		entries
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

/// The prefix every scratch directory for an *unpublished* place carries.
/// A published place has a place id to be named by; an unpublished one has
/// only the key the desktop app minted for it, and the prefix is what keeps
/// the auto-delete rule as narrow for a key as it is for an id
pub const UNPUBLISHED_PREFIX: &str = "unpublished-";

/// Longest scratch directory name accepted. The key carries a slug and a hex
/// suffix; anything past this is not one of ours
const MAX_SCRATCH_NAME: usize = 96;

/// A directory name a scratch place project is allowed to have: a place id
/// (digits only), or `unpublished-<slug>-<hex>` — one path component of
/// `[a-z0-9-]`, no dots and no separators, so it can name nothing but a
/// direct child
pub fn is_scratch_name(name: &str) -> bool {
	if name.is_empty() || name.len() > MAX_SCRATCH_NAME {
		return false;
	}

	if name.chars().all(|character| character.is_ascii_digit()) {
		return true;
	}

	name.starts_with(UNPUBLISHED_PREFIX)
		&& name.len() > UNPUBLISHED_PREFIX.len()
		&& !name.ends_with('-')
		&& name
			.chars()
			.all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
}

/// Whether `dir` is a scratch place project this tool may delete.
///
/// The auto-delete is the only destructive thing place routing does, so the
/// rule is deliberately narrow: the directory must carry a scratch name — a
/// place id, or the `unpublished-` key an attach minted — and sit *directly*
/// inside one of the two scratch roots. A project the user pointed at with
/// `--into` never matches, and neither does anything else on disk
pub fn is_scratch_dir(dir: &Path) -> bool {
	let named_after_a_place = dir
		.file_name()
		.and_then(|name| name.to_str())
		.is_some_and(is_scratch_name);

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
pub fn write_scratch_project(
	dir: &Path,
	place_id: Option<u64>,
	game_id: Option<u64>,
	place_key: Option<&str>,
	name: Option<&str>,
) -> Result<PathBuf> {
	if place_id.is_none() && place_key.is_none() {
		bail!("A scratch place project needs a place id or a place key to be addressed by");
	}

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
		"name": name.filter(|n| !n.trim().is_empty()).map(str::to_owned).unwrap_or_else(|| scratch_project_name(place_id, place_key)),
		"scope": "code",
		"initialSync": "keep",
		"servePlaceIds": place_id.map(|id| vec![id]).unwrap_or_default(),
		"gameId": game_id,
		"placeKey": place_key,
		"ignoreGlobs": ["**/.git/**", "**/.wsync-*/**"],
		"tree": Value::Object(tree),
	});

	// A null would fail the project parser's stricter readings, and an empty
	// `servePlaceIds` is a claim on nothing; drop each key that has no value
	let mut project = project;
	let fields = project.as_object_mut().expect("object");

	if game_id.is_none() {
		fields.remove("gameId");
	}

	if place_key.is_none() {
		fields.remove("placeKey");
	}

	if place_id.is_none() {
		fields.remove("servePlaceIds");
	}

	fs::write(&project_path, format!("{}\n", serde_json::to_string_pretty(&project)?))
		.with_context(|| format!("Failed to write {}", project_path.to_string()))?;

	Ok(project_path)
}

/// What a scratch project calls itself. A published place keeps the
/// `place-<id>` name `wsync open` has always written; an unpublished one is
/// named after the key it is reached by, which is the only name it has
pub fn scratch_project_name(place_id: Option<u64>, place_key: Option<&str>) -> String {
	match (place_id, place_key) {
		(Some(id), _) => format!("place-{id}"),
		(None, Some(key)) => key.to_owned(),
		(None, None) => String::from("place"),
	}
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

		let rows: Vec<[String; 9]> = entries.iter().map(row_cells).collect();
		let headers = [
			"PROJECT",
			"PORT",
			"GAME",
			"PLACES",
			"KEY",
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

fn render(cells: &[String; 9], widths: &[usize; 9]) -> String {
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

	if entry.ephemeral() {
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

fn row_cells(entry: &DaemonEntry) -> [String; 9] {
	[
		entry.name(),
		entry.record.port.to_string(),
		entry.game_id().map_or_else(|| String::from("-"), |id| id.to_string()),
		places_cell(entry),
		entry.place_key().unwrap_or_else(|| String::from("-")),
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
		"placeKey": entry.place_key(),
		"branch": entry.branch(),
		"managedBy": entry.record.managed_by,
		"live": entry.hello.is_some(),
		"plugin": entry.hello.as_ref().and_then(|hello| hello.plugin.as_ref()).map(|plugin| {
			json!({ "name": plugin.name, "transport": plugin.transport })
		}),
		"attach": entry.is_attach(),
		"ephemeral": entry.ephemeral(),
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
			place_key: None,
			branch: branch.map(str::to_owned),
			source,
		}
	}

	fn keyed(project: &str, port: u16, key: &str) -> Candidate {
		Candidate {
			place_key: Some(key.to_owned()),
			game_id: None,
			place_ids: Vec::new(),
			..candidate(project, port, &[], None, ClaimSource::Live)
		}
	}

	fn id(place: u64) -> Selector {
		Selector::Id(place)
	}

	fn key(place: &str) -> Selector {
		Selector::Key(place.to_owned())
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

		assert_eq!(resolve(&pool, &id(222), None).unwrap().port, 7970);
	}

	#[test]
	fn no_claim_refuses_and_says_how_to_get_one() {
		let pool = vec![main_daemon()];
		let error = resolve(&pool, &id(42), None).unwrap_err().to_string();

		assert!(error.contains("No WSync daemon claims place 42"), "{error}");
		assert!(error.contains("wsync open 42"), "{error}");
	}

	#[test]
	fn main_wins_over_a_branch_serving_the_same_place() {
		let pool = vec![branch_daemon(), main_daemon()];

		assert_eq!(resolve(&pool, &id(111), None).unwrap().port, 7970);
	}

	#[test]
	fn branch_selects_that_branch() {
		let pool = vec![main_daemon(), branch_daemon()];

		assert_eq!(resolve(&pool, &id(111), Some("ui-pass")).unwrap().port, 7971);
		assert_eq!(resolve(&pool, &id(111), Some("main")).unwrap().port, 7970);
	}

	#[test]
	fn an_unknown_branch_refuses_instead_of_falling_back() {
		let pool = vec![main_daemon(), branch_daemon()];
		let error = resolve(&pool, &id(111), Some("nope")).unwrap_err().to_string();

		assert!(error.contains("is on branch `nope`"), "{error}");
		assert!(error.contains("ui-pass"), "{error}");
	}

	#[test]
	fn two_branches_on_one_place_are_ambiguous() {
		let pool = vec![
			branch_daemon(),
			candidate("/other/p.json", 7973, &[111], Some("fx"), ClaimSource::Live),
		];
		let error = resolve(&pool, &id(111), None).unwrap_err().to_string();

		assert!(error.contains("served by 2 daemons"), "{error}");
		assert!(error.contains("--branch"), "{error}");
	}

	#[test]
	fn a_live_daemon_beats_a_project_file_claim() {
		let pool = vec![
			candidate("/dead/p.json", 7974, &[111], None, ClaimSource::ProjectFile),
			main_daemon(),
		];

		assert_eq!(resolve(&pool, &id(111), None).unwrap().port, 7970);
	}

	#[test]
	fn a_project_file_claim_still_resolves_when_it_is_the_only_one() {
		let pool = vec![candidate("/dead/p.json", 7974, &[111], None, ClaimSource::ProjectFile)];
		let resolved = resolve(&pool, &id(111), None).unwrap();

		assert_eq!(resolved.port, 7974);
		assert!(!resolved.is_live());
	}

	#[test]
	fn two_mains_on_one_place_are_ambiguous() {
		let pool = vec![
			main_daemon(),
			candidate("/twin/p.json", 7975, &[111], None, ClaimSource::Live),
		];
		let error = resolve(&pool, &id(111), None).unwrap_err().to_string();

		assert!(error.contains("served by 2 daemons"), "{error}");
	}

	#[test]
	fn a_key_resolves_the_daemon_that_claims_it() {
		let pool = vec![main_daemon(), keyed("/scratch/p.json", 7979, "my-level-ab12cd34")];
		let resolved = resolve(&pool, &key("my-level-ab12cd34"), None).unwrap();

		assert_eq!(resolved.port, 7979);
		// ...and an id never reaches a keyed daemon, nor a key an id one
		assert!(resolve(&pool, &key("something-else"), None).is_err());
		assert!(resolve(&pool, &id(111), None).unwrap().port == 7970);
	}

	#[test]
	fn an_unclaimed_key_says_so_without_offering_a_launch() {
		let pool = vec![main_daemon()];
		let error = resolve(&pool, &key("ghost-00000000"), None).unwrap_err().to_string();

		assert!(error.contains("place key `ghost-00000000`"), "{error}");
		assert!(!error.contains("wsync open ghost"), "{error}");
	}

	#[test]
	fn a_selector_reads_digits_as_an_id_and_everything_else_as_a_key() {
		assert_eq!(Selector::parse("128374926"), Selector::Id(128_374_926));
		assert_eq!(Selector::parse("  42 "), Selector::Id(42));
		assert_eq!(Selector::parse("my-level-ab12cd34"), Selector::Key(String::from("my-level-ab12cd34")));
		// Too long to be a u64: a key, not a silently truncated id
		assert_eq!(
			Selector::parse("99999999999999999999999"),
			Selector::Key(String::from("99999999999999999999999"))
		);
	}

	#[test]
	fn a_scratch_name_is_a_place_id_or_an_unpublished_key() {
		assert!(is_scratch_name("1234567"));
		assert!(is_scratch_name("unpublished-my-level-ab12cd34"));
		assert!(!is_scratch_name("unpublished-"));
		assert!(!is_scratch_name("unpublished-x-"));
		assert!(!is_scratch_name("unpublished-My-Level"));
		assert!(!is_scratch_name("unpublished-a/b"));
		assert!(!is_scratch_name("unpublished-a.b"));
		assert!(!is_scratch_name(".."));
		assert!(!is_scratch_name("my-level-ab12cd34"));
		assert!(!is_scratch_name(""));
	}

	#[test]
	fn an_unpublished_scratch_project_claims_a_key_and_no_ids() {
		let dir = tempfile::tempdir().unwrap();
		let path = write_scratch_project(dir.path(), None, None, Some("my-level-ab12cd34"), None).unwrap();
		let project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();

		assert_eq!(project["name"], json!("my-level-ab12cd34"));
		assert_eq!(project["placeKey"], json!("my-level-ab12cd34"));
		assert_eq!(project["initialSync"], json!("keep"));
		assert!(project.get("servePlaceIds").is_none());
		assert!(project.get("gameId").is_none());

		let loaded = Project::load(&path).unwrap();

		assert!(loaded.is_place());
		assert!(loaded.place_ids.is_empty());
		assert_eq!(loaded.place_key.as_deref(), Some("my-level-ab12cd34"));
	}

	#[test]
	fn a_scratch_project_needs_something_to_be_addressed_by() {
		let dir = tempfile::tempdir().unwrap();

		assert!(write_scratch_project(dir.path(), None, None, None, None).is_err());
	}

	#[test]
	fn a_scratch_directory_is_recognized_only_under_a_scratch_root() {
		let root = scratch_root().unwrap();

		assert!(is_scratch_dir(&root.join("1234567")));
		assert!(is_scratch_dir(&root.join("unpublished-my-level-ab12cd34")));
		assert!(!is_scratch_dir(&root.join("not-a-place")));
		assert!(!is_scratch_dir(&root.join("1234567").join("src")));
		assert!(!is_scratch_dir(Path::new("/tmp/1234567")));
	}

	#[test]
	fn a_scratch_project_keeps_both_sides_where_they_are() {
		let dir = tempfile::tempdir().unwrap();
		let path = write_scratch_project(dir.path(), Some(1234567), Some(42), None, None).unwrap();
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
		let path = write_scratch_project(dir.path(), Some(99), None, None, None).unwrap();
		let project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();

		assert!(project.get("gameId").is_none());
	}
}
