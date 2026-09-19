//! Starting a daemon for a project without becoming one.
//!
//! `wsync daemon start` is deliberately a foreground command — the process
//! that runs it *is* the daemon. A command that only wants a daemon to exist
//! (today `wsync open`'s attach mode, docs/places.md) therefore has to
//! re-exec this binary detached and wait for the new process to say it is
//! serving.
//!
//! Readiness is the daemon's own runtime record plus a `GET /hello` on the
//! port that record names — never a line on a pipe. Nothing is piped on
//! purpose: a daemon holding this process's stdout keeps the pipe open long
//! after the command returns, so anything reading our output would wait for
//! an EOF that only arrives when the daemon dies.
//!
//! NOTE(dedupe): `src/cli/branch.rs` carries its own copy of this logic
//! (`spawn_daemon`/`detach`/`disinherit_std_handles`). That file is being
//! edited concurrently, so the copy was lifted rather than moved; the two
//! should be collapsed onto this module once branch.rs settles.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use serde_json::Value;
use std::{
	env,
	path::Path,
	process::{Command, Stdio},
	thread,
	time::{Duration, Instant},
};

use crate::{daemon, ext::PathExt};

/// How long a spawned daemon has to answer `/hello` before the spawn fails
pub const DAEMON_READY: Duration = Duration::from_secs(30);

/// How long each readiness probe waits
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// A daemon this process started, as its own runtime record describes it
pub struct Spawned {
	pub port: u16,
	pub pid: u32,
}

/// Starts `wsync daemon start --project <project>` fully detached and waits
/// for it to answer for that exact project and boot.
///
/// `extra` is appended to the argument list (`--ephemeral` for a scratch
/// place daemon). A child that exits before recording itself is reported
/// with the log to read, not retried.
pub fn daemon(project: &Path, extra: &[&str]) -> Result<Spawned> {
	let binary = env::current_exe().context("Failed to locate the running wsync executable")?;
	let canonical = daemon::canonical_project(project)?;
	let paths = daemon::runtime_paths(&daemon::state_dir(None)?, &canonical);

	// Whatever is recorded now belongs to a previous daemon for this project;
	// readiness is a record this spawn wrote, never that one
	let stale = daemon::read_record(&paths.record)
		.unwrap_or_default()
		.map(|record| record.boot_id);

	let mut command = Command::new(&binary);

	command
		.args(["daemon", "start", "--project"])
		.arg(project)
		.args(extra)
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null());

	detach(&mut command);

	let mut child = command
		.spawn()
		.with_context(|| format!("Failed to run {}", binary.to_string()))?;

	let deadline = Instant::now() + DAEMON_READY;

	loop {
		if let Ok(Some(record)) = daemon::read_record(&paths.record) {
			if Some(&record.boot_id) != stale.as_ref() {
				if let Some(hello) = probe_hello(record.port) {
					let serves = hello.get("canonicalProject").and_then(Value::as_str) == Some(canonical.as_str());
					let booted = hello.get("bootId").and_then(Value::as_str) == Some(record.boot_id.as_str());

					if serves && booted {
						return Ok(Spawned {
							port: record.port,
							pid: record.pid,
						});
					}
				}
			}
		}

		// A child that has already exited will never record itself
		if let Ok(Some(status)) = child.try_wait() {
			bail!(
				"The daemon exited before it started serving ({status}). Its own log has the reason: `{}`",
				format!("wsync daemon logs --project {}", project.to_string()).bold()
			);
		}

		if Instant::now() >= deadline {
			child.kill().ok();
			child.wait().ok();

			bail!(
				"The daemon did not start serving within {}s; it was stopped",
				DAEMON_READY.as_secs()
			);
		}

		thread::sleep(Duration::from_millis(250));
	}
}

fn probe_hello(port: u16) -> Option<Value> {
	reqwest::blocking::Client::builder()
		.no_proxy()
		.timeout(PROBE_TIMEOUT)
		.redirect(reqwest::redirect::Policy::none())
		.build()
		.ok()?
		.get(format!("http://localhost:{port}/hello"))
		.send()
		.ok()?
		.json()
		.ok()
}

/// Cuts a child loose from this process: no inherited console, and its own
/// process group, so a Ctrl-C aimed at the CLI does not reach it
#[allow(unused_variables)]
fn detach(command: &mut Command) {
	#[cfg(target_os = "windows")]
	{
		use std::os::windows::process::CommandExt;

		disinherit_std_handles();

		// DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
		command.creation_flags(0x0000_0008 | 0x0000_0200);
	}

	// On unix the child inherits this process's process group and controlling
	// terminal, so a Ctrl-C at the prompt SIGINTs the daemon we just started
	// and closing the terminal SIGHUPs it. A group of its own is the whole
	// point of the name `detach`, and Windows was the only side implementing
	// it: `CREATE_NEW_PROCESS_GROUP` there, `setpgid(0, 0)` here.
	#[cfg(unix)]
	{
		use std::os::unix::process::CommandExt;

		command.process_group(0);
	}
}

/// Stops this process's own standard handles from reaching any child.
///
/// Null stdio is not enough on Windows: `CreateProcessW` is called with
/// `bInheritHandles = TRUE` whenever any stdio is passed, which hands the
/// child every inheritable handle this process holds — including the pipe a
/// caller gave us as stdout. A detached daemon then holds a duplicate of
/// that write end and the caller never sees EOF
#[cfg(target_os = "windows")]
fn disinherit_std_handles() {
	use std::ffi::c_void;

	const STD_INPUT: u32 = -10i32 as u32;
	const STD_OUTPUT: u32 = -11i32 as u32;
	const STD_ERROR: u32 = -12i32 as u32;
	const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

	extern "system" {
		fn GetStdHandle(std_handle: u32) -> *mut c_void;
		fn SetHandleInformation(object: *mut c_void, mask: u32, flags: u32) -> i32;
	}

	for id in [STD_INPUT, STD_OUTPUT, STD_ERROR] {
		// SAFETY: both calls take a handle this process already owns and change
		// only its inheritance flag. Neither reads nor writes any memory, and a
		// failure on a redirected or closed handle is ignored on purpose
		unsafe {
			let handle = GetStdHandle(id);

			if !handle.is_null() && handle as isize != -1 {
				SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
			}
		}
	}
}
