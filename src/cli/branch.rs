//! `wsync branch` — parallel places (docs/branches.md).
//!
//! A branch is four things and nothing else: a git worktree, a WSync daemon
//! on that worktree, a Studio open on an immutable clone of the place, and a
//! journal of what that Studio changed. This module owns their lifecycle
//! (`new`, `ls`, `up`, `rm`) and the merge driver (`finish`, `pick`).
//!
//! Everything that touches the place goes through the two daemons: main's
//! plugin takes the checkpoint (`branch_checkpoint`) and replays the branch's
//! journal (`journal_replay`); the branch's plugin adopts the same numbering
//! (`branch_adopt`). The CLI never diffs a tree and never assembles a merge —
//! it moves ids, blobs and decisions between the two daemons and drives git
//! around them.
//!
//! The plugin ops are the ones `plugin/src/Remote/Handlers/Branch.luau`
//! serves: `branch_checkpoint {name}` → `{checkpoint, count, blobs, blobIds,
//! shape}` where each blob is `{artifactId, bytes, sha256}`; `branch_adopt
//! {checkpoint, count, shapeBlob?}` → `{ok, count, matched?, extras?,
//! missing?}`, which answers `ok: false` on a refusal instead of failing; the blob pump `branch_blob_open
//! {artifactId}` → `{bytes, sha256}` / `branch_blob_read` / `branch_blob_close`;
//! `journal_flush`; and `journal_replay {checkpoint, events, blobs?,
//! decisions?}` → `{applied, skipped, conflicts}`.
//!
//! Two shapes are easy to get wrong. A replay's `blobs` travel INLINE as
//! `{artifactId: <base64 rbxm>}`, because main's plugin has no way to reach
//! the branch daemon; and its `decisions` are indexed by event number,
//! `{n: "ours"|"theirs"}`, not the ordered list `decisions.json` keeps.
//!
//! Still unverified here: the daemon routes `POST /journal/mark`,
//! `GET /journal?checkpoint=`, `POST /sync/pause` and `POST /sync/resume`.

use anyhow::{bail, Context, Result};
use base64::Engine;
use clap::{Parser, Subcommand};
use colored::Colorize;
use rbx_dom_weak::{
	types::{Attributes, Ref, Variant},
	InstanceBuilder, WeakDom,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
	collections::HashSet,
	env, fs,
	io::{BufRead, BufReader, BufWriter, Write},
	path::{Path, PathBuf},
	process::{self, Command, Stdio},
	sync::mpsc,
	thread,
	time::{Duration, Instant},
};

use crate::{
	cli::client::{print_json, truncate, Client, Target, Targeting},
	ext::PathExt,
	wsync_info, wsync_warn,
};

/// Everything a branch owns lives under here, beside the main checkout
const BRANCHES_DIR: &str = ".wsync-branches";

/// The one instance WSync adds to a cloned place, under `ServerStorage`
const MARKER_NAME: &str = "WSyncBranch";

/// Number attribute on that marker carrying main's universe id, because a
/// clone opened from an `.rbxl` reports `game.GameId == 0`
const GAME_ID_ATTRIBUTE: &str = "GameId";

/// Number attribute on the marker naming the branch daemon's port, so the
/// plugin's discovery tries it before it ranks anything
const DAEMON_PORT_ATTRIBUTE: &str = "DaemonPort";

/// Read part size for a blob pull: the plugin's `BLOB_READ_MAX_LEN` ceiling,
/// eight times the chunk the clipboard pump uses. Ten services at 64 KiB a
/// part was the bulk of a `branch new`
const BLOB_PART_BYTES: u64 = 512 * 1024;

/// The compact DFS shape of the numbered tree, beside the branch record.
/// Studio adds instances of its own when it opens a place file, so a clone
/// never numbers to exactly main's count; `branch_adopt` aligns against this
/// by name and class and skips what Studio put there
const SHAPE_FILE: &str = "checkpoint.shape.json";

/// Live branch Studios allowed at once: past Roblox's window cap a launch
/// succeeds into a windowless stub, so the budget is a real guardrail
const DEFAULT_BUDGET: usize = 3;

/// Exit code for "the merge stopped on conflicts you have to decide"
const CONFLICT_EXIT: i32 = 3;

/// Checkpoint and replay are whole-tree operations; they outlast the 5 s
/// default remote-op deadline the way `copy` does
const HEAVY_TIMEOUT_MS: u64 = 120_000;

/// How long `new`/`up` wait for a freshly launched Studio's plugin to connect
const PLUGIN_WAIT: Duration = Duration::from_secs(180);

/// How long a spawned daemon has to print its `--raw` ready line
const DAEMON_READY: Duration = Duration::from_secs(30);

/// Work on a place in parallel with main: worktree, daemon, clone and Studio
#[derive(Parser)]
pub struct Branch {
	#[command(subcommand)]
	command: Command_,
}

#[derive(Subcommand)]
enum Command_ {
	New(New),
	Ls(Ls),
	Up(Up),
	Rm(Rm),
	Finish(Finish),
	Pick(Pick),
}

impl Branch {
	pub fn main(self) -> Result<()> {
		match self.command {
			Command_::New(command) => command.main(),
			Command_::Ls(command) => command.main(),
			Command_::Up(command) => command.main(),
			Command_::Rm(command) => command.main(),
			Command_::Finish(command) => command.main(),
			Command_::Pick(command) => command.main(),
		}
	}
}

// ---------------------------------------------------------------------------
// The on-disk record
// ---------------------------------------------------------------------------

/// Who holds the branch. The creating process is recorded; a later command
/// refuses while that process is still alive and is not this one, and adopts
/// the branch when it is gone. `--steal` overrides both
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct Owner {
	pid: u32,
	image: String,
	started_at: String,
}

impl Owner {
	fn here() -> Self {
		Self {
			pid: process::id(),
			image: env::current_exe()
				.ok()
				.map(|path| path.get_name().to_owned())
				.unwrap_or_else(|| "wsync".to_owned()),
			started_at: now(),
		}
	}
}

/// `.wsync-branches/<name>/branch.json` — the whole branch, described
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct Record {
	name: String,
	checkpoint: String,
	count: u64,
	worktree: String,
	git_branch: String,
	clone: String,
	synthetic_place_id: u64,
	owner: Owner,
	created_at: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	shape_blob: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	daemon_port: Option<u16>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	studio_pid: Option<u32>,
}

impl Record {
	fn read(root: &Path) -> Result<Self> {
		let path = root.join("branch.json");
		let text = fs::read_to_string(&path)
			.with_context(|| format!("No branch record at {} — run `wsync branch ls`", path.to_string()))?;

		serde_json::from_str(&text).with_context(|| format!("The branch record {} is not valid JSON", path.to_string()))
	}

	fn write(&self, root: &Path) -> Result<()> {
		let path = root.join("branch.json");

		fs::create_dir_all(root).with_context(|| format!("Failed to create {}", root.to_string()))?;
		fs::write(&path, format!("{:#}\n", json!(self)))
			.with_context(|| format!("Failed to write {}", path.to_string()))
	}

	fn worktree(&self) -> PathBuf {
		PathBuf::from(&self.worktree)
	}

	/// The worktree's own project file: the main project file's name, inside
	/// the worktree (a git worktree carries the same tracked layout)
	fn project(&self, main_project: &Path) -> PathBuf {
		self.worktree().join(main_project.get_name())
	}

	fn targeting(&self, main_project: &Path) -> Targeting {
		Targeting {
			project: Some(self.project(main_project)),
			port: self.daemon_port,
		}
	}
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

/// Create a branch: worktree, checkpoint, clone, daemon and Studio
#[derive(Parser)]
struct New {
	/// Branch name (also the git branch `wsync/<name>`)
	#[arg(value_name = "NAME")]
	name: String,

	#[command(flatten)]
	targeting: Targeting,

	/// Live branch Studios allowed at once (default 3)
	#[arg(long, value_name = "COUNT")]
	budget: Option<usize>,

	/// Print the branch record as one machine-readable JSON line
	#[arg(long)]
	raw: bool,
}

/// List every branch beside this checkout with its daemon, Studio and owner
#[derive(Parser)]
struct Ls {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// Reopen a closed branch: daemon, Studio, adopt the numbering, replay its
/// own journal back into its own Studio
#[derive(Parser)]
struct Up {
	/// Branch name
	#[arg(value_name = "NAME")]
	name: String,

	#[command(flatten)]
	targeting: Targeting,

	/// Live branch Studios allowed at once (default 3)
	#[arg(long, value_name = "COUNT")]
	budget: Option<usize>,

	/// Take the branch from an owner that is still running
	#[arg(long)]
	steal: bool,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// Close a branch's Studio and daemon; `--purge` also removes its worktree,
/// clone and journal
#[derive(Parser)]
struct Rm {
	/// Branch name
	#[arg(value_name = "NAME")]
	name: String,

	#[command(flatten)]
	targeting: Targeting,

	/// Also remove the git worktree, the branch `wsync/<name>` and the
	/// branch directory
	#[arg(long)]
	purge: bool,

	/// Purge a worktree with uncommitted changes, and steal a live owner
	#[arg(long)]
	force: bool,

	/// Take the branch from an owner that is still running
	#[arg(long)]
	steal: bool,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// Merge a branch into main: git for the scripts, a Studio replay for the
/// instances
#[derive(Parser)]
struct Finish {
	/// Branch name
	#[arg(value_name = "NAME")]
	name: String,

	#[command(flatten)]
	targeting: Targeting,

	/// Also purge the worktree once the merge lands
	#[arg(long)]
	purge: bool,

	/// Take the branch from an owner that is still running
	#[arg(long)]
	steal: bool,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// Decide one conflict `finish` came back with
#[derive(Parser)]
struct Pick {
	/// Branch name
	#[arg(value_name = "NAME")]
	name: String,

	/// Conflict number, as `wsync branch finish` printed it
	#[arg(value_name = "N")]
	number: u64,

	/// `ours` keeps main's value, `theirs` takes the branch's
	#[arg(value_name = "SIDE")]
	side: String,

	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

// ---------------------------------------------------------------------------
// new
// ---------------------------------------------------------------------------

impl New {
	fn main(self) -> Result<()> {
		let name = validate_name(&self.name)?;
		let main = main_checkout(&self.targeting)?;
		let root = main.branches().join(&name);

		if root.exists() {
			bail!(
				"Branch `{name}` already exists at {} — reopen it with `{}` or remove it with `{}`",
				root.to_string(),
				format!("wsync branch up {name}").bold(),
				format!("wsync branch rm {name} --purge").bold()
			);
		}

		enforce_budget(&main.workspace, self.budget.unwrap_or(DEFAULT_BUDGET), &name)?;

		let raw = self.raw;
		let mut clock = Timings::new(raw);
		let clone_dir = root.join("clone");
		fs::create_dir_all(&clone_dir).with_context(|| format!("Failed to create {}", clone_dir.to_string()))?;
		exclude_branches_dir(&main.workspace);

		// (1) the worktree, from the main checkout's HEAD
		let worktree = root.join("tree");
		let git_branch = format!("wsync/{name}");
		git_out(
			&main.workspace,
			&["worktree", "add", &worktree.to_string(), "-b", &git_branch, "HEAD"],
		)?;
		clock.mark("worktree", "");

		// (2) the checkpoint and the clone, out of main's Studio
		let client = Client::connect(&self.targeting)?;
		let checkpoint = client
			.request_with_timeout("branch_checkpoint", json!({ "name": name }), HEAVY_TIMEOUT_MS)?
			.into_value(raw)?;

		let (id, count) = checkpoint_fields(&checkpoint)?;
		// `blobs` carries `{artifactId, bytes, sha256}` per service, which is
		// everything a pull needs; `blobIds` is the flat compat map
		let blobs = checkpoint
			.get("blobs")
			.and_then(Value::as_object)
			.or_else(|| checkpoint.get("blobIds").and_then(Value::as_object))
			.context("The plugin answered `branch_checkpoint` without a `blobs` map")?;

		// The clone is an `.rbxl`, so `game.GameId` boots as 0 and the plugin
		// reads the real universe id off the marker instead
		let game_id = main_game_id(&main.project, &client);

		clock.mark("checkpoint", &format!("{count} instances"));

		// Services are parented in the order this map iterates (lexicographic
		// by class name). Both plugins number the tree themselves, so the
		// order only has to be stable — `branch_adopt`'s shape alignment is what
		// proves the two numberings agree.
		//
		// The pulls run at once: the plugin pins each artifact separately and
		// the daemon serves requests concurrently, so ten services in sequence
		// was ten times a round trip for no reason
		let jobs: Vec<(String, Value, PathBuf)> = blobs
			.iter()
			.map(|(service, blob)| (service.clone(), blob.clone(), clone_dir.join(format!("{service}.rbxm"))))
			.collect();

		let pulled: Vec<Result<(String, u64)>> = thread::scope(|scope| {
			let handles: Vec<_> = jobs
				.iter()
				.map(|(service, blob, file)| {
					let client = &client;

					scope.spawn(move || {
						pull_checkpoint_blob(client, service, blob, file, raw)
							.with_context(|| format!("Failed to download the {service} blob"))
					})
				})
				.collect();

			handles
				.into_iter()
				.map(|handle| {
					handle
						.join()
						.unwrap_or_else(|_| bail!("a blob download thread panicked"))
				})
				.collect()
		});

		let mut services = Vec::with_capacity(jobs.len());
		let mut pulled_bytes = 0;

		for ((service, _, file), result) in jobs.iter().zip(pulled) {
			pulled_bytes += result?.1;
			services.push((service.clone(), file.clone()));
		}

		// The shape travels the same way the service blobs do. Without it
		// `branch_adopt` has only the count to go on, and Studio's own additions
		// guarantee the count will not match
		let shape_blob = match checkpoint.get("shape").filter(|shape| !shape.is_null()) {
			Some(shape) => Some(
				pull_checkpoint_blob(&client, "shape", shape, &root.join(SHAPE_FILE), raw)
					.context("Failed to download the checkpoint shape")?
					.0,
			),
			None => None,
		};

		clock.mark(
			"blobs",
			&format!("{} blobs, {}", services.len(), human_size(pulled_bytes)),
		);

		// (3) main remembers where this branch forked its journal
		client
			.post_endpoint("/journal/mark", &json!({ "checkpoint": id, "branch": name }))?
			.json("/journal/mark")?;
		clock.mark("mark", "");

		// (4) the worktree's project file
		let synthetic = synthetic_place_id(&name);
		let project = worktree.join(main.project.get_name());
		write_branch_project(&main.project, &project, &name, synthetic)?;

		// (5) the record, before anything is started: a failed start must
		// leave a branch that `rm` can find
		let place = root.join(format!("{name}.rbxl"));
		let mut record = Record {
			name: name.clone(),
			checkpoint: id,
			count,
			worktree: worktree.to_string(),
			git_branch,
			clone: place.to_string(),
			synthetic_place_id: synthetic,
			owner: Owner::here(),
			created_at: now(),
			shape_blob,
			daemon_port: None,
			studio_pid: None,
		};
		record.write(&root)?;
		clock.mark("project", "");

		// (6) the branch's own daemon. It only needs the worktree, so it starts
		// before the clone is assembled — and its port goes into the clone's
		// marker, which is how the branch plugin finds it without ranking
		let (port, daemon_pid) = spawn_daemon(&project)?;
		record.daemon_port = Some(port);
		record.write(&root)?;
		clock.mark("daemon", &format!("port {port}"));

		// (7) the clone, then Studio on it the moment the file is there
		assemble_place(&services, &name, game_id, Some(port), &place, raw)?;
		clock.mark("assemble", "");

		let studio_pid = launch_studio(&place)?;
		record.studio_pid = Some(studio_pid);
		record.write(&root)?;
		clock.mark("studio", &format!("PID {studio_pid}"));

		// (8) the branch plugin adopts main's numbering
		let branch = Client::connect(&record.targeting(&main.project))?;
		wait_for_plugin(&branch, PLUGIN_WAIT)?;
		clock.mark("plugin", "");

		adopt(&branch, &record, &root, raw)?;
		clock.mark("adopt", "");

		if raw {
			let mut line = json!(record);
			line["timings"] = clock.as_json();

			print_json(&line);

			return Ok(());
		}

		println!("{}", worktree.to_string());

		wsync_info!(
			"Branch {} is live in {:.1}s: daemon on port {} (PID {}), Studio PID {}, checkpoint {} instances",
			name.bold(),
			clock.total(),
			port.to_string().bold(),
			daemon_pid.to_string().bold(),
			studio_pid.to_string().bold(),
			record.count.to_string().bold()
		);

		Ok(())
	}
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

impl Ls {
	fn main(self) -> Result<()> {
		let main = main_checkout(&self.targeting)?;
		let mut rows = Vec::new();

		for root in branch_roots(&main.workspace) {
			let Ok(record) = Record::read(&root) else {
				continue;
			};

			let daemon = record.daemon_port.is_some_and(daemon_answers);
			let studio = record.studio_pid.is_some_and(process_alive);
			let dirty = git_out(&record.worktree(), &["status", "--porcelain"])
				.map(|out| !out.trim().is_empty())
				.unwrap_or(false);

			rows.push(json!({
				"name": record.name,
				"checkpoint": record.checkpoint,
				"count": record.count,
				"worktree": record.worktree,
				"gitBranch": record.git_branch,
				"daemon": daemon,
				"daemonPort": record.daemon_port,
				"studio": studio,
				"studioPid": record.studio_pid,
				"owner": record.owner,
				"ownerAlive": process_alive(record.owner.pid),
				"dirty": dirty,
			}));
		}

		if self.raw {
			print_json(&json!({ "ok": true, "branches": rows }));

			return Ok(());
		}

		if rows.is_empty() {
			wsync_info!(
				"No branches beside {} — create one with `{}`",
				main.workspace.to_string().bold(),
				"wsync branch new <name>".bold()
			);

			return Ok(());
		}

		println!(
			"{:<20} {:<8} {:<8} {:<7} {:<10} WORKTREE",
			"NAME", "DAEMON", "STUDIO", "DIRTY", "OWNER"
		);

		for row in &rows {
			println!(
				"{:<20} {:<8} {:<8} {:<7} {:<10} {}",
				row["name"].as_str().unwrap_or_default(),
				yes_no(row["daemon"].as_bool() == Some(true)),
				yes_no(row["studio"].as_bool() == Some(true)),
				yes_no(row["dirty"].as_bool() == Some(true)),
				row["owner"]["pid"].as_u64().unwrap_or(0),
				row["worktree"].as_str().unwrap_or_default()
			);
		}

		Ok(())
	}
}

// ---------------------------------------------------------------------------
// up
// ---------------------------------------------------------------------------

impl Up {
	fn main(self) -> Result<()> {
		let main = main_checkout(&self.targeting)?;
		let root = main.branches().join(validate_name(&self.name)?);
		let mut record = Record::read(&root)?;

		claim(&mut record, &root, self.steal)?;

		let project = record.project(&main.project);
		let restarted = restart(
			&mut record,
			&root,
			&project,
			self.budget.unwrap_or(DEFAULT_BUDGET),
			&main.workspace,
		)?;

		let branch = Client::connect(&record.targeting(&main.project))?;
		wait_for_plugin(&branch, PLUGIN_WAIT)?;

		// The clone is immutable, so the reopened Studio is the tree the
		// checkpoint numbered: adopt it, then replay the branch's own journal
		// back over it — a self-replay, which the plugin takes only under `own`
		adopt(&branch, &record, &root, self.raw)?;

		let journal = fetch_journal(&branch, &record.checkpoint)?;
		let decisions = read_decisions(&root);

		// A clone that has just been reopened has nothing pinned, so a blob the
		// daemon did not keep is reported rather than fatal
		let blobs = collect_blobs(&branch, &journal, false, self.raw)?;
		let replayed = replay(
			&branch,
			&record.checkpoint,
			&journal,
			&blobs,
			&decisions,
			true,
			self.raw,
		)?;
		let conflicts = conflicts_of(&replayed);

		if !conflicts.is_empty() {
			bail!(
				"Replaying branch `{}` into its own Studio reported {} conflict(s), which cannot happen against an \
				 untouched clone — the clone or the journal is not the one this checkpoint numbered",
				record.name,
				conflicts.len()
			);
		}

		if self.raw {
			print_json(&json!({
				"ok": true,
				"branch": record,
				"restarted": restarted,
				"applied": replayed.get("applied").cloned().unwrap_or(Value::Null),
			}));

			return Ok(());
		}

		println!("{}", record.worktree);

		wsync_info!(
			"Branch {} is up: {} event(s) replayed into its Studio",
			record.name.bold(),
			replayed
				.get("applied")
				.and_then(Value::as_u64)
				.unwrap_or(0)
				.to_string()
				.bold()
		);

		Ok(())
	}
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

impl Rm {
	fn main(self) -> Result<()> {
		let main = main_checkout(&self.targeting)?;
		let root = main.branches().join(validate_name(&self.name)?);
		let mut record = Record::read(&root)?;

		claim(&mut record, &root, self.steal || self.force)?;

		let removed = close(&mut record, &root, &main.project)?;

		if self.purge {
			purge(&record, &root, &main.workspace, self.force)?;
		}

		if self.raw {
			print_json(&json!({
				"ok": true,
				"name": record.name,
				"stoppedStudio": removed.0,
				"stoppedDaemon": removed.1,
				"purged": self.purge,
			}));

			return Ok(());
		}

		wsync_info!(
			"Branch {} closed{}",
			record.name.bold(),
			if self.purge {
				" and purged"
			} else {
				" — its worktree, clone and journal are kept"
			}
		);

		Ok(())
	}
}

// ---------------------------------------------------------------------------
// finish
// ---------------------------------------------------------------------------

impl Finish {
	fn main(self) -> Result<()> {
		let main = main_checkout(&self.targeting)?;
		let root = main.branches().join(validate_name(&self.name)?);
		let mut record = Record::read(&root)?;

		claim(&mut record, &root, self.steal)?;

		let worktree = record.worktree();

		// (1) the branch's own commit, scoped to its worktree
		git_out(&worktree, &["add", "-A"])?;

		let committed = if git_out(&worktree, &["diff", "--cached", "--quiet"]).is_err() {
			git_out(&worktree, &["commit", "-m", &format!("wsync branch {}", record.name)])?;
			true
		} else {
			false
		};

		// (2) the file merge, in the main checkout.
		//
		// Main's project file is never main's business to merge: the branch
		// rewrote its own copy in `new` and committed it, so the merge would
		// otherwise hand main the branch's name, `branch` and synthetic placeId
		let project_file = repo_relative(&main.workspace, &main.project);

		// A `finish` that parked conflicts left the merge in progress on purpose,
		// so a rerun after `branch pick` resumes it rather than starting another.
		// The parking commit is skipped too: its pathspec is precisely what git
		// rejects as a partial commit while `MERGE_HEAD` is set
		let resuming = merge_in_progress(&main.workspace)
			|| root.join("conflicts.json").exists()
			|| root.join("decisions.json").exists();

		let (parked, left_dirty) = if resuming {
			(Vec::new(), 0)
		} else {
			// Main is reliably dirty on the agent docs the daemon regenerates on
			// every plugin connect, and the branch committed its own regenerated
			// copies — so git refuses the merge before it starts unless that
			// overlap is committed first
			commit_blocking_work(&main.workspace, &record.git_branch, &record.name, &project_file)?
		};

		if !parked.is_empty() && !self.raw {
			wsync_info!(
				"Committed {} file(s) of main's own work that `{}` also touches{}",
				parked.len().to_string().bold(),
				record.git_branch.bold(),
				if left_dirty > 0 {
					format!("; {left_dirty} other changed file(s) left alone")
				} else {
					String::new()
				}
			);
		}

		let refusal = if resuming {
			None
		} else {
			let merge = git_try(
				&main.workspace,
				&["merge", "--no-ff", "--no-commit", &record.git_branch],
			)?;

			(!merge.0).then(|| {
				if merge.2.trim().is_empty() {
					merge.1.trim().to_owned()
				} else {
					merge.2.trim().to_owned()
				}
			})
		};

		let unmerged = unmerged_paths(&main.workspace);

		if !unmerged.is_empty() {
			print_paths(
				"git could not merge these files; resolve them in the worktree, then run `wsync branch finish` again",
				&unmerged,
				self.raw,
			);
			process::exit(CONFLICT_EXIT);
		}

		// A refusal with nothing left unmerged is git declining to start —
		// its own message is the diagnosis, so it is printed verbatim
		if let Some(message) = refusal {
			print_merge_refusal(&record.name, &record.git_branch, &message, self.raw);
			process::exit(CONFLICT_EXIT);
		}

		// `HEAD` is still main's pre-merge tip until the commit below — a merge
		// left in progress by an earlier run included — so main's own project
		// file comes back from there, into both the index and the working tree,
		// over whatever the branch brought in.
		//
		// A failure here is reported rather than fatal. `merge --no-commit`
		// has already staged the whole merge, so bailing now would leave main
		// mid-merge with a half-finished branch behind it — worse than a project
		// file the user can check
		let restored = git_try(&main.workspace, &["checkout", "HEAD", "--", &project_file])?;

		if !restored.0 {
			wsync_warn!(
				"Could not restore main's own {} from HEAD ({}) — check it before the next `{}`, because the merge may \
				 have given main the branch's name, `branch` and synthetic placeId",
				project_file.bold(),
				restored.2.trim(),
				"wsync branch new".bold()
			);
		}

		// (3) the instance replay, into main's Studio
		let branch = Client::connect(&record.targeting(&main.project))?;
		branch
			.request_with_timeout("journal_flush", json!({}), HEAVY_TIMEOUT_MS)?
			.into_value(self.raw)?;

		let journal = fetch_journal(&branch, &record.checkpoint)?;
		let decisions = read_decisions(&root);

		// Every `add` payload has to travel inline: main's plugin cannot reach
		// the branch daemon, so the blobs are drained from the branch's own
		// plugin here, while its Studio is still up
		let blobs = collect_blobs(&branch, &journal, true, self.raw)?;

		let client = Client::connect(&self.targeting)?;
		client.post_endpoint("/sync/pause", &json!({}))?.json("/sync/pause")?;

		let replayed = replay(
			&client,
			&record.checkpoint,
			&journal,
			&blobs,
			&decisions,
			false,
			self.raw,
		);

		let replayed = match replayed {
			Ok(replayed) => replayed,
			Err(error) => {
				resume(&client);

				return Err(error);
			}
		};

		let conflicts = conflicts_of(&replayed);

		if !conflicts.is_empty() {
			let path = root.join("conflicts.json");
			fs::write(
				&path,
				format!(
					"{:#}\n",
					json!({ "name": record.name, "checkpoint": record.checkpoint, "conflicts": conflicts })
				),
			)
			.with_context(|| format!("Failed to write {}", path.to_string()))?;

			resume(&client);
			print_conflicts(&record.name, &conflicts, &path, self.raw);
			process::exit(CONFLICT_EXIT);
		}

		// (4) the merge commit. The staging is scoped to what the merge actually
		// touched — never `git add -A`, which would sweep the branch directory in
		// — while the commit itself is whole, because git rejects a pathspec
		// while `MERGE_HEAD` is set
		let mut paths = git_out(&main.workspace, &["diff", "--name-only", "HEAD...MERGE_HEAD"])
			.unwrap_or_default()
			.lines()
			.map(str::to_owned)
			.filter(|line| !line.is_empty())
			.collect::<Vec<_>>();

		paths.extend(unmerged);
		paths.sort();
		paths.dedup();

		// Already staged from `HEAD` above; re-adding it would put the branch's
		// copy straight back
		paths.retain(|path| path != &project_file);

		if !paths.is_empty() {
			let mut args = vec!["add", "--"];
			args.extend(paths.iter().map(String::as_str));
			git_out(&main.workspace, &args)?;
		}

		// An earlier run (or a hand) may have committed this merge already:
		// nothing staged and no `MERGE_HEAD` is a finished merge, not a failure
		let pending =
			merge_in_progress(&main.workspace) || git_out(&main.workspace, &["diff", "--cached", "--quiet"]).is_err();

		if pending {
			git_out(
				&main.workspace,
				&["commit", "-m", &format!("Merge branch '{}'", record.git_branch)],
			)?;
		} else if !self.raw {
			wsync_info!("main's merge commit was already in place, so nothing further was committed");
		}

		resume(&client);

		// The replay lives in main's Studio and nowhere else: a place opened
		// from a file is saved by nobody, so the merged instances would be gone
		// the next time that Studio is relaunched
		let (saved, save_reason) = save_main(&client, self.raw);

		// The parked conflicts and the decisions belonged to this merge: a
		// finished branch must not still look like one mid-conflict, or the
		// next `finish` would take the resume path with nothing to resume
		fs::remove_file(root.join("conflicts.json")).ok();
		fs::remove_file(root.join("decisions.json")).ok();

		// (5) the branch is done
		close(&mut record, &root, &main.project)?;

		if self.purge {
			purge(&record, &root, &main.workspace, true)?;
		}

		if self.raw {
			print_json(&json!({
				"ok": true,
				"name": record.name,
				"committed": committed,
				"files": paths,
				"resumed": resuming,
				"mergeCommitted": pending,
				"parked": parked,
				"leftDirty": left_dirty,
				"projectRestored": project_file,
				"saved": saved,
				"saveReason": save_reason,
				"applied": replayed.get("applied").cloned().unwrap_or(Value::Null),
				"skipped": replayed.get("skipped").cloned().unwrap_or(Value::Null),
				"purged": self.purge,
			}));

			return Ok(());
		}

		wsync_info!(
			"Merged {} into main: {} file(s) by git, {} event(s) replayed into main's Studio, {}",
			record.name.bold(),
			paths.len().to_string().bold(),
			replayed
				.get("applied")
				.and_then(Value::as_u64)
				.unwrap_or(0)
				.to_string()
				.bold(),
			if saved {
				"place saved".to_owned()
			} else {
				format!("{} — save it in Studio", "NOT saved".bold())
			}
		);

		Ok(())
	}
}

// ---------------------------------------------------------------------------
// pick
// ---------------------------------------------------------------------------

impl Pick {
	fn main(self) -> Result<()> {
		let side = match self.side.as_str() {
			"ours" | "theirs" => self.side.clone(),
			other => bail!("A decision is `ours` (keep main's value) or `theirs` (take the branch's), not `{other}`"),
		};

		let main = main_checkout(&self.targeting)?;
		let root = main.branches().join(validate_name(&self.name)?);
		let record = Record::read(&root)?;

		let conflicts_path = root.join("conflicts.json");
		let parked: Value = fs::read_to_string(&conflicts_path)
			.ok()
			.and_then(|text| serde_json::from_str(&text).ok())
			.unwrap_or(Value::Null);

		let known = parked
			.get("conflicts")
			.and_then(Value::as_array)
			.map(|conflicts| {
				conflicts
					.iter()
					.filter_map(|conflict| conflict.get("n").and_then(Value::as_u64))
					.collect::<Vec<_>>()
			})
			.unwrap_or_default();

		if known.is_empty() {
			bail!(
				"No parked conflicts for branch `{}` — `{}` writes {} when a replay stops",
				record.name,
				format!("wsync branch finish {}", record.name).bold(),
				conflicts_path.to_string()
			);
		}

		if !known.contains(&self.number) {
			bail!(
				"Conflict {} is not one of the {} parked for branch `{}` (1-{})",
				self.number,
				known.len(),
				record.name,
				known.iter().max().copied().unwrap_or(0)
			);
		}

		let mut decisions = read_decisions(&root);
		decisions.retain(|decision| decision.get("n").and_then(Value::as_u64) != Some(self.number));
		decisions.push(json!({ "n": self.number, "side": side }));
		decisions.sort_by_key(|decision| decision.get("n").and_then(Value::as_u64).unwrap_or(0));

		let path = root.join("decisions.json");
		fs::write(
			&path,
			format!(
				"{:#}\n",
				json!({ "name": record.name, "checkpoint": record.checkpoint, "decisions": decisions })
			),
		)
		.with_context(|| format!("Failed to write {}", path.to_string()))?;

		if self.raw {
			print_json(&json!({ "ok": true, "name": record.name, "decisions": decisions }));

			return Ok(());
		}

		wsync_info!(
			"Conflict {} decided {}: {} of {} decided — run `{}` again to apply the rest",
			self.number.to_string().bold(),
			self.side.bold(),
			decisions.len().to_string().bold(),
			known.len().to_string().bold(),
			format!("wsync branch finish {}", record.name).bold()
		);

		Ok(())
	}
}

/// Wall clock per phase of a `branch new`. Each phase prints as it finishes,
/// dimmed and on stderr so stdout stays the worktree path a caller reads;
/// `--raw` prints nothing and carries the whole set as `timings`
struct Timings {
	raw: bool,
	started: Instant,
	phases: Vec<(String, u128)>,
}

impl Timings {
	fn new(raw: bool) -> Self {
		Self {
			raw,
			started: Instant::now(),
			phases: Vec::new(),
		}
	}

	fn mark(&mut self, phase: &str, detail: &str) {
		let elapsed = self.started.elapsed();

		self.started = Instant::now();

		if !self.raw {
			eprintln!(
				"{}",
				format!("  {phase:<11}{:>7.1}s  {detail}", elapsed.as_secs_f64())
					.trim_end()
					.dimmed()
			);
		}

		self.phases.push((phase.to_owned(), elapsed.as_millis()));
	}

	fn as_json(&self) -> Value {
		let mut timings: serde_json::Map<String, Value> = self
			.phases
			.iter()
			.map(|(phase, millis)| (phase.clone(), json!(millis)))
			.collect();

		timings.insert(
			"total".to_owned(),
			json!(self.phases.iter().map(|(_, millis)| millis).sum::<u128>()),
		);

		Value::Object(timings)
	}

	/// The sum, for the one human-readable line at the end
	fn total(&self) -> f64 {
		self.phases.iter().map(|(_, millis)| *millis as f64).sum::<f64>() / 1000.0
	}
}

// ---------------------------------------------------------------------------
// The main checkout
// ---------------------------------------------------------------------------

struct MainCheckout {
	project: PathBuf,
	workspace: PathBuf,
}

impl MainCheckout {
	fn branches(&self) -> PathBuf {
		self.workspace.join(BRANCHES_DIR)
	}
}

/// Resolves the project a branch command addresses and refuses to run inside
/// a branch worktree: a branch of a branch has no checkpoint to fork from
fn main_checkout(targeting: &Targeting) -> Result<MainCheckout> {
	let target = Target::resolve(targeting)?;

	if !target.project_exists {
		bail!(
			"No project file at {} — run `{}` or pass `{}`",
			target.project_path.to_string().bold(),
			"wsync init".bold(),
			"--project <path>".bold()
		);
	}

	let workspace = target.project_path.get_parent().to_path_buf();

	if let Some(root) = in_branch_worktree(&workspace) {
		bail!(
			"{} is inside branch `{}` — every `wsync branch` command runs from the main checkout",
			workspace.to_string().bold(),
			root.get_name().bold()
		);
	}

	Ok(MainCheckout {
		project: target.project_path,
		workspace,
	})
}

/// The marker every branch worktree carries: its `branch.json`, one level up,
/// under a `.wsync-branches` directory. Nothing is written into the worktree
/// itself, so `git add -A` there can never sweep WSync's own bookkeeping in
fn in_branch_worktree(workspace: &Path) -> Option<PathBuf> {
	workspace.ancestors().find_map(|ancestor| {
		let parent = ancestor.parent()?;

		(parent.get_name() == BRANCHES_DIR && ancestor.join("branch.json").is_file()).then(|| ancestor.to_owned())
	})
}

fn branch_roots(workspace: &Path) -> Vec<PathBuf> {
	let Ok(entries) = fs::read_dir(workspace.join(BRANCHES_DIR)) else {
		return Vec::new();
	};

	let mut roots: Vec<PathBuf> = entries
		.flatten()
		.map(|entry| entry.path())
		.filter(|path| path.join("branch.json").is_file())
		.collect();

	roots.sort();
	roots
}

fn enforce_budget(workspace: &Path, budget: usize, wanted: &str) -> Result<()> {
	let live: Vec<String> = branch_roots(workspace)
		.iter()
		.filter_map(|root| Record::read(root).ok())
		.filter(|record| record.studio_pid.is_some_and(process_alive))
		.map(|record| record.name)
		.collect();

	if live.len() >= budget {
		bail!(
			"{} branch Studios are already live ({}) and the budget is {budget} — close one with `{}` before opening \
			 `{wanted}` (past Roblox's window cap a launch succeeds into a windowless stub)",
			live.len(),
			live.join(", "),
			format!("wsync branch rm {}", live[0]).bold()
		);
	}

	Ok(())
}

/// Keeps `.wsync-branches/` out of the main checkout's status without
/// touching a tracked `.gitignore`
fn exclude_branches_dir(workspace: &Path) {
	let Ok(dir) = git_out(workspace, &["rev-parse", "--git-common-dir"]) else {
		return;
	};

	let exclude = workspace.join(dir.trim()).join("info").join("exclude");
	let entry = format!("/{BRANCHES_DIR}/");

	if let Ok(existing) = fs::read_to_string(&exclude) {
		if existing.lines().any(|line| line.trim() == entry) {
			return;
		}

		fs::write(&exclude, format!("{existing}\n{entry}\n")).ok();
	}
}

// ---------------------------------------------------------------------------
// Ownership
// ---------------------------------------------------------------------------

/// One owner per branch. A branch whose owner is gone is adopted; a branch
/// whose owner is still running is refused unless `--steal`
fn claim(record: &mut Record, root: &Path, steal: bool) -> Result<()> {
	let mine = process::id();

	if record.owner.pid != mine && process_alive(record.owner.pid) && !steal {
		bail!(
			"Branch `{}` is owned by {} (PID {}, since {}) — pass `{}` to take it",
			record.name,
			record.owner.image.bold(),
			record.owner.pid,
			record.owner.started_at,
			"--steal".bold()
		);
	}

	if record.owner.pid != mine {
		record.owner = Owner::here();
		record.write(root)?;
	}

	Ok(())
}

// ---------------------------------------------------------------------------
// Daemon, Studio and plugin
// ---------------------------------------------------------------------------

/// Starts `wsync daemon start --project <project> --raw` and reads its ready
/// line, the way the desktop lifecycle manager does. Returns the port the
/// daemon bound and the child's PID
fn spawn_daemon(project: &Path) -> Result<(u16, u32)> {
	let binary = env::current_exe().context("Failed to locate the running wsync executable")?;

	let mut child = Command::new(&binary)
		.args(["daemon", "start", "--project"])
		.arg(project)
		.arg("--raw")
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.with_context(|| format!("Failed to run {}", binary.to_string()))?;

	let pid = child.id();
	let stdout = child.stdout.take().context("The daemon child had no stdout")?;
	let (sender, receiver) = mpsc::channel();

	thread::spawn(move || {
		let mut line = String::new();

		sender
			.send(BufReader::new(stdout).read_line(&mut line).map(|_| line))
			.ok();
	});

	let ready = match receiver.recv_timeout(DAEMON_READY) {
		Ok(Ok(line)) => line,
		Ok(Err(error)) => {
			child.kill().ok();

			bail!("Failed to read the branch daemon's ready line: {error}");
		}
		Err(_) => {
			child.kill().ok();

			bail!(
				"The branch daemon did not report itself within {}s; nothing was left running",
				DAEMON_READY.as_secs()
			);
		}
	};

	let ready: Value = serde_json::from_str(ready.trim())
		.with_context(|| format!("The branch daemon's ready line is not JSON: {}", ready.trim()))?;

	let port = ready
		.get("port")
		.and_then(Value::as_u64)
		.context("The branch daemon's ready line carries no port")? as u16;

	Ok((port, pid))
}

/// Launches Studio on a place and keeps the PID.
///
/// UNVERIFIED against the parallel engine change: `studio::launch` still
/// returns `Result<()>` in this tree, so the spawn is done here to get a PID.
/// Once `studio::launch` answers `Result<u32>` this whole function collapses
/// into a call to it
fn launch_studio(place: &Path) -> Result<u32> {
	let studio = roblox_install::RobloxStudio::locate()
		.context("Failed to locate Roblox Studio")?
		.application_path()
		.to_owned();

	let child = Command::new(&studio)
		.arg(place)
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.with_context(|| format!("Failed to launch Roblox Studio from {}", studio.to_string()))?;

	Ok(child.id())
}

fn daemon_answers(port: u16) -> bool {
	reqwest::blocking::Client::builder()
		.no_proxy()
		.timeout(Duration::from_millis(1500))
		.redirect(reqwest::redirect::Policy::none())
		.build()
		.ok()
		.and_then(|http| http.get(format!("http://localhost:{port}/hello")).send().ok())
		.is_some_and(|response| response.status().is_success())
}

/// Waits for a plugin to connect to a daemon: a `ping` that comes back
/// `plugin_missing` means Studio is still loading the clone
fn wait_for_plugin(client: &Client, patience: Duration) -> Result<()> {
	let deadline = Instant::now() + patience;

	loop {
		match client.request("ping", json!({})) {
			Ok(envelope) if envelope.ok => return Ok(()),
			Ok(envelope) if !envelope.plugin_missing() => {
				return Err(envelope.into_value(false).unwrap_err());
			}
			_ => {}
		}

		if Instant::now() >= deadline {
			bail!(
				"No Studio plugin connected to the branch daemon on port {} within {}s — check that the clone opened \
				 and that the plugin is installed (`wsync plugin`)",
				client.port(),
				patience.as_secs()
			);
		}

		thread::sleep(Duration::from_secs(2));
	}
}

/// `branch_adopt` — the branch's plugin takes main's numbering, aligning
/// against the checkpoint shape by name and class. The counts are NOT
/// expected to match: Studio adds instances of its own as it opens a place
/// file, so the clone always numbers a little higher than main did
fn adopt(client: &Client, record: &Record, root: &Path, raw: bool) -> Result<()> {
	// The plugin asks its own daemon for the shape, so it has to be there
	// first — and a daemon that restarted since `new` serves a fresh store
	push_shape(record, root)?;

	let mut args = json!({ "checkpoint": record.checkpoint, "count": record.count });

	if let Some(shape) = &record.shape_blob {
		args["shapeBlob"] = json!(shape);
	}

	let adopted = client
		.request_with_timeout("branch_adopt", args, HEAVY_TIMEOUT_MS)?
		.into_value(raw)?;

	// A refusal is data, not an error envelope: the plugin reports what it
	// could and could not line up
	if adopted.get("ok").and_then(Value::as_bool) == Some(true) {
		return Ok(());
	}

	let count = adopted.get("count").and_then(Value::as_u64).unwrap_or_default();
	let matched = adopted.get("matched").cloned().unwrap_or(Value::Null);
	let shape_count = adopted.get("shapeCount").cloned().unwrap_or(Value::Null);
	let extras = adopted.get("extras").cloned().unwrap_or(Value::Null);
	let missing = adopted.get("missing").cloned().unwrap_or(Value::Null);

	if raw {
		print_json(&json!({
			"ok": false,
			"stage": "adopt",
			"branch": record.name,
			"checkpoint": record.checkpoint,
			"count": count,
			"expected": record.count,
			"shapeBlob": record.shape_blob,
			"matched": matched,
			"shapeCount": shape_count,
			"extras": extras,
			"missing": missing,
		}));
	}

	bail!(
		"Branch `{}` refused to adopt checkpoint {}: it numbered {count} instances against main's {}, matched {} of \
		 {}, extras {}, missing {}{}. Nothing was adopted — a numbering the two Studios do not share is a journal whose \
		 ids mean different instances on each side. Rebuild the branch (`{}`)",
		record.name,
		record.checkpoint,
		record.count,
		render_side(&matched),
		render_side(&shape_count),
		render_side(&extras),
		render_side(&missing),
		if record.shape_blob.is_none() {
			", and this checkpoint carried no shape to align against"
		} else {
			""
		},
		format!("wsync branch rm {} --purge", record.name).bold()
	)
}

/// A refusal's `matched`/`extras`/`missing`: a count, or a bounded list of
/// the paths the plugin could not line up
fn render_side(value: &Value) -> String {
	match value {
		Value::Null => "none reported".to_owned(),
		Value::Array(items) => {
			let shown: Vec<String> = items
				.iter()
				.take(8)
				.map(|item| match item {
					Value::String(text) => text.clone(),
					other => other.to_string(),
				})
				.collect();

			if shown.is_empty() {
				"0".to_owned()
			} else if items.len() > shown.len() {
				format!(
					"{} ({}, … {} more)",
					items.len(),
					shown.join(", "),
					items.len() - shown.len()
				)
			} else {
				format!("{} ({})", items.len(), shown.join(", "))
			}
		}
		other => other.to_string(),
	}
}

/// Puts the checkpoint shape on the branch's OWN daemon under the id main's
/// plugin gave it, so `branch_adopt` can fetch it from there. Idempotent, and
/// a no-op for a checkpoint that carried no shape
fn push_shape(record: &Record, root: &Path) -> Result<bool> {
	let (Some(shape), Some(port)) = (record.shape_blob.as_deref(), record.daemon_port) else {
		return Ok(false);
	};

	let file = root.join(SHAPE_FILE);
	let payload =
		fs::read(&file).with_context(|| format!("Failed to read the checkpoint shape {}", file.to_string()))?;

	upload_blob(port, shape, &payload)?;

	Ok(true)
}

/// `POST /journal/blob/<id>` — the raw bytes, verbatim. The daemon keeps
/// blobs beside the journal that names them precisely because the plugin
/// that made them is usually gone by the time something reads them
fn upload_blob(port: u16, id: &str, payload: &[u8]) -> Result<()> {
	let response = reqwest::blocking::Client::builder()
		.no_proxy()
		.timeout(Duration::from_secs(30))
		.redirect(reqwest::redirect::Policy::none())
		.build()?
		.post(format!("http://localhost:{port}/journal/blob/{id}"))
		.body(payload.to_vec())
		.send()
		.with_context(|| format!("Failed to upload blob {id} to the branch daemon on port {port}"))?;

	let status = response.status();

	if !status.is_success() {
		bail!(
			"The branch daemon on port {port} refused blob {id} (HTTP {status}): {}",
			truncate(&response.text().unwrap_or_default(), 200)
		);
	}

	Ok(())
}

/// Brings a branch's daemon and Studio back if either is gone. Returns what
/// had to be restarted
fn restart(record: &mut Record, root: &Path, project: &Path, budget: usize, workspace: &Path) -> Result<Vec<String>> {
	let mut restarted = Vec::new();

	if !record.daemon_port.is_some_and(daemon_answers) {
		let (port, _) = spawn_daemon(project)?;

		record.daemon_port = Some(port);
		record.write(root)?;
		restarted.push("daemon".to_owned());
	}

	if !record.studio_pid.is_some_and(process_alive) {
		enforce_budget(workspace, budget, &record.name)?;

		let pid = launch_studio(Path::new(&record.clone))?;

		record.studio_pid = Some(pid);
		record.write(root)?;
		restarted.push("studio".to_owned());
	}

	Ok(restarted)
}

/// Closes a branch: Studio first (it holds the clone), then its daemon
fn close(record: &mut Record, root: &Path, main_project: &Path) -> Result<(bool, bool)> {
	let studio = match record.studio_pid {
		Some(pid) if process_alive(pid) => {
			kill_tree(pid);
			true
		}
		_ => false,
	};

	let daemon = record.daemon_port.is_some_and(daemon_answers);

	if daemon {
		let binary = env::current_exe().context("Failed to locate the running wsync executable")?;
		let project = record.project(main_project);
		let stopped = Command::new(&binary)
			.args(["daemon", "stop", "--project"])
			.arg(&project)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status();

		if !stopped.map(|status| status.success()).unwrap_or(false) {
			wsync_warn!(
				"`wsync daemon stop --project {}` did not report success; the branch daemon may still be running",
				project.to_string()
			);
		}
	}

	record.studio_pid = None;
	record.daemon_port = None;
	record.write(root)?;

	Ok((studio, daemon))
}

/// Removes the worktree, the git branch and the branch directory
fn purge(record: &Record, root: &Path, workspace: &Path, force: bool) -> Result<()> {
	let worktree = record.worktree();

	if worktree.exists() {
		let dirty = git_out(&worktree, &["status", "--porcelain"])
			.map(|out| !out.trim().is_empty())
			.unwrap_or(false);

		if dirty && !force {
			bail!(
				"The worktree {} has uncommitted changes — commit them, or pass `{}` to discard them",
				worktree.to_string().bold(),
				"--force".bold()
			);
		}

		git_out(workspace, &["worktree", "remove", "--force", &worktree.to_string()])?;
	}

	git_try(workspace, &["branch", "-D", &record.git_branch])?;

	if root.exists() {
		fs::remove_dir_all(root).with_context(|| format!("Failed to remove {}", root.to_string()))?;
	}

	git_try(workspace, &["worktree", "prune"])?;

	Ok(())
}

// ---------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------

/// `GET /journal?checkpoint=` — the events (and the blob ids they reference)
/// a daemon has recorded since a checkpoint
fn fetch_journal(client: &Client, checkpoint: &str) -> Result<Value> {
	let path = format!("/journal?checkpoint={checkpoint}");

	client.get_endpoint(&path)?.json(&path).cloned()
}

fn read_decisions(root: &Path) -> Vec<Value> {
	fs::read_to_string(root.join("decisions.json"))
		.ok()
		.and_then(|text| serde_json::from_str::<Value>(&text).ok())
		.and_then(|stored| stored.get("decisions").and_then(Value::as_array).cloned())
		.unwrap_or_default()
}

/// `journal_replay` — the only way instances move between places. `blobs` is
/// `{artifactId: <base64 rbxm>}` and `decisions` is `{n: "ours"|"theirs"}`,
/// both as the plugin indexes them.
///
/// `own` marks a self-replay: a branch clone reapplying its own stream over
/// its own reopened tree. The plugin otherwise refuses `journal_replay` on a
/// branch, since a merge only ever lands on main; with `own` it accepts one
/// whose checkpoint is the clone's own adopted checkpoint
fn replay(
	client: &Client,
	checkpoint: &str,
	journal: &Value,
	blobs: &Value,
	decisions: &[Value],
	own: bool,
	raw: bool,
) -> Result<Value> {
	let events = journal.get("events").cloned().unwrap_or_else(|| json!([]));
	let mut args = json!({
		"checkpoint": checkpoint,
		"events": events,
		"blobs": blobs,
		"decisions": decisions_map(decisions),
	});

	if own {
		args["own"] = json!(true);
	}

	client
		.request_with_timeout("journal_replay", args, HEAVY_TIMEOUT_MS)?
		.into_value(raw)
}

/// decisions.json keeps an ordered list, because a human reads it; the plugin
/// indexes the same decisions by event number
fn decisions_map(decisions: &[Value]) -> Value {
	let mut map = serde_json::Map::new();

	for decision in decisions {
		if let (Some(n), Some(side)) = (
			decision.get("n").and_then(Value::as_u64),
			decision.get("side").and_then(Value::as_str),
		) {
			map.insert(n.to_string(), json!(side));
		}
	}

	Value::Object(map)
}

/// Gathers the `{artifactId: <base64 rbxm>}` a replay needs: whatever the
/// daemon already kept, plus every `add` event's blob drained from the plugin
/// that pinned it. `required` is false where the pin is legitimately gone —
/// a reopened clone starts with an empty blob store
fn collect_blobs(client: &Client, journal: &Value, required: bool, raw: bool) -> Result<Value> {
	let mut blobs = journal
		.get("blobs")
		.and_then(Value::as_object)
		.cloned()
		.unwrap_or_default();

	let mut wanted: Vec<String> = journal
		.get("events")
		.and_then(Value::as_array)
		.map(|events| {
			events
				.iter()
				.filter(|event| event.get("op").and_then(Value::as_str) == Some("add"))
				.filter_map(|event| event.get("blob").and_then(Value::as_str))
				.map(str::to_owned)
				.collect()
		})
		.unwrap_or_default();

	wanted.sort();
	wanted.dedup();

	for artifact in wanted {
		if blobs.get(&artifact).and_then(Value::as_str).is_some() {
			continue;
		}

		// Not closed: a replay that comes back with conflicts is run again
		// after `branch pick`, and it needs the same payloads
		match pull_blob(client, &artifact, raw) {
			Ok(payload) => {
				blobs.insert(
					artifact,
					json!(base64::engine::general_purpose::STANDARD.encode(payload)),
				);
			}
			Err(error) if !required => {
				wsync_warn!("Journal blob {artifact} is no longer pinned, so its `add` will be skipped: {error}");
			}
			Err(error) => {
				return Err(error.context(format!("Journal blob {artifact} could not be drained for the replay")));
			}
		}
	}

	Ok(Value::Object(blobs))
}

fn conflicts_of(replayed: &Value) -> Vec<Value> {
	replayed
		.get("conflicts")
		.and_then(Value::as_array)
		.cloned()
		.unwrap_or_default()
}

/// Asks main's Studio to save the place it just replayed into. A place
/// opened from a file has no other copy, so a merge nobody saves is a merge
/// that disappears on the next relaunch.
///
/// A refusal is a warning, never a failure: by this point the merge commit is
/// in and the instances are live. The plugin refuses on a branch clone, and
/// answers `PLUGIN_ERROR` where `DataModel.Save()` does not exist — a cloud or
/// Team Create place, which Studio saves through its own publish path
fn save_main(client: &Client, raw: bool) -> (bool, String) {
	let (reason, detail) = match client.request("save", json!({})) {
		Ok(envelope) if envelope.ok => return (true, String::new()),
		Ok(envelope) => (
			format!("{} [{}]", envelope.error_message(), envelope.error_code()),
			"A cloud or Team Create place is saved by Studio itself, not through the plugin",
		),
		Err(error) => (
			error.to_string(),
			"The merge is already committed and live in Studio; only the save did not happen",
		),
	};

	if !raw {
		wsync_warn!(
			"Main's Studio did not save the replayed merge: {reason}. {detail} — save the place there, or the \
			 replayed instances are lost when that Studio is relaunched"
		);
	}

	(false, reason)
}

/// `POST /sync/resume` — best effort, because it runs on the failure path
/// too and a paused daemon is worse than a noisy one
fn resume(client: &Client) {
	if let Err(error) = client
		.post_endpoint("/sync/resume", &json!({}))
		.and_then(|endpoint| endpoint.json("/sync/resume").cloned())
	{
		wsync_warn!("Failed to resume main's sync — run `wsync daemon restart --project` if it stays paused: {error}");
	}
}

fn print_conflicts(name: &str, conflicts: &[Value], path: &Path, raw: bool) {
	if raw {
		print_json(&json!({ "ok": false, "name": name, "conflicts": conflicts, "path": path.to_string() }));

		return;
	}

	for conflict in conflicts {
		println!(
			"{:>3}  {:<8} {:<40} ours={} theirs={}",
			conflict.get("n").and_then(Value::as_u64).unwrap_or(0),
			conflict.get("kind").and_then(Value::as_str).unwrap_or(""),
			conflict.get("path").and_then(Value::as_str).unwrap_or(""),
			conflict.get("ours").map(Value::to_string).unwrap_or_default(),
			conflict.get("theirs").map(Value::to_string).unwrap_or_default()
		);
	}

	println!();
	wsync_warn!(
		"{} conflict(s) parked in {} — decide each with `{}`, then run `{}` again",
		conflicts.len(),
		path.to_string().bold(),
		format!("wsync branch pick {name} <n> ours|theirs").bold(),
		format!("wsync branch finish {name}").bold()
	);
	std::io::stdout().flush().ok();
}

/// Commits exactly the files main has changed that the branch also touched,
/// except `exclude` — main's project file, which the merge is not allowed to
/// carry over and so is never worth committing on main's behalf.
/// so `git merge` stops refusing. The daemon regenerates the agent docs on
/// every plugin connect, which leaves main permanently dirty on files a
/// branch commits too; that overlap is the whole problem and the only thing
/// committed here. Never `git add -A` — unrelated work in main stays exactly
/// where it was. Returns what was committed and how many changed files were
/// left alone
fn commit_blocking_work(workspace: &Path, git_branch: &str, name: &str, exclude: &str) -> Result<(Vec<String>, usize)> {
	let touched: HashSet<String> = git_out(workspace, &["diff", "--name-only", &format!("HEAD...{git_branch}")])
		.unwrap_or_default()
		.lines()
		.filter(|line| !line.is_empty() && *line != exclude)
		.map(str::to_owned)
		.collect();

	let dirty = dirty_paths(workspace)?;
	let blocking: Vec<String> = dirty
		.iter()
		.filter(|path| touched.contains(path.as_str()))
		.cloned()
		.collect();

	let left_alone = dirty.len() - blocking.len();

	if blocking.is_empty() {
		return Ok((blocking, left_alone));
	}

	let mut staged = vec!["add", "--"];
	staged.extend(blocking.iter().map(String::as_str));
	git_out(workspace, &staged)?;

	// The pathspec keeps anything the user had staged already out of this
	// commit; `add` above is only there so an untracked path has something to
	// commit from
	let message = format!("wsync: main work in progress before merging {name}");
	let mut commit = vec!["commit", "-m", &message, "--"];
	commit.extend(blocking.iter().map(String::as_str));
	git_out(workspace, &commit)?;

	Ok((blocking, left_alone))
}

/// Is a merge in progress here? `MERGE_HEAD` exists only between a
/// `merge --no-commit` and the commit that finishes it, which is exactly the
/// window a `finish` that parked conflicts leaves main sitting in
fn merge_in_progress(workspace: &Path) -> bool {
	git_out(workspace, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
		.map(|head| !head.trim().is_empty())
		.unwrap_or(false)
}

/// The paths git left with conflict markers
fn unmerged_paths(workspace: &Path) -> Vec<String> {
	git_out(workspace, &["diff", "--name-only", "--diff-filter=U"])
		.unwrap_or_default()
		.lines()
		.filter(|line| !line.is_empty())
		.map(str::to_owned)
		.collect()
}

/// Every path `git status` reports as changed or untracked, renames counted
/// under their new name
fn dirty_paths(workspace: &Path) -> Result<Vec<String>> {
	let status = git_out(
		workspace,
		&[
			"-c",
			"core.quotepath=false",
			"status",
			"--porcelain",
			"--untracked-files=all",
		],
	)?;

	let mut paths: Vec<String> = status
		.lines()
		.filter(|line| line.len() > 3)
		.map(|line| {
			let path = &line[3..];

			// `R  old -> new`: the new name is the one a merge collides with
			path.rsplit(" -> ").next().unwrap_or(path).trim_matches('"').to_owned()
		})
		.collect();

	paths.sort();
	paths.dedup();

	Ok(paths)
}

/// git declined to start the merge. Whatever it said is the diagnosis — the
/// files it names are the ones to deal with — so it goes out verbatim
fn print_merge_refusal(name: &str, git_branch: &str, message: &str, raw: bool) {
	if raw {
		print_json(
			&json!({ "ok": false, "stage": "merge", "branch": name, "gitBranch": git_branch, "error": message }),
		);

		return;
	}

	println!("{message}");
	println!();
	wsync_warn!(
		"git refused to merge {} into main. Deal with what it names above, then run `{}` again — nothing was merged \
		 and the branch is untouched",
		git_branch.bold(),
		format!("wsync branch finish {name}").bold()
	);
	std::io::stdout().flush().ok();
}

fn print_paths(headline: &str, paths: &[String], raw: bool) {
	if raw {
		print_json(&json!({ "ok": false, "conflicts": paths }));

		return;
	}

	for path in paths {
		println!("{path}");
	}

	println!();
	wsync_warn!("{headline}");
	std::io::stdout().flush().ok();
}

// ---------------------------------------------------------------------------
// The clone
// ---------------------------------------------------------------------------

/// One checkpoint blob onto disk — a service's subtree, or the shape. The
/// descriptor `branch_checkpoint` returned already carries `bytes` and
/// `sha256`, so the `branch_blob_open` round trip is only made when a compat
/// `blobIds` entry left them out. A checkpoint blob IS closed afterwards: it
/// is pulled exactly once. Returns the artifact id and its size
fn pull_checkpoint_blob(client: &Client, label: &str, blob: &Value, out: &Path, raw: bool) -> Result<(String, u64)> {
	let (artifact, bytes, sha) = describe_blob(client, label, blob, raw)?;
	let payload = pull_parts(client, &artifact, bytes, &sha, raw);

	client
		.request("branch_blob_close", json!({ "artifactId": artifact }))
		.ok();

	fs::write(out, payload?).with_context(|| format!("Failed to write {}", out.to_string()))?;

	Ok((artifact, bytes))
}

/// The pump itself: `branch_blob_read` in 512 KiB parts, checked against the
/// size and digest the far side declared. A torn, reordered or truncated
/// transfer is a hard error, never a silently wrong artifact
fn pull_parts(client: &Client, artifact: &str, bytes: u64, sha: &str, raw: bool) -> Result<Vec<u8>> {
	let mut payload: Vec<u8> = Vec::with_capacity(bytes as usize);

	while (payload.len() as u64) < bytes {
		let part = client
			.request_with_timeout(
				"branch_blob_read",
				json!({ "artifactId": artifact, "offset": payload.len() as u64, "maxLen": BLOB_PART_BYTES }),
				HEAVY_TIMEOUT_MS,
			)?
			.into_value(raw)?;

		let total = part
			.get("totalBytes")
			.and_then(Value::as_u64)
			.with_context(|| format!("The plugin answered `branch_blob_read` for {artifact} without `totalBytes`"))?;

		// The size was fixed when the blob was pinned; a part that claims
		// otherwise means the far side re-captured under the same id
		if total != bytes {
			bail!("Blob {artifact} changed size mid-transfer (opened at {bytes} bytes, a part reports {total})");
		}

		let chunk = base64::engine::general_purpose::STANDARD
			.decode(part.get("data").and_then(Value::as_str).unwrap_or(""))
			.with_context(|| {
				format!("The plugin answered `branch_blob_read` for {artifact} with undecodable base64")
			})?;

		let eof = part.get("eof").and_then(Value::as_bool) == Some(true);

		if chunk.is_empty() {
			bail!(
				"The pull of blob {artifact} stalled: an empty part arrived at {} bytes",
				payload.len()
			);
		}

		if payload.len() as u64 + chunk.len() as u64 > bytes {
			bail!("The pull of blob {artifact} overran its declared {bytes} bytes");
		}

		payload.extend_from_slice(&chunk);

		if eof {
			break;
		}
	}

	if payload.len() as u64 != bytes {
		bail!(
			"Blob {artifact} ended early: {} of {bytes} bytes arrived",
			payload.len()
		);
	}

	let digest = format!("{:x}", Sha256::digest(&payload));

	if !digest.eq_ignore_ascii_case(sha) {
		bail!("Blob {artifact} arrived corrupted: SHA-256 {digest} does not match the declared {sha}");
	}

	Ok(payload)
}

/// A byte count the way a human reads one
fn human_size(bytes: u64) -> String {
	const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];

	let mut value = bytes as f64;
	let mut unit = 0;

	while value >= 1024.0 && unit < UNITS.len() - 1 {
		value /= 1024.0;
		unit += 1;
	}

	if unit == 0 {
		format!("{bytes} B")
	} else {
		format!("{value:.1} {}", UNITS[unit])
	}
}

/// Accepts either the `{artifactId, bytes, sha256}` descriptor or the compat
/// `blobIds` string, filling anything a descriptor left out from
/// `branch_blob_open`
fn describe_blob(client: &Client, label: &str, blob: &Value, raw: bool) -> Result<(String, u64, String)> {
	let artifact = match blob {
		Value::String(artifact) => artifact.clone(),
		blob => blob
			.get("artifactId")
			.and_then(Value::as_str)
			.with_context(|| format!("`branch_checkpoint` gave {label} no artifact id"))?
			.to_owned(),
	};

	let declared = (
		blob.get("bytes").and_then(Value::as_u64),
		blob.get("sha256").and_then(Value::as_str),
	);

	if let (Some(bytes), Some(sha)) = declared {
		if !sha.is_empty() {
			return Ok((artifact, bytes, sha.to_owned()));
		}
	}

	let (bytes, sha) = open_blob(client, &artifact, raw)?;

	Ok((artifact, bytes, sha))
}

/// `branch_blob_open` — the size and digest a pull sizes and verifies itself
/// against. It creates nothing; the blob was pinned when it was serialized
fn open_blob(client: &Client, artifact: &str, raw: bool) -> Result<(u64, String)> {
	let opened = client
		.request_with_timeout("branch_blob_open", json!({ "artifactId": artifact }), HEAVY_TIMEOUT_MS)?
		.into_value(raw)?;

	let bytes = opened
		.get("bytes")
		.and_then(Value::as_u64)
		.with_context(|| format!("The plugin opened blob {artifact} without a byte count"))?;
	let sha = opened.get("sha256").and_then(Value::as_str).unwrap_or_default();

	if sha.is_empty() {
		bail!("The plugin opened blob {artifact} without a SHA-256 digest");
	}

	Ok((bytes, sha.to_owned()))
}

/// One `add` event's blob into memory, out of the same store the checkpoint's
/// services came from and through the same read op
fn pull_blob(client: &Client, artifact: &str, raw: bool) -> Result<Vec<u8>> {
	let (bytes, sha) = open_blob(client, artifact, raw)?;

	pull_parts(client, artifact, bytes, &sha, raw)
}

/// Names the service, the blob, the reader and the real cause — the top-level
/// error printer shows `Display` only, so anyhow's context chain would
/// otherwise swallow exactly the part that says what went wrong
fn undecodable(service: &str, blob: &Path, error: &dyn std::error::Error, raw: bool) -> anyhow::Error {
	let bytes = fs::metadata(blob).map(|meta| meta.len()).unwrap_or_default();
	let cause = error_chain(error);
	let reader = rbx_binary_version();

	if raw {
		print_json(&json!({
			"ok": false,
			"stage": "assemble",
			"service": service,
			"blob": blob.to_string(),
			"bytes": bytes,
			"rbxBinary": reader,
			"error": cause,
			"kept": blob.get_parent().to_string(),
		}));
	}

	anyhow::anyhow!(
		"The {service} blob {} ({bytes} bytes) did not decode with rbx_binary {reader}: {cause}. The downloaded blobs \
		 are kept in {} for inspection; no clone was assembled, because a place missing a service is not the tree the \
		 checkpoint numbered",
		blob.to_string().bold(),
		blob.get_parent().to_string().bold()
	)
}

/// Every layer of an error's `source` chain on one line: a decode failure's
/// real cause is usually a link or two down
fn error_chain(error: &dyn std::error::Error) -> String {
	let mut text = error.to_string();
	let mut source = error.source();

	while let Some(cause) = source {
		text.push_str(&format!(": {cause}"));
		source = cause.source();
	}

	text
}

/// The `rbx_binary` this CLI reads and writes places with, taken from the
/// manifest at compile time so a decode failure names the exact reader
fn rbx_binary_version() -> &'static str {
	include_str!("../../Cargo.toml")
		.lines()
		.find_map(|line| line.trim().strip_prefix("rbx_binary = "))
		.map(|version| version.trim().trim_matches('"'))
		.unwrap_or("unknown")
}

/// The universe id the clone has to report. A place opened from an `.rbxl`
/// boots with `game.GameId == 0`, so the id travels on the marker and the
/// plugin calls `SetUniverseId` with it
fn main_game_id(project: &Path, client: &Client) -> Option<u64> {
	fs::read_to_string(project)
		.ok()
		.and_then(|text| serde_json::from_str::<Value>(&text).ok())
		.and_then(|project| project.get("gameId").and_then(Value::as_u64))
		.or(client.hello.game_id)
		.filter(|id| *id != 0)
}

/// Assembles the service blobs into one place file: a fresh DataModel, each
/// blob's roots reparented under its service, plus the branch marker.
///
/// A service that will not decode fails the whole assembly. There is no
/// partial clone to fall back to — a place missing a service is not the tree
/// the checkpoint numbered, so every id in the journal would be pointing at
/// the wrong instance. The downloaded blobs are left on disk for inspection
fn assemble_place(
	services: &[(String, PathBuf)],
	name: &str,
	game_id: Option<u64>,
	daemon_port: Option<u16>,
	out: &Path,
	raw: bool,
) -> Result<()> {
	let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
	let root = dom.root_ref();
	let mut storage = None;

	for (service, blob) in services {
		let reader = BufReader::new(
			fs::File::open(blob).with_context(|| format!("Failed to open the {service} blob {}", blob.to_string()))?,
		);
		let mut source = match rbx_binary::from_reader(reader) {
			Ok(source) => source,
			Err(error) => return Err(undecodable(service, blob, &error, raw)),
		};

		let service_ref = dom.insert(root, InstanceBuilder::new(service.as_str()).with_name(service.as_str()));

		if service == "ServerStorage" {
			storage = Some(service_ref);
		}

		let roots: Vec<Ref> = source.root().children().to_vec();

		for instance in roots {
			source.transfer(instance, &mut dom, service_ref);
		}
	}

	// The clone's one added instance, so the plugin knows it is a branch
	// before discovery runs
	let storage =
		storage.unwrap_or_else(|| dom.insert(root, InstanceBuilder::new("ServerStorage").with_name("ServerStorage")));

	let mut marker = InstanceBuilder::new("StringValue")
		.with_name(MARKER_NAME)
		.with_property("Value", name.to_owned());

	let mut attributes = Attributes::new();

	if let Some(game_id) = game_id {
		attributes.insert(GAME_ID_ATTRIBUTE.to_owned(), Variant::Float64(game_id as f64));
	}

	if let Some(port) = daemon_port {
		attributes.insert(DAEMON_PORT_ATTRIBUTE.to_owned(), Variant::Float64(f64::from(port)));
	}

	if attributes.iter().next().is_some() {
		marker = marker.with_property("Attributes", attributes);
	}

	dom.insert(storage, marker);

	let writer =
		BufWriter::new(fs::File::create(out).with_context(|| format!("Failed to create {}", out.to_string()))?);
	let roots: Vec<Ref> = dom.root().children().to_vec();

	rbx_binary::to_writer(writer, &dom, &roots)
		.with_context(|| format!("Failed to write the place {}", out.to_string()))
}

/// The worktree's project file: main's, renamed, disk-first, claiming the
/// synthetic place id so the branch Studio ranks its own daemon exact and
/// main's weak
fn write_branch_project(main: &Path, out: &Path, name: &str, synthetic: u64) -> Result<()> {
	let text =
		fs::read_to_string(main).with_context(|| format!("Failed to read the project file {}", main.to_string()))?;
	let mut project: Value = serde_json::from_str(&text)
		.with_context(|| format!("The project file {} is not valid JSON", main.to_string()))?;

	let fields = project
		.as_object_mut()
		.with_context(|| format!("The project file {} is not a JSON object", main.to_string()))?;

	let renamed = format!(
		"{} ({name})",
		strip_branch_suffix(fields.get("name").and_then(Value::as_str).unwrap_or("wsync"))
	);

	fields.insert("name".to_owned(), json!(renamed));
	// `keep`, not `disk`: the clone is a copy of what Studio already holds, so
	// a connect has nothing to reconcile. Disk-wins reads every place-only
	// Folder as Studio-only and deletes it (docs/branches.md)
	fields.insert("initialSync".to_owned(), json!("keep"));
	fields.insert("branch".to_owned(), json!(name));
	fields.insert("placeIds".to_owned(), json!([synthetic]));
	// The legacy spelling would otherwise sit beside the new one and win
	fields.remove("servePlaceIds");

	if let Some(parent) = out.parent() {
		fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.to_string()))?;
	}

	fs::write(out, format!("{project:#}\n")).with_context(|| format!("Failed to write {}", out.to_string()))
}

/// `wsync-lab (alpha) (gamma)` → `wsync-lab`. A branch's project file carries
/// main's name plus ` (<branch>)`; if one of those ever reaches main — the
/// merge used to carry it — the next branch must not compound it. A project
/// genuinely named `Game (v2)` pays for that, which is the cheaper mistake
fn strip_branch_suffix(name: &str) -> &str {
	let mut base = name.trim();

	while base.ends_with(')') {
		match base.rfind(" (") {
			Some(cut) => base = base[..cut].trim_end(),
			None => break,
		}
	}

	if base.is_empty() {
		name.trim()
	} else {
		base
	}
}

/// A path as git names it: relative to the checkout, forward slashes
fn repo_relative(workspace: &Path, path: &Path) -> String {
	path.strip_prefix(workspace)
		.map(|relative| relative.to_string().replace('\\', "/"))
		.unwrap_or_else(|_| path.get_name().to_owned())
}

/// FNV-1a over the UTF-8 name, masked to 53 bits — the same arithmetic the
/// plugin does, so both sides derive the identical synthetic place id
fn synthetic_place_id(name: &str) -> u64 {
	let mut hash: u64 = 0xcbf2_9ce4_8422_2325;

	for byte in name.as_bytes() {
		hash ^= u64::from(*byte);
		hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
	}

	hash & ((1u64 << 53) - 1)
}

// ---------------------------------------------------------------------------
// Small shared things
// ---------------------------------------------------------------------------

fn checkpoint_fields(checkpoint: &Value) -> Result<(String, u64)> {
	let id = checkpoint
		.get("checkpoint")
		.and_then(Value::as_str)
		.context("The plugin answered `branch_checkpoint` without a checkpoint id")?
		.to_owned();
	let count = checkpoint
		.get("count")
		.and_then(Value::as_u64)
		.context("The plugin answered `branch_checkpoint` without an instance count")?;

	Ok((id, count))
}

/// A branch name is a directory, a git ref and a Studio window title, so it
/// is kept to what all three accept without quoting
fn validate_name(name: &str) -> Result<String> {
	let trimmed = name.trim();

	if trimmed.is_empty() || trimmed.len() > 64 {
		bail!("A branch name is 1-64 characters; `{name}` is {}", trimmed.len());
	}

	if !trimmed
		.chars()
		.all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
		|| trimmed.starts_with('.')
	{
		bail!("A branch name holds letters, digits, `-`, `_` and `.` (not leading), so `{name}` cannot be one");
	}

	Ok(trimmed.to_owned())
}

fn now() -> String {
	chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn yes_no(value: bool) -> &'static str {
	if value {
		"yes"
	} else {
		"no"
	}
}

/// Runs git in a directory and fails the command on a non-zero exit
fn git_out(dir: &Path, args: &[&str]) -> Result<String> {
	let (ok, out, err) = git_try(dir, args)?;

	if !ok {
		bail!(
			"`git {}` failed in {}: {}",
			args.join(" "),
			dir.to_string(),
			if err.trim().is_empty() { out.trim() } else { err.trim() }
		);
	}

	Ok(out)
}

/// Runs git and hands back success, stdout and stderr — for the calls whose
/// failure is an outcome (`merge` stopping on conflicts, `branch -D` on a
/// branch that is already gone)
fn git_try(dir: &Path, args: &[&str]) -> Result<(bool, String, String)> {
	let output = Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(args)
		.stdin(Stdio::null())
		.output()
		.with_context(|| format!("Failed to run `git {}` in {}", args.join(" "), dir.to_string()))?;

	Ok((
		output.status.success(),
		String::from_utf8_lossy(&output.stdout).into_owned(),
		String::from_utf8_lossy(&output.stderr).into_owned(),
	))
}

/// Is this PID alive? Unlike [`crate::util::process_exists`] this asks about
/// any process, not just a wsync one — a branch's Studio is neither
fn process_alive(pid: u32) -> bool {
	if pid == 0 {
		return false;
	}

	#[cfg(not(target_os = "windows"))]
	{
		Command::new("kill")
			.arg("-0")
			.arg(pid.to_string())
			.output()
			.map(|output| output.status.success())
			.unwrap_or(false)
	}

	#[cfg(target_os = "windows")]
	{
		Command::new("TASKLIST")
			.arg("/NH")
			.args(["/FI", &format!("PID eq {pid}")])
			.output()
			.map(|output| {
				let listed = String::from_utf8_lossy(&output.stdout);

				listed
					.split_whitespace()
					.any(|field| field.trim_matches(',') == pid.to_string())
			})
			.unwrap_or(false)
	}
}

/// Kills a process and everything it started — Studio spawns helpers
fn kill_tree(pid: u32) {
	#[cfg(target_os = "windows")]
	{
		Command::new("taskkill")
			.args(["/PID", &pid.to_string(), "/T", "/F"])
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()
			.ok();
	}

	#[cfg(not(target_os = "windows"))]
	crate::util::kill_process(pid);
}
