//! `auth` — the CLI Roblox Open Cloud credential store (auth.json).
//!
//! The credential is never accepted as a command-line value and never
//! printed: `set` reads it from exactly one of stdin, a file, or a named
//! environment variable; `status` reports presence plus a masked tail only;
//! `clear` removes it. Nothing here touches the network.
//!
//! The store is `secrets.json` in the WSync state directory (or an explicit
//! `--data-dir`), holding the `robloxCloudApiKey` key. On Unix both the file
//! and every write go through mode 0600 — filesystem-permission protection,
//! which auth.json is explicit is a fallback tier below an OS keychain. The
//! file is written atomically (0600 temp + rename), so a torn write can
//! never leave a world-readable or half-written secret behind.
//!
//! Named **profiles** (`--profile <name>`) sit beside the default credential
//! under the store's `profiles` object, one per Roblox account or group:
//! each holds its own Open Cloud key, a default upload creator
//! (`user:<id>`/`group:<id>`), or both. `upload` and `monetization` pick one
//! with `--profile` or `WSYNC_PROFILE`; a profile without a key borrows the
//! normal credential chain and only switches the creator.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use serde_json::{json, Map, Value};
use std::{
	fs,
	io::{Read, Write},
	path::{Path, PathBuf},
};

use crate::{cli::client::print_json, daemon, wsync_info};

/// The store's key for the Open Cloud credential (auth.json)
const CREDENTIAL_KEY: &str = "robloxCloudApiKey";

/// The store's file name inside the state directory
const SECRETS_FILE: &str = "secrets.json";

/// The store's object of named profiles
const PROFILES_KEY: &str = "profiles";

/// A profile's credential and default-creator fields
const PROFILE_KEY_FIELD: &str = "apiKey";
const PROFILE_CREATOR_FIELD: &str = "creator";

/// The environment variable that selects a profile when `--profile` is absent
pub const PROFILE_ENV: &str = "WSYNC_PROFILE";

/// Credentials are small; anything past this is a mistake, not a key
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;

/// Store, inspect, or clear the CLI Roblox Open Cloud credential
#[derive(Parser)]
pub struct Auth {
	#[command(subcommand)]
	command: AuthCommand,
}

#[derive(Subcommand)]
enum AuthCommand {
	/// Store a credential from stdin, a file, or an environment variable
	Set(AuthSet),
	/// Report whether a credential is stored (never prints the credential)
	Status(AuthStatus),
	/// Remove the stored credential (or one profile)
	Clear(AuthClear),
}

/// A named profile: its own key, its default creator, or both
pub(crate) struct Profile {
	pub api_key: Option<String>,
	pub creator: Option<String>,
}

impl Auth {
	pub fn main(self) -> Result<()> {
		match self.command {
			AuthCommand::Set(command) => command.main(),
			AuthCommand::Status(command) => command.main(),
			AuthCommand::Clear(command) => command.main(),
		}
	}
}

#[derive(Parser)]
struct AuthSet {
	/// Read the credential from stdin (the credential itself is never
	/// accepted as an argument)
	#[arg(long = "from-stdin", conflicts_with_all = ["file", "from_env"])]
	from_stdin: bool,

	/// Read the credential from this file
	#[arg(long, value_name = "PATH", conflicts_with_all = ["from_stdin", "from_env"])]
	file: Option<PathBuf>,

	/// Read the credential from this environment variable
	#[arg(long = "from-env", value_name = "NAME", conflicts_with_all = ["from_stdin", "file"])]
	from_env: Option<String>,

	/// Store into this named profile instead of the default credential
	#[arg(long, value_name = "NAME")]
	profile: Option<String>,

	/// The profile's default upload creator, `user:<id>` or `group:<id>`
	#[arg(long, value_name = "USER-OR-GROUP", requires = "profile")]
	creator: Option<String>,

	/// Override the WSync state directory holding the store
	#[arg(long = "data-dir", value_name = "PATH")]
	data_dir: Option<PathBuf>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl AuthSet {
	fn main(self) -> Result<()> {
		// clap's conflicts_with rules out pairs; this rules out none
		let sources =
			usize::from(self.from_stdin) + usize::from(self.file.is_some()) + usize::from(self.from_env.is_some());

		if let Some(name) = &self.profile {
			validate_profile_name(name)?;
		}

		let creator = match &self.creator {
			Some(creator) => Some(crate::cli::upload::Creator::parse(creator)?.describe()),
			None => None,
		};

		// A profile may take only a creator (it then borrows the normal
		// credential chain); everything else needs exactly one key source
		if sources > 1 || (sources == 0 && creator.is_none()) {
			bail!(
				"`{}` needs exactly one of --from-stdin, --file, or --from-env{}",
				"wsync auth set".bold(),
				if self.profile.is_some() {
					" (or --creator alone)"
				} else {
					""
				}
			);
		}

		let path = store_path(self.data_dir.as_deref())?;

		if sources == 0 {
			let name = self.profile.as_deref().unwrap_or_default();
			let profile = write_profile(&path, name, None, creator.as_deref())?;

			return report_profile(&path, name, &profile, self.raw, "Profile stored");
		}

		let credential = if self.from_stdin {
			let mut value = String::new();

			// The +1 makes an oversized paste detectable instead of silently
			// truncated into a wrong-but-stored key
			std::io::stdin()
				.take(MAX_CREDENTIAL_BYTES + 1)
				.read_to_string(&mut value)
				.context("Failed to read the credential from stdin")?;

			value
		} else if let Some(path) = &self.file {
			let metadata =
				fs::metadata(path).with_context(|| format!("Failed to inspect the --file {}", path.display()))?;

			if metadata.len() > MAX_CREDENTIAL_BYTES {
				bail!("The credential file exceeds 64 KiB — that is not an API key");
			}

			fs::read_to_string(path).with_context(|| format!("Failed to read the --file {}", path.display()))?
		} else {
			let name = self.from_env.as_deref().unwrap_or_default();

			match std::env::var(name) {
				Ok(value) => value,
				Err(_) => bail!("The environment variable `{name}` is not set (or is not valid UTF-8)"),
			}
		};

		let credential = credential.trim();

		if credential.is_empty() {
			bail!("The credential is empty");
		}

		if credential.len() as u64 > MAX_CREDENTIAL_BYTES {
			bail!("The credential exceeds 64 KiB — that is not an API key");
		}

		if let Some(name) = &self.profile {
			let profile = write_profile(&path, name, Some(credential), creator.as_deref())?;

			return report_profile(&path, name, &profile, self.raw, "Profile stored");
		}

		write_credential(&path, credential)?;

		report(
			&path,
			true,
			Some(masked_tail(credential)),
			self.raw,
			"Credential stored",
			None,
		)
	}
}

#[derive(Parser)]
struct AuthStatus {
	/// Report only this named profile
	#[arg(long, value_name = "NAME")]
	profile: Option<String>,

	/// Override the WSync state directory holding the store
	#[arg(long = "data-dir", value_name = "PATH")]
	data_dir: Option<PathBuf>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl AuthStatus {
	fn main(self) -> Result<()> {
		let path = store_path(self.data_dir.as_deref())?;

		if let Some(name) = &self.profile {
			validate_profile_name(name)?;

			return match read_profile(&path, name)? {
				Some(profile) => report_profile(&path, name, &profile, self.raw, "Profile"),
				None => bail!(
					"No profile named `{name}` — store one with `{}`",
					format!("wsync auth set --profile {name}").bold()
				),
			};
		}

		let credential = read_credential(&path)?;
		let profiles = read_profiles(&path)?;

		if !self.raw && !profiles.is_empty() {
			for (name, profile) in &profiles {
				report_profile(&path, name, profile, false, "Profile")?;
			}
		}

		let raw_profiles = (self.raw && !profiles.is_empty()).then(|| profiles_json(&profiles));

		match credential {
			Some(credential) => report(
				&path,
				true,
				Some(masked_tail(&credential)),
				self.raw,
				"A credential is stored",
				raw_profiles,
			),
			// The CLI file is empty — the desktop app's keychain entry still
			// counts (it is what `stored_credential` will hand the cloud
			// client), and saying "no credential" while uploads work would be
			// the same confusion this tier exists to end
			None => match app_keychain_credential() {
				Some(credential) => report(
					&path,
					true,
					Some(masked_tail(&credential)),
					self.raw,
					"A credential is stored in the desktop app's keychain entry",
					raw_profiles,
				),
				None => report(
					&path,
					false,
					None,
					self.raw,
					"No default credential stored",
					raw_profiles,
				),
			},
		}
	}
}

#[derive(Parser)]
struct AuthClear {
	/// Remove this named profile instead of the default credential
	#[arg(long, value_name = "NAME")]
	profile: Option<String>,

	/// Override the WSync state directory holding the store
	#[arg(long = "data-dir", value_name = "PATH")]
	data_dir: Option<PathBuf>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl AuthClear {
	fn main(self) -> Result<()> {
		let path = store_path(self.data_dir.as_deref())?;

		if let Some(name) = &self.profile {
			validate_profile_name(name)?;
			remove_profile(&path, name)?;

			if self.raw {
				print_json(
					&json!({ "ok": true, "profile": name, "configured": false, "path": path.to_string_lossy() }),
				);
			} else {
				wsync_info!("Profile {} cleared ({})", name.bold(), path.display());
			}

			return Ok(());
		}

		remove_credential(&path)?;

		report(&path, false, None, self.raw, "Credential cleared", None)
	}
}

/// Presence plus location — never the secret. The masked tail is the last
/// four characters, enough to tell keys apart and useless to an attacker
fn report(
	path: &Path,
	configured: bool,
	masked: Option<String>,
	raw: bool,
	message: &str,
	profiles: Option<Value>,
) -> Result<()> {
	if raw {
		let mut record = json!({
			"ok": true,
			"configured": configured,
			"key": CREDENTIAL_KEY,
			"path": path.to_string_lossy(),
		});

		if let Some(masked) = &masked {
			record["maskedTail"] = json!(masked);
		}

		if let Some(profiles) = profiles {
			record["profiles"] = profiles;
		}

		print_json(&record);

		return Ok(());
	}

	wsync_info!(
		"{message}{} ({})",
		match &masked {
			Some(masked) => format!(" — {}", masked.bold()),
			None => String::new(),
		},
		path.display()
	);

	Ok(())
}

/// One profile's presence report — its creator and masked key tail, never
/// the key
fn report_profile(path: &Path, name: &str, profile: &Profile, raw: bool, message: &str) -> Result<()> {
	if raw {
		let mut record = profile_json(profile);

		record["ok"] = json!(true);
		record["profile"] = json!(name);
		record["path"] = json!(path.to_string_lossy());
		print_json(&record);

		return Ok(());
	}

	wsync_info!(
		"{message} {}: key {}, creator {}",
		name.bold(),
		match &profile.api_key {
			Some(key) => masked_tail(key).bold().to_string(),
			None => "none (uses the default credential chain)".to_owned(),
		},
		profile
			.creator
			.as_deref()
			.unwrap_or("none (uses --creator or the project's groupId)")
	);

	Ok(())
}

fn profile_json(profile: &Profile) -> Value {
	let mut record = json!({
		"configured": profile.api_key.is_some(),
		"creator": profile.creator,
	});

	if let Some(key) = &profile.api_key {
		record["maskedTail"] = json!(masked_tail(key));
	}

	record
}

fn profiles_json(profiles: &[(String, Profile)]) -> Value {
	Value::Object(
		profiles
			.iter()
			.map(|(name, profile)| (name.clone(), profile_json(profile)))
			.collect(),
	)
}

/// Profile names travel through flags, env vars and messages — keep them
/// plain
fn validate_profile_name(name: &str) -> Result<()> {
	let valid = !name.is_empty()
		&& name.len() <= 64
		&& name
			.chars()
			.all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.');

	if !valid {
		bail!("Profile names are 1–64 letters, digits, `-`, `_` or `.`, not `{name}`");
	}

	Ok(())
}

fn parse_profile(value: &Value) -> Profile {
	let field = |key: &str| {
		value
			.get(key)
			.and_then(Value::as_str)
			.map(str::trim)
			.filter(|text| !text.is_empty())
			.map(str::to_owned)
	};

	Profile {
		api_key: field(PROFILE_KEY_FIELD),
		creator: field(PROFILE_CREATOR_FIELD),
	}
}

/// Every stored profile, sorted by name
fn read_profiles(path: &Path) -> Result<Vec<(String, Profile)>> {
	let store = read_store(path)?;
	let mut profiles: Vec<(String, Profile)> = match store.get(PROFILES_KEY) {
		Some(Value::Object(map)) => map
			.iter()
			.map(|(name, value)| (name.clone(), parse_profile(value)))
			.collect(),
		_ => Vec::new(),
	};

	profiles.sort_by(|a, b| a.0.cmp(&b.0));

	Ok(profiles)
}

fn read_profile(path: &Path, name: &str) -> Result<Option<Profile>> {
	let store = read_store(path)?;

	Ok(store
		.get(PROFILES_KEY)
		.and_then(|profiles| profiles.get(name))
		.map(parse_profile))
}

/// The named profile from the CLI store — the cloud credential chain's
/// override tier. `Ok(None)` means no such profile
pub(crate) fn stored_profile(name: &str) -> Result<Option<Profile>> {
	validate_profile_name(name)?;
	read_profile(&store_path(None)?, name)
}

/// Create or update a profile. A field left `None` keeps its stored value,
/// so the key and the creator can be set in separate calls
fn write_profile(path: &Path, name: &str, api_key: Option<&str>, creator: Option<&str>) -> Result<Profile> {
	let mut store = read_store(path)?;
	let profiles = store
		.entry(PROFILES_KEY.to_owned())
		.or_insert_with(|| Value::Object(Map::new()));

	let Value::Object(profiles) = profiles else {
		bail!("{} holds a `{PROFILES_KEY}` that is not a JSON object", path.display());
	};

	let entry = profiles
		.entry(name.to_owned())
		.or_insert_with(|| Value::Object(Map::new()));

	let Value::Object(entry) = entry else {
		bail!("{} holds a profile `{name}` that is not a JSON object", path.display());
	};

	if let Some(api_key) = api_key {
		entry.insert(PROFILE_KEY_FIELD.to_owned(), json!(api_key));
	}

	if let Some(creator) = creator {
		entry.insert(PROFILE_CREATOR_FIELD.to_owned(), json!(creator));
	}

	let profile = parse_profile(&Value::Object(entry.clone()));

	write_store(path, &store)?;

	Ok(profile)
}

fn remove_profile(path: &Path, name: &str) -> Result<()> {
	let mut store = read_store(path)?;

	let removed = match store.get_mut(PROFILES_KEY) {
		Some(Value::Object(profiles)) => {
			let removed = profiles.remove(name).is_some();

			if profiles.is_empty() {
				store.remove(PROFILES_KEY);
			}

			removed
		}
		_ => false,
	};

	if !removed {
		bail!("No profile named `{name}`");
	}

	if store.is_empty() {
		match fs::remove_file(path) {
			Ok(()) => Ok(()),
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Err(err) => Err(err).with_context(|| format!("Failed to remove {}", path.display())),
		}
	} else {
		write_store(path, &store)
	}
}

fn masked_tail(credential: &str) -> String {
	let tail: String = credential
		.chars()
		.rev()
		.take(4)
		.collect::<Vec<_>>()
		.into_iter()
		.rev()
		.collect();

	format!("…{tail}")
}

fn store_path(data_dir: Option<&Path>) -> Result<PathBuf> {
	Ok(daemon::state_dir(data_dir)?.join(SECRETS_FILE))
}

/// The parsed store, tolerating a missing file. A store that exists but does
/// not parse is a hard error — silently overwriting it could destroy another
/// tool's secret
fn read_store(path: &Path) -> Result<Map<String, Value>> {
	let text = match fs::read_to_string(path) {
		Ok(text) => text,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
		Err(err) => return Err(err).with_context(|| format!("Failed to read {}", path.display())),
	};

	let parsed: Value =
		serde_json::from_str(&text).with_context(|| format!("{} exists but is not valid JSON", path.display()))?;

	match parsed {
		Value::Object(map) => Ok(map),
		other => bail!(
			"{} exists but holds {}, not a JSON object",
			path.display(),
			crate::cli::client::kind_of(&other)
		),
	}
}

/// The JSON key the **desktop app** writes an Open Cloud key under in this same
/// `secrets.json` (its file-store tier). Pinned to `secrets.rs` `SECRET_NAMES`
/// and `settings.js` `SECRET_NAME`. A long key (a real Open Cloud key is ~600
/// bytes) lands here rather than the keychain, whose prompt truncates at 128 —
/// so the CLI must read this name or `wsync upload` sees "no credential" while
/// the app shows the key as set.
const APP_FILE_STORE_KEY: &str = "openCloudApiKey";

/// The stored credential from `secrets.json`: the CLI's own key
/// (`wsync auth set`) wins, then the desktop app's file-store key. Both tools
/// share this one file.
fn read_credential(path: &Path) -> Result<Option<String>> {
	let store = read_store(path)?;
	let value = store
		.get(CREDENTIAL_KEY)
		.and_then(Value::as_str)
		.or_else(|| store.get(APP_FILE_STORE_KEY).and_then(Value::as_str));
	Ok(value.map(str::to_owned))
}

/// The stored Open Cloud credential, if any — the first tier of the cloud
/// credential chain (upload.json, monetization.json). The value is handed to
/// the cloud client and never printed.
///
/// Two stores are consulted, in order:
///
/// 1. the CLI's own `secrets.json` — an explicit `wsync auth set` always wins;
/// 2. the **desktop app's keychain entry** — the Settings → Secrets panel
///    stores the key in the macOS keychain, and a key the user already gave
///    the app must not read as "no credential" here. That asymmetry is exactly
///    how an agent ends up improvising a browser-upload fallback while the app
///    truthfully shows the key as Set.
pub(crate) fn stored_credential() -> Result<Option<String>> {
	if let Some(credential) = read_credential(&store_path(None)?)? {
		return Ok(Some(credential));
	}

	Ok(app_keychain_credential())
}

/// The keychain identity the desktop app writes under — pinned to
/// `desktop/src-tauri/src/secrets.rs` (`KEYCHAIN_SERVICE`) and the Settings
/// panel's secret name (`settings.js` `SECRET_NAME`). Change them together.
const APP_KEYCHAIN_SERVICE: &str = "com.wsync.desktop";
const APP_KEYCHAIN_ACCOUNT: &str = "openCloudApiKey";

/// Read the app-stored credential from the macOS keychain via
/// `/usr/bin/security` — the same binary the app writes through, so the item's
/// ACL already trusts it and no GUI prompt appears. Absent item, non-macOS, or
/// any failure is simply `None`: this is a fallback tier, never an error.
fn app_keychain_credential() -> Option<String> {
	if !cfg!(target_os = "macos") {
		return None;
	}

	let output = std::process::Command::new("/usr/bin/security")
		.args([
			"find-generic-password",
			"-s",
			APP_KEYCHAIN_SERVICE,
			"-a",
			APP_KEYCHAIN_ACCOUNT,
			"-w",
		])
		.output()
		.ok()?;

	if !output.status.success() {
		return None;
	}

	let credential = String::from_utf8(output.stdout).ok()?;
	let credential = credential.trim_end_matches('\n');

	if credential.is_empty() {
		None
	} else {
		Some(credential.to_owned())
	}
}

fn write_credential(path: &Path, credential: &str) -> Result<()> {
	let mut store = read_store(path)?;

	store.insert(CREDENTIAL_KEY.to_owned(), json!(credential));
	write_store(path, &store)
}

/// Removing the last key removes the file; other tools' keys survive
fn remove_credential(path: &Path) -> Result<()> {
	let mut store = read_store(path)?;

	if store.remove(CREDENTIAL_KEY).is_none() {
		return Ok(());
	}

	if store.is_empty() {
		match fs::remove_file(path) {
			Ok(()) => Ok(()),
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Err(err) => Err(err).with_context(|| format!("Failed to remove {}", path.display())),
		}
	} else {
		write_store(path, &store)
	}
}

/// Atomic 0600 write: the temp file is created with the final mode, so the
/// secret is never readable by another user for even a moment, and the
/// rename means no reader ever sees a partial store
fn write_store(path: &Path, store: &Map<String, Value>) -> Result<()> {
	let parent = path.parent().context("The credential store path has no parent")?;

	fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;

	let temp = path.with_extension(format!("tmp-{}", std::process::id()));
	let mut options = fs::OpenOptions::new();

	options.write(true).create_new(true);

	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;

		options.mode(0o600);
	}

	let written = (|| -> Result<()> {
		let mut file = options
			.open(&temp)
			.with_context(|| format!("Failed to create {}", temp.display()))?;

		serde_json::to_writer_pretty(&mut file, &Value::Object(store.clone()))?;
		file.write_all(b"\n")?;
		file.sync_all()?;

		fs::rename(&temp, path)
			.with_context(|| format!("Failed to move the store into place at {}", path.display()))?;

		// An existing store predating this write keeps whatever mode it had;
		// pin it, so upgrading an old file also tightens it
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;

			fs::set_permissions(path, fs::Permissions::from_mode(0o600))
				.with_context(|| format!("Failed to set permissions on {}", path.display()))?;
		}

		Ok(())
	})();

	if written.is_err() {
		fs::remove_file(&temp).ok();
	}

	written
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn masked_tails_never_leak_more_than_four_characters() {
		assert_eq!(masked_tail("abcdefgh"), "…efgh");
		assert_eq!(masked_tail("abc"), "…abc");
		assert_eq!(masked_tail("k"), "…k");
	}

	#[test]
	fn profiles_store_merge_list_and_clear_beside_the_default_credential() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join(SECRETS_FILE);

		write_credential(&path, "default-key").unwrap();
		write_profile(&path, "owner", Some("owner-key"), Some("user:4774617141")).unwrap();
		write_profile(&path, "group", None, Some("group:280410213")).unwrap();

		// A later key-only write keeps the stored creator
		let owner = write_profile(&path, "owner", Some("owner-key-2"), None).unwrap();
		assert_eq!(owner.api_key.as_deref(), Some("owner-key-2"));
		assert_eq!(owner.creator.as_deref(), Some("user:4774617141"));

		let names: Vec<String> = read_profiles(&path)
			.unwrap()
			.into_iter()
			.map(|(name, _)| name)
			.collect();
		assert_eq!(names, ["group", "owner"]);
		assert!(read_profile(&path, "group").unwrap().unwrap().api_key.is_none());

		// Profiles never shadow the default credential
		assert_eq!(read_credential(&path).unwrap().as_deref(), Some("default-key"));

		remove_profile(&path, "owner").unwrap();
		remove_profile(&path, "group").unwrap();
		assert!(remove_profile(&path, "group").is_err());
		assert!(read_store(&path).unwrap().get(PROFILES_KEY).is_none());
		assert_eq!(read_credential(&path).unwrap().as_deref(), Some("default-key"));
	}

	#[test]
	fn profile_names_stay_plain() {
		assert!(validate_profile_name("igntiino").is_ok());
		assert!(validate_profile_name("cute-malt_games.2").is_ok());
		assert!(validate_profile_name("").is_err());
		assert!(validate_profile_name("a b").is_err());
		assert!(validate_profile_name("../x").is_err());
	}

	#[test]
	fn read_credential_finds_the_app_file_store_key_and_prefers_the_cli_own() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join(SECRETS_FILE);

		// Nothing stored yet.
		assert_eq!(read_credential(&path).unwrap(), None);

		// The desktop app's file-store tier writes `openCloudApiKey` — a long
		// Open Cloud key lands here because the keychain prompt truncates it.
		// The CLI must find it or `wsync upload` reports "no credential" while
		// the app shows the key as set.
		fs::write(&path, br#"{"openCloudApiKey":"app-written-key"}"#).unwrap();
		assert_eq!(read_credential(&path).unwrap().as_deref(), Some("app-written-key"));

		// An explicit `wsync auth set` (its own key) wins over the app's.
		fs::write(
			&path,
			br#"{"openCloudApiKey":"app-written-key","robloxCloudApiKey":"cli-set-key"}"#,
		)
		.unwrap();
		assert_eq!(read_credential(&path).unwrap().as_deref(), Some("cli-set-key"));
	}
}
