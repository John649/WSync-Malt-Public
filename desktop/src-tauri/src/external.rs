//! Daemons this host did not start (docs/places.md's runtime registry).
//!
//! The app reports only the sessions it spawned or adopted itself, which is a
//! lie on the one screen where it matters: a project served from the CLI
//! (`wsync daemon start --project <dir>`) has a daemon running and a Studio
//! plugin attached while its card still reads "Idle". The user is told "not
//! connected" by the app and "connected" by the plugin at the same time.
//!
//! This module closes that gap the way `wsync places` does, and with the same
//! two ingredients: the runtime records under `<state>/daemons/*.json`, and a
//! `GET /hello` on the port each one names. Nothing here is authority — it is
//! a read of what is already on disk plus one loopback probe, and it starts,
//! stops and adopts nothing. Flipping the toggle is still what adopts a
//! daemon (`daemon.rs`'s "spawn (or adopt)").
//!
//! Two state directories are read because there are two kinds of daemon: the
//! CLI keeps its records under the *local* data dir, the desktop app under the
//! *roaming* one it hands every daemon it spawns. A record naming a port is
//! not proof of a daemon: a process that died and left its port to another
//! project must never be reported as this record's daemon, so an entry is kept
//! only when `/hello` answers with the record's own boot id and project.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::loopback;

/// A probe is a loopback round trip to a process that is either there or not.
/// Matches the CLI registry's budget (`src/cli/places.rs`).
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// A runtime record, in the shape the engine writes it (`src/daemon.rs`).
///
/// Every field is optional on read: a record written by an older or newer
/// engine must degrade to "not a daemon I can describe", never to a parse
/// error that empties the whole listing.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeRecord {
    #[serde(default)]
    project: String,
    #[serde(default)]
    canonical_project: String,
    #[serde(default)]
    pid: u32,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    boot_id: String,
    #[serde(default)]
    managed_by: String,
}

/// One registered project, reduced to what matching needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisteredProject {
    pub(crate) id: String,
    pub(crate) path: String,
}

/// A live daemon found on disk rather than in this host's registry.
///
/// `projectId` is the whole point: the matching happens here, where the
/// canonical-path rules live, so the view only has to look an id up.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExternalDaemon {
    /// The registered project this daemon serves, when it serves one.
    pub(crate) project_id: Option<String>,
    /// Project file path, as the record spells it.
    pub(crate) project: String,
    pub(crate) canonical_project: String,
    pub(crate) port: u16,
    pub(crate) pid: u32,
    /// `cli` or `desktop` — who started the process.
    pub(crate) managed_by: String,
    /// The project name the daemon reports.
    pub(crate) name: String,
    /// The branch this daemon serves. Non-null means it belongs to a project's
    /// branches rather than to the project card.
    pub(crate) branch: Option<String>,
    /// A scratch daemon that disappears with its Studio window: never a
    /// registered project.
    pub(crate) ephemeral: bool,
    pub(crate) plugin_connected: bool,
    /// The place the connected Studio plugin has open, when one is connected.
    pub(crate) place: Option<String>,
}

// ------------------------------------------------------------ path matching --

/// Fold a path into the form two spellings of the same file share.
///
/// Windows supplies three ways to disagree about one path: the `\\?\` verbatim
/// prefix (`fs::canonicalize` adds it, so the folder picker's answer carries
/// one and a hand-typed `--project` does not), the separator, and case. The
/// engine's `canonicalProject` and this app's stored project path are both
/// canonical and still need not match as strings, so neither is compared raw.
///
/// The verbatim prefix is stripped on every platform rather than under
/// `cfg(windows)`: a record copied from a Windows machine then reads the same
/// anywhere, and a POSIX path never carries one to begin with.
fn normalize(path: &str) -> String {
    let mut text = path.trim();
    for prefix in ["\\\\?\\UNC\\", "\\\\?\\", "//?/UNC/", "//?/"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest;
            break;
        }
    }

    // Separator and case folding are Windows facts, not universal ones: `a/b`
    // and `a\b` are one file there and two names on POSIX.
    let mut normalized = if cfg!(windows) {
        text.replace('/', "\\").to_lowercase()
    } else {
        text.to_owned()
    };

    while normalized.len() > 1 && (normalized.ends_with('\\') || normalized.ends_with('/')) {
        normalized.pop();
    }
    normalized
}

/// The directory a project file sits in, given an already-normalized path.
fn parent_dir(path: &str) -> Option<&str> {
    let cut = path.rfind(['\\', '/'])?;
    if cut == 0 {
        return Some("/");
    }
    Some(&path[..cut])
}

/// Which registered project a daemon's project file belongs to.
///
/// The comparison is on the *directory*: a project is a folder here, and the
/// file inside it may be `default.project.json` or any other `*.project.json`
/// the engine accepted. It also falls out that a branch daemon — whose project
/// file lives under `<project>/.wsync-branches/<name>/tree/` — never matches
/// the project card, which is the right answer for a second reason.
pub(crate) fn match_project(project_file: &str, projects: &[RegisteredProject]) -> Option<String> {
    let directory = parent_dir(&normalize(project_file))?.to_owned();
    projects
        .iter()
        .find(|project| normalize(&project.path) == directory)
        .map(|project| project.id.clone())
}

// ---------------------------------------------------------------- the scan --

/// Every `daemons/` directory a record could have been written into.
///
/// `WSYNC_STATE_DIR` is honored for the reason the engine honors it: with it
/// set, that *is* where the CLI's records are, and reading the platform
/// directories alone would report no daemons at all.
pub(crate) fn daemon_dirs(roaming: &Path, local: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let push = |dirs: &mut Vec<PathBuf>, dir: PathBuf| {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };

    push(&mut dirs, roaming.to_path_buf());
    if let Some(local) = local {
        push(&mut dirs, local.join("WSync").join("daemons"));
    }
    if let Some(explicit) = std::env::var_os("WSYNC_STATE_DIR").filter(|value| !value.is_empty()) {
        push(&mut dirs, PathBuf::from(explicit).join("daemons"));
    }
    dirs
}

/// Read one `daemons/` directory. A record that will not parse is skipped, not
/// reported: this listing is a convenience and must never fail as a whole.
fn read_records(dir: &Path) -> Vec<RuntimeRecord> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        // `.start.lock` and the staging temp files share the directory.
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !name.starts_with('.'))
        })
        .filter_map(|path| fs::read(&path).ok())
        .filter_map(|bytes| serde_json::from_slice::<RuntimeRecord>(&bytes).ok())
        .filter(|record| record.port != 0)
        .collect()
}

/// Every live daemon the records point at, matched against the registry.
///
/// The probes run together: one unreachable record must not add its timeout to
/// the next one's, because this is polled on the Projects view's cadence.
pub(crate) async fn scan(dirs: &[PathBuf], projects: &[RegisteredProject]) -> Vec<ExternalDaemon> {
    let mut seen: HashSet<(u16, u32)> = HashSet::new();
    let mut records: Vec<RuntimeRecord> = Vec::new();
    for dir in dirs {
        for record in read_records(dir) {
            if seen.insert((record.port, record.pid)) {
                records.push(record);
            }
        }
    }

    let probes: Vec<_> = records
        .into_iter()
        .map(|record| {
            let port = record.port;
            (record, tokio::spawn(hello(port)))
        })
        .collect();

    let mut found = Vec::new();
    for (record, probe) in probes {
        let Ok(Some(hello)) = probe.await else {
            continue;
        };
        // A record points at a port, not at a process. Only a daemon that
        // answers with this record's own boot id and project is this record's
        // daemon; anything else took the port over after it died.
        if text(&hello, "bootId") != record.boot_id
            || normalize(&text(&hello, "canonicalProject")) != normalize(&record.canonical_project)
        {
            continue;
        }

        let plugin = hello.get("plugin").filter(|value| value.is_object());
        found.push(ExternalDaemon {
            project_id: match_project(&record.canonical_project, projects),
            project: record.project.clone(),
            canonical_project: record.canonical_project.clone(),
            port: record.port,
            pid: record.pid,
            managed_by: if record.managed_by.trim().is_empty() {
                text(&hello, "managedBy")
            } else {
                record.managed_by.clone()
            },
            name: text(&hello, "name"),
            branch: hello
                .get("branch")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|branch| !branch.is_empty()),
            ephemeral: hello
                .get("ephemeral")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            plugin_connected: plugin.is_some(),
            place: plugin
                .and_then(|plugin| plugin.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|name| !name.is_empty()),
        });
    }

    found.sort_by_key(|daemon| daemon.port);
    found
}

async fn hello(port: u16) -> Option<Value> {
    let response = loopback::get(port, "/hello", PROBE_TIMEOUT).await.ok()?;
    if !response.is_success() {
        return None;
    }
    response.json().filter(Value::is_object)
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The registered projects, read out of `state.json`'s `projects` array.
///
/// A project with no path is dropped rather than matched loosely: an empty
/// path would normalize to an empty string and claim every daemon whose
/// project file sits at a filesystem root.
pub(crate) fn registered_projects(projects: &Value) -> Vec<RegisteredProject> {
    let Some(entries) = projects.as_array() else {
        return Vec::new();
    };

    entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?.trim();
            let path = entry.get("path").and_then(Value::as_str)?.trim();
            if id.is_empty() || path.is_empty() {
                return None;
            }
            Some(RegisteredProject {
                id: id.to_owned(),
                path: path.to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, path: &str) -> RegisteredProject {
        RegisteredProject {
            id: id.into(),
            path: path.into(),
        }
    }

    #[test]
    fn verbatim_prefix_does_not_hide_a_match() {
        let projects = vec![project("p_1", "C:\\Users\\Me\\projects\\dungeondemo")];
        assert_eq!(
            match_project(
                "\\\\?\\C:\\Users\\Me\\projects\\dungeondemo\\default.project.json",
                &projects
            ),
            Some("p_1".into())
        );
    }

    #[test]
    fn a_verbatim_registered_path_matches_too() {
        // The folder picker canonicalizes, so this is the spelling the app
        // actually stores on Windows.
        let projects = vec![project("p_1", "\\\\?\\C:\\Users\\Me\\projects\\dungeondemo")];
        assert_eq!(
            match_project(
                "\\\\?\\C:\\Users\\Me\\projects\\dungeondemo\\default.project.json",
                &projects
            ),
            Some("p_1".into())
        );
    }

    #[test]
    fn a_trailing_separator_is_not_a_different_project() {
        let projects = vec![project("p_1", "\\\\?\\C:\\Users\\Me\\projects\\dungeondemo\\")];
        assert_eq!(
            match_project(
                "C:\\Users\\Me\\projects\\dungeondemo\\default.project.json",
                &projects
            ),
            Some("p_1".into())
        );
    }

    #[test]
    fn another_project_under_the_same_root_does_not_match() {
        let projects = vec![project("p_1", "C:\\Users\\Me\\projects\\dungeondemo")];
        assert_eq!(
            match_project(
                "C:\\Users\\Me\\projects\\dungeondemo2\\default.project.json",
                &projects
            ),
            None
        );
    }

    #[test]
    fn a_branch_daemon_belongs_to_no_project_card() {
        let projects = vec![project("p_1", "\\\\?\\C:\\Users\\Me\\projects\\dungeondemo")];
        assert_eq!(
            match_project(
                "\\\\?\\C:\\Users\\Me\\projects\\dungeondemo\\.wsync-branches\\door-boost\\tree\\default.project.json",
                &projects
            ),
            None
        );
    }

    #[test]
    fn a_project_file_that_is_not_the_default_one_still_matches() {
        let projects = vec![project("p_1", "C:\\Users\\Me\\projects\\dungeondemo")];
        assert_eq!(
            match_project(
                "C:\\Users\\Me\\projects\\dungeondemo\\game.project.json",
                &projects
            ),
            Some("p_1".into())
        );
    }

    #[cfg(windows)]
    #[test]
    fn case_and_separator_are_not_a_difference_on_windows() {
        let projects = vec![project("p_1", "C:/Users/Me/projects/DungeonDemo")];
        assert_eq!(
            match_project(
                "\\\\?\\c:\\users\\me\\projects\\dungeondemo\\default.project.json",
                &projects
            ),
            Some("p_1".into())
        );
    }

    #[test]
    fn a_bare_project_file_matches_nothing() {
        let projects = vec![project("p_1", "C:\\Users\\Me\\projects\\dungeondemo")];
        assert_eq!(match_project("default.project.json", &projects), None);
    }

    #[test]
    fn registered_projects_skips_entries_with_no_usable_path() {
        let document = serde_json::json!([
            {"id": "p_1", "path": "C:\\a\\b"},
            {"id": "p_2"},
            {"id": "", "path": "C:\\a\\c"},
            {"path": "C:\\a\\d"},
            "not an object",
        ]);
        assert_eq!(
            registered_projects(&document),
            vec![project("p_1", "C:\\a\\b")]
        );
    }

    #[test]
    fn registered_projects_of_a_missing_key_is_empty() {
        assert!(registered_projects(&Value::Null).is_empty());
    }

    #[test]
    fn daemon_dirs_holds_both_state_dirs_once() {
        let roaming = PathBuf::from("R:\\WSync\\daemons");
        let local = PathBuf::from("L:\\");
        let dirs = daemon_dirs(&roaming, Some(&local));
        assert!(dirs.contains(&roaming));
        assert!(dirs.contains(&local.join("WSync").join("daemons")));
        let mut unique = dirs.clone();
        unique.dedup();
        assert_eq!(unique.len(), dirs.len(), "no directory is read twice");
    }

    #[test]
    fn records_that_will_not_parse_are_skipped_not_fatal() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("good.json"),
            br#"{"port":7979,"pid":1,"bootId":"b","managedBy":"cli"}"#,
        )
        .unwrap();
        fs::write(directory.path().join("broken.json"), b"{not json").unwrap();
        fs::write(directory.path().join(".hidden.json"), br#"{"port":7980}"#).unwrap();
        fs::write(directory.path().join("ignored.log"), b"").unwrap();

        let records = read_records(directory.path());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].port, 7979);
        assert_eq!(records[0].managed_by, "cli");
    }

    #[test]
    fn a_missing_daemons_directory_reads_as_no_daemons() {
        assert!(read_records(Path::new("Z:\\nothing\\here\\daemons")).is_empty());
    }
}
