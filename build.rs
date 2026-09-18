use anyhow::Result;
use std::{
	env, fs,
	fs::File,
	path::{Path, PathBuf},
	process::{Command, Stdio},
};

/// Runs a git command in the manifest directory, returning trimmed stdout on
/// success and `None` when git is absent, the directory is not a repository,
/// or the command fails (e.g. a repository with no commits yet)
fn git_output(manifest_dir: &str, args: &[&str]) -> Option<String> {
	let output = Command::new("git")
		.args(args)
		.current_dir(manifest_dir)
		.stderr(Stdio::null())
		.output()
		.ok()?;

	if !output.status.success() {
		return None;
	}

	Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Stamps the build identity as compile-time environment for the version
/// surfaces (`/hello` reports `commit` and `dirty` from these)
fn stamp_build_identity(manifest_dir: &str) {
	let commit = git_output(manifest_dir, &["rev-parse", "--short=12", "HEAD"])
		.filter(|commit| !commit.is_empty())
		.unwrap_or_else(|| String::from("unknown"));

	// A repository with pending changes is a dirty build; outside git (or in
	// a repository git cannot answer for) the tree is reported clean
	let dirty = match git_output(manifest_dir, &["status", "--porcelain"]) {
		Some(status) => !status.is_empty(),
		None => false,
	};

	println!("cargo:rustc-env=WSYNC_BUILD_COMMIT={commit}");
	println!("cargo:rustc-env=WSYNC_BUILD_DIRTY={dirty}");

	// Re-stamp when the checked-out commit moves; dirtiness inherently lags
	// until the next rebuild, which is acceptable for a diagnostic surface
	let Some(gitdir) = resolve_gitdir(manifest_dir) else {
		return;
	};

	let head = gitdir.join("HEAD");

	if !head.exists() {
		return;
	}

	println!("cargo:rerun-if-changed={}", head.display());

	// `HEAD` on a branch is one line naming a ref, and that file is what
	// actually moves when a commit lands — watching `HEAD` alone means the
	// stamp only refreshes on a checkout. In a worktree the ref lives in the
	// *common* directory, not beside this worktree's HEAD
	let Ok(contents) = fs::read_to_string(&head) else {
		return;
	};

	let Some(reference) = contents.trim().strip_prefix("ref:") else {
		// Detached HEAD: the hash is in HEAD itself, already watched above
		return;
	};

	let reference = reference.trim();
	let common = common_dir(&gitdir);

	// A packed ref has no file to watch; `packed-refs` is the next best
	// trigger and covers a freshly cloned or gc'd repository
	for candidate in [common.join(reference), common.join("packed-refs")] {
		if candidate.exists() {
			println!("cargo:rerun-if-changed={}", candidate.display());
			break;
		}
	}
}

/// The real git directory for `manifest_dir`. In a normal checkout that is
/// `.git/`; in a worktree (and in a submodule) `.git` is a *file* holding a
/// `gitdir: <path>` line pointing at
/// `<common>/worktrees/<name>`, and everything this build script reads —
/// `HEAD` above all — lives there instead
fn resolve_gitdir(manifest_dir: &str) -> Option<PathBuf> {
	let dot_git = PathBuf::from(manifest_dir).join(".git");

	if dot_git.is_dir() {
		return Some(dot_git);
	}

	if !dot_git.is_file() {
		return None;
	}

	let pointer = fs::read_to_string(&dot_git).ok()?;
	let target = pointer.lines().find_map(|line| line.trim().strip_prefix("gitdir:"))?;
	let target = PathBuf::from(target.trim());

	// The pointer is allowed to be relative to the directory holding `.git`
	let target = if target.is_absolute() {
		target
	} else {
		PathBuf::from(manifest_dir).join(target)
	};

	target.is_dir().then_some(target)
}

/// Where shared refs live for a git directory: a worktree's gitdir carries a
/// `commondir` file pointing back at the main repository's `.git`, and
/// `refs/heads/...` is only ever resolvable there
fn common_dir(gitdir: &Path) -> PathBuf {
	let Ok(contents) = fs::read_to_string(gitdir.join("commondir")) else {
		return gitdir.to_owned();
	};

	let common = PathBuf::from(contents.trim());

	if common.is_absolute() {
		common
	} else {
		gitdir.join(common)
	}
}

fn main() -> Result<()> {
	let manifest_dir = env::var("CARGO_MANIFEST_DIR")?;
	let out_path = PathBuf::from(env::var("OUT_DIR")?).join("WSync.rbxm");

	stamp_build_identity(&manifest_dir);

	// WSync never downloads the plugin at compile time (the Argon fork did).
	// With the `plugin` feature enabled the locally built plugin is bundled
	// into the binary when present, otherwise an empty placeholder is used
	// so that offline builds always succeed
	let local_plugin = PathBuf::from(&manifest_dir).join("plugin").join("WSync.rbxm");

	println!("cargo:rerun-if-changed=build.rs");

	if cfg!(feature = "plugin") {
		println!("cargo:rerun-if-changed={}", local_plugin.display());

		if local_plugin.exists() {
			fs::copy(local_plugin, out_path)?;
			return Ok(());
		}

		println!("cargo:warning=plugin/WSync.rbxm not found, building without a bundled plugin!");
	}

	File::create(out_path)?;

	Ok(())
}
