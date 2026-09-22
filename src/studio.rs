use anyhow::{bail, Result};
use roblox_install::RobloxStudio;
use std::{
	env,
	path::PathBuf,
	process::{Command, Stdio},
};

#[cfg(target_os = "windows")]
use winsafe::{co::SW, EnumWindows};

/// Set to anything non-empty to make every Studio launch fail instead. The
/// test sandbox sets it: a test exercising `wsync open` or `branch new` must
/// never open Roblox Studio on the machine running the suite
pub const NO_LAUNCH_ENV: &str = "WSYNC_NO_STUDIO_LAUNCH";

/// Refuses when [`NO_LAUNCH_ENV`] is set
pub fn ensure_launch_allowed() -> Result<()> {
	if env::var_os(NO_LAUNCH_ENV).is_some_and(|value| !value.is_empty()) {
		bail!("Studio launches are disabled in this environment ({NO_LAUNCH_ENV} is set)");
	}

	Ok(())
}

/// Launches Studio and returns the pid of the process that was spawned.
///
/// The pid is what a branch records so it can tell "my Studio is gone" from
/// "some Studio is running" later (docs/branches.md). Studio may re-exec
/// itself into another process, so the pid is the launcher's, not a handle
/// on the window that eventually appears — a caller that needs the window
/// still has to look for it.
pub fn launch(path: Option<PathBuf>) -> Result<u32> {
	ensure_launch_allowed()?;

	let studio_path = RobloxStudio::locate()?.application_path().to_owned();

	let child = Command::new(studio_path)
		.arg(path.unwrap_or_default())
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()?;

	Ok(child.id())
}

/// The protocol URL that opens a cloud place for editing.
///
/// Studio is not given a file here: `EditPlace` makes it fetch the place from
/// Roblox itself, which is the only way to reach a place that exists nowhere
/// on this disk. Both ids are required — Studio resolves a place inside its
/// universe, and a URL without `universeId` opens nothing
pub fn edit_place_url(place_id: u64, universe_id: u64) -> String {
	format!("roblox-studio:1+launchmode:edit+task:EditPlace+placeId:{place_id}+universeId:{universe_id}")
}

/// Launches Studio on a protocol URL and returns the pid of whatever was
/// spawned.
///
/// The installed Studio binary is tried first, because it gives a pid worth
/// having; the shell's protocol handler is the fallback for a machine where
/// the binary cannot be located but the URL scheme is registered (in which
/// case the pid belongs to the launcher, not to Studio).
pub fn launch_url(url: &str) -> Result<u32> {
	ensure_launch_allowed()?;

	if let Ok(studio) = RobloxStudio::locate() {
		let child = Command::new(studio.application_path())
			.arg(url)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()?;

		return Ok(child.id());
	}

	shell_open(url)
}

#[allow(unused_variables)]
fn shell_open(url: &str) -> Result<u32> {
	#[cfg(target_os = "windows")]
	{
		// `start` is a cmd builtin, and the empty first argument is the
		// window title it would otherwise take the URL for
		let mut command = Command::new("cmd");

		command
			.args(["/C", "start", "", url])
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null());

		// `cmd` is a console program: started from the desktop app or a
		// daemon it would open a terminal just to run `start`
		crate::util::hide_console(&mut command);

		let child = command.spawn()?;

		Ok(child.id())
	}

	#[cfg(target_os = "macos")]
	{
		let child = Command::new("open")
			.arg(url)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()?;

		Ok(child.id())
	}

	#[cfg(target_os = "linux")]
	{
		anyhow::bail!("Roblox Studio cannot be launched on Linux");
	}
}

#[allow(unused_variables)]
pub fn is_running(title: Option<String>) -> Result<bool> {
	#[cfg(target_os = "macos")]
	{
		let output = Command::new("osascript")
				.args([
					"-e",
					"tell app \"System Events\" to get the title of every window of (processes whose background only is false)",
				])
				.output()?;

		let windows = String::from_utf8(output.stdout)?;

		if let Some(title) = title {
			Ok(windows.contains(&format!("{title} - Roblox Studio")))
		} else {
			Ok(windows.contains("Roblox Studio"))
		}
	}

	#[cfg(target_os = "windows")]
	{
		let is_studio_running = EnumWindows(|hwnd| -> bool {
			if !hwnd.IsWindowVisible() {
				return true;
			}

			if let Ok(text) = hwnd.GetWindowText() {
				if let Some(title) = &title {
					if text == format!("{} - Roblox Studio", title) {
						return false;
					}
				} else if text.contains("Roblox Studio") {
					return false;
				}
			}

			true
		})
		.is_err();

		Ok(is_studio_running)
	}

	#[cfg(target_os = "linux")]
	{
		anyhow::bail!("This feature is not yet supported on Linux!");
	}
}

#[allow(unused_variables)]
pub fn focus(title: Option<String>) -> Result<()> {
	#[cfg(target_os = "macos")]
	{
		if let Some(title) = title {
			let title = title.replace('\\', "\\\\").replace('"', "\\\"");

			Command::new("osascript")
				.args([
					"-e",
					&format!(
						r#"tell application "System Events"
						repeat with theProcess in processes whose name is "RobloxStudio"
								tell theProcess
									set windowList to windows whose name contains "{title} - Roblox Studio"
									
									if (count of windowList) > 0 then
										set frontmost to true
										perform action "AXRaise" of window 1
									end if
								end tell
						end repeat
					end tell"#
					),
				])
				.output()?;
		} else {
			Command::new("osascript")
				.args([
					"-e",
					r#"tell application "System Events"
						tell process "RobloxStudio"
							set frontmost to true
							perform action "AXRaise" of window 1
						end tell
					end tell"#,
				])
				.output()?;
		}

		Ok(())
	}

	#[cfg(target_os = "windows")]
	{
		let result = EnumWindows(|hwnd| -> bool {
			if !hwnd.IsWindowVisible() {
				return true;
			}

			if let Ok(text) = hwnd.GetWindowText() {
				if let Some(title) = &title {
					if text == format!("{} - Roblox Studio", title) {
						hwnd.SetForegroundWindow();
						hwnd.ShowWindow(SW::RESTORE);

						return false;
					}
				} else if text.contains("Roblox Studio") {
					hwnd.SetForegroundWindow();
					hwnd.ShowWindow(SW::RESTORE);

					return false;
				}
			}

			true
		});

		match result {
			Ok(()) => (),
			Err(err) => {
				if err.raw() != 0 {
					anyhow::bail!("Failed to focus Roblox Studio: {}", err)
				}
			}
		}

		Ok(())
	}

	#[cfg(target_os = "linux")]
	{
		anyhow::bail!("This feature is not yet supported on Linux!");
	}
}
