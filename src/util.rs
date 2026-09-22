use anyhow::{Context, Result};
use colored::Colorize;
use directories::UserDirs;
use env_logger::WriteStyle;
use json_formatter::JsonFormatter;
use log::LevelFilter;
use rbx_dom_weak::types::Variant;
use rbx_reflection::{ClassTag, ReflectionDatabase};
use roblox_install::RobloxStudio;
use serde::Serialize;
use std::{env, path::PathBuf, process::Command};

use crate::{ext::ResultExt, Properties};

/// Returns the `.wsync` directory
pub fn get_wsync_dir() -> Result<PathBuf> {
	let user_dirs = UserDirs::new().context("Failed to get user directory")?;

	Ok(user_dirs.home_dir().join(".wsync"))
}

/// A workspace-relative path as the protocol spells it: relative to
/// `workspace_dir` and always forward-slashed.
///
/// Every surface that puts a disk path in JSON goes through here — `path`,
/// `meta` and `where`, the divergence set, the backlog. Windows' native
/// separator must not reach any of them: the same string is compared against
/// paths from other machines, fed back to the daemon by clients that built it
/// on a different platform, and rejected outright by
/// [`crate::server::backlog::safe_rel_path`], which treats a backslash as the
/// escape attempt it is on the platforms where it is not a separator.
/// Windows accepts forward slashes everywhere it accepts a path, so joining
/// one back onto `workspace_dir` still opens the file it names
pub fn relative_string(path: &std::path::Path, workspace_dir: &std::path::Path) -> String {
	let relative = path.strip_prefix(workspace_dir).unwrap_or(path);

	relative
		.components()
		.map(|component| component.as_os_str().to_string_lossy())
		.collect::<Vec<_>>()
		.join("/")
}

/// Returns the Git or local username of the current user
pub fn get_username() -> String {
	let mut git = Command::new("git");
	hide_console(git.arg("config").arg("user.name"));

	if let Ok(output) = git.output() {
		let username = String::from_utf8_lossy(&output.stdout).trim().to_owned();

		if !username.is_empty() {
			return username;
		}
	}

	whoami::username()
}

pub fn get_plugin_path() -> Result<PathBuf> {
	Ok(RobloxStudio::locate()?.plugins_path().join("WSync.rbxm"))
}

/// Checks if the given `class` is a service
pub fn is_service(class: &str) -> bool {
	let descriptor = get_reflection_database().classes.get(class);

	let has_tag = if let Some(descriptor) = descriptor {
		descriptor.tags.contains(&ClassTag::Service)
	} else {
		false
	};

	has_tag || class == "StarterPlayerScripts" || class == "StarterCharacterScripts"
}

/// Checks if the given `class` is a script
pub fn is_script(class: &str) -> bool {
	class == "Script" || class == "LocalScript" || class == "ModuleScript"
}

/// Windows `CREATE_NO_WINDOW` process-creation flag
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Keeps a console program this process starts from opening a console window.
///
/// On Windows a console program gets a console of its own whenever the process
/// starting it has none, and a console is a window. A detached daemon, the hub
/// and the desktop app have none, so every `TASKLIST` a daemon ran used to
/// flash a terminal on screen. Whatever a helper prints is captured or
/// discarded anyway, so it never needs one. A no-op on every other platform
#[allow(unused_variables)]
pub fn hide_console(command: &mut Command) {
	#[cfg(target_os = "windows")]
	{
		use std::os::windows::process::CommandExt;

		command.creation_flags(CREATE_NO_WINDOW);
	}
}

/// Whether this process has a console window to share with what it starts.
/// Always true off Windows, where the question does not come up
pub fn has_console() -> bool {
	#[cfg(target_os = "windows")]
	{
		extern "system" {
			fn GetConsoleWindow() -> *mut std::ffi::c_void;
		}

		// SAFETY: takes no arguments and only reports this process's own
		// console window, or null when it has none
		unsafe { !GetConsoleWindow().is_null() }
	}

	#[cfg(not(target_os = "windows"))]
	{
		true
	}
}

/// Kills the process with the given `pid`
pub fn kill_process(pid: u32) {
	#[cfg(not(target_os = "windows"))]
	{
		// Kill main process
		Command::new("kill").arg(pid.to_string()).output().ok();

		// Kill child processes
		Command::new("pkill").arg("-P").arg(pid.to_string()).output().ok();
	}

	// Kill both main and child processes
	#[cfg(target_os = "windows")]
	{
		let mut taskkill = Command::new("TASKKILL");
		hide_console(taskkill.arg("/F").arg("/T").args(["/PID", &pid.to_string()]));
		taskkill.output().ok();
	}
}

pub fn process_exists(pid: u32) -> bool {
	#[cfg(not(target_os = "windows"))]
	{
		if let Ok(output) = Command::new("kill").arg("-0").arg(pid.to_string()).output() {
			output.status.success()
		} else {
			false
		}
	}

	#[cfg(target_os = "windows")]
	{
		let mut tasklist = Command::new("TASKLIST");
		hide_console(tasklist.arg("/NH").args(["/FI", &format!("PID eq {}", pid)]));

		let output = tasklist.output();

		if let Ok(output) = output {
			String::from_utf8_lossy(&output.stdout).contains("wsync.exe")
		} else {
			false
		}
	}
}

/// Cuts a child loose from this process: no inherited console, and its own
/// process group, so a Ctrl-C aimed at the CLI does not reach it
#[allow(unused_variables)]
pub fn detach(command: &mut Command) {
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

/// Returns progress bar styling
pub fn get_progress_style() -> (String, String) {
	let mut template = match env_log_style() {
		WriteStyle::Always => "PROGRESS: ".magenta().bold().to_string(),
		_ => "PROGRESS: ".into(),
	};
	template.push_str("[{bar:40}] ({bytes}/{total_bytes})");

	(template, String::from("=>-"))
}

/// Returns the `RUST_VERBOSE` environment variable
pub fn env_verbosity() -> LevelFilter {
	let verbosity = env::var("RUST_VERBOSE").unwrap_or("ERROR".into());

	match verbosity.as_str() {
		"OFF" => LevelFilter::Off,
		"ERROR" => LevelFilter::Error,
		"WARN" => LevelFilter::Warn,
		"INFO" => LevelFilter::Info,
		"DEBUG" => LevelFilter::Debug,
		"TRACE" => LevelFilter::Trace,
		_ => LevelFilter::Error,
	}
}

/// Returns the `RUST_LOG_STYLE` environment variable
pub fn env_log_style() -> WriteStyle {
	let log_style = env::var("RUST_LOG_STYLE").unwrap_or("auto".into());

	match log_style.as_str() {
		"always" => WriteStyle::Always,
		"never" => WriteStyle::Never,
		_ => WriteStyle::Auto,
	}
}

/// Returns the `RUST_BACKTRACE` environment variable
pub fn env_backtrace() -> bool {
	let backtrace = env::var("RUST_BACKTRACE").unwrap_or("0".into());
	backtrace == "1"
}

/// Returns the `RUST_YES` environment variable
pub fn env_yes() -> bool {
	let yes = env::var("RUST_YES").unwrap_or("0".into());
	yes == "1"
}

/// Returns line of code count from snapshot's properties
pub fn count_loc_from_properties(properties: &Properties) -> usize {
	let mut loc = 0;

	for value in properties.values() {
		if let Variant::String(value) = value {
			loc += value.lines().count();
		}
	}

	loc
}

/// Serializes value to a pretty-printed and sorted JSON
pub fn serialize_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
	let formatter = JsonFormatter::new()
		.with_array_breaks(false)
		.with_extra_newline(true)
		.with_sorted_keys(true)
		.with_max_decimals(4);

	formatter.to_vec(value).desc("Failed to serialize JSON")
}

/// Returns local or bundled reflection database
pub fn get_reflection_database() -> &'static ReflectionDatabase<'static> {
	rbx_reflection_database::get_local()
		.ok()
		.flatten()
		.unwrap_or_else(rbx_reflection_database::get_bundled)
}

/// Formats a `Ref` as protocol v1's canonical 32-char lowercase hex string
/// (the all-zeros string is the root sentinel). This matches the `Ref` serde
/// human-readable representation, so refs inside serialized JSON frames come
/// out identically
pub fn format_ref(id: rbx_dom_weak::types::Ref) -> String {
	id.to_string()
}

/// Strictly parses a protocol v1 JSON ref: exactly 32 lowercase hex chars.
/// The all-zeros string parses to the none/root sentinel. Anything else
/// (wrong length, uppercase, non-hex) is rejected with a clear error so a
/// frame containing a malformed ref is never partially applied
pub fn parse_ref(hex: &str) -> Result<rbx_dom_weak::types::Ref> {
	if hex.len() != 32 || !hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) {
		anyhow::bail!("Malformed ref {hex:?}: expected exactly 32 lowercase hex characters");
	}

	hex.parse()
		.map_err(|err| anyhow::anyhow!("Malformed ref {hex:?}: {err}"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use rbx_dom_weak::types::Ref;

	#[test]
	fn ref_hex_round_trips() {
		let id = Ref::new();
		let hex = format_ref(id);

		assert_eq!(hex.len(), 32);
		assert!(hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')));
		assert_eq!(parse_ref(&hex).unwrap(), id);
	}

	#[test]
	fn all_zeros_is_the_root_sentinel() {
		let root = parse_ref("00000000000000000000000000000000").unwrap();

		assert!(root.is_none());
		assert_eq!(format_ref(Ref::none()), "00000000000000000000000000000000");
	}

	#[test]
	fn malformed_refs_are_rejected() {
		// Wrong length
		assert!(parse_ref("abc123").is_err());
		assert!(parse_ref("").is_err());
		assert!(parse_ref(&"a".repeat(33)).is_err());
		// Uppercase
		assert!(parse_ref("ABCDEF00000000000000000000000000").is_err());
		// Non-hex
		assert!(parse_ref("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
		// Sign or whitespace smuggled past a lax integer parser
		assert!(parse_ref("+1234567890123456789012345678901").is_err());
		assert!(parse_ref(" 1234567890123456789012345678901").is_err());
	}

	#[test]
	fn json_serialization_matches_the_codec() {
		let id = Ref::new();

		assert_eq!(
			serde_json::to_value(id).unwrap(),
			serde_json::Value::String(format_ref(id))
		);
	}
}
