//! Integration coverage for `wsync rbxl` and `POST /place-file`
//! (docs/place-file.md): the real binary against a real daemon whose fake
//! plugin serves `place_file_status`, `place_file_capture` and the blob pull,
//! so the per-service cache, the assembly, the atomic write and the git
//! exclusion are exercised end to end.

mod common;

use rbx_dom_weak::{types::Variant, ustr, InstanceBuilder, WeakDom};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
	collections::HashMap,
	fs,
	process::Command,
	sync::{Arc, Mutex},
};

use common::{
	chunk_answer, cli_json, journal_ops, scratch_project, spawn_cli_plugin, start_daemon, start_daemon_in, CliAnswer,
	CliSandbox, TestDaemon,
};

/// One service's roots, as SerializationService would hand them over
fn rbxm(names: &[&str]) -> Vec<u8> {
	let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
	let root = dom.root_ref();

	for name in names {
		let folder = dom.insert(root, InstanceBuilder::new("Folder").with_name(*name));

		dom.insert(folder, InstanceBuilder::new("Part").with_name("Inner"));
	}

	let roots = dom.root().children().to_vec();
	let mut bytes = Vec::new();

	rbx_binary::to_writer(&mut bytes, &dom, &roots).unwrap();

	bytes
}

/// The fake Studio: service generations a test can move, and the roots each
/// service serializes to at the moment it is captured
#[derive(Default)]
struct Place {
	generations: HashMap<String, u64>,
	roots: HashMap<String, Vec<&'static str>>,
	blobs: HashMap<String, Vec<u8>>,
	next_blob: u64,
}

fn place_plugin(daemon: &TestDaemon, place: Arc<Mutex<Place>>) -> common::CliJournal {
	spawn_cli_plugin(
		daemon,
		"place-plugin",
		Arc::new(move |op, args| {
			let mut place = place.lock().unwrap();

			match op {
				"place_file_status" => {
					let services: Vec<Value> = ["Workspace", "Lighting"]
						.iter()
						.map(|name| {
							let properties = match *name {
								"Workspace" => json!({
									"Gravity": { "Float32": 50.0 },
									"CurrentCamera": { "Ref": "0123456789abcdef0123456789abcdef" },
								}),
								_ => json!({ "ClockTime": { "Float32": 6.5 } }),
							};

							json!({
								"name": name,
								"className": name,
								"generation": place.generations.get(*name).copied().unwrap_or(0),
								"properties": properties,
							})
						})
						.collect();

					CliAnswer::Value(json!({ "epoch": "epoch-1", "services": services }))
				}
				"place_file_capture" => {
					let service = args["service"].as_str().unwrap_or_default().to_owned();
					let generation = place.generations.get(&service).copied().unwrap_or(0);
					let roots = place.roots.get(&service).cloned().unwrap_or_default();

					if roots.is_empty() {
						return CliAnswer::Value(json!({ "name": service, "generation": generation, "roots": 0 }));
					}

					let bytes = rbxm(&roots);
					let sha = format!("{:x}", Sha256::digest(&bytes));

					place.next_blob += 1;

					let id = format!("blob-{}", place.next_blob);

					place.blobs.insert(id.clone(), bytes.clone());

					CliAnswer::Value(json!({
						"name": service,
						"generation": generation,
						"roots": roots.len(),
						"blob": { "artifactId": id, "bytes": bytes.len(), "sha256": sha },
					}))
				}
				"branch_blob_open" => {
					let id = args["artifactId"].as_str().unwrap_or_default();

					match place.blobs.get(id) {
						Some(bytes) => CliAnswer::Value(json!({
							"bytes": bytes.len(),
							"sha256": format!("{:x}", Sha256::digest(bytes)),
						})),
						None => CliAnswer::Failure("NOT_FOUND", "no blob"),
					}
				}
				"branch_blob_read" => {
					let id = args["artifactId"].as_str().unwrap_or_default();

					match place.blobs.get(id) {
						Some(bytes) => CliAnswer::Value(chunk_answer(bytes, args)),
						None => CliAnswer::Failure("NOT_FOUND", "no blob"),
					}
				}
				"branch_blob_close" => {
					let id = args["artifactId"].as_str().unwrap_or_default();

					CliAnswer::Value(json!({ "ok": place.blobs.remove(id).is_some() }))
				}
				_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
			}
		}),
	)
}

fn run_rbxl(sandbox: &CliSandbox, daemon: &TestDaemon) -> std::process::Output {
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	sandbox.run(&["rbxl", "--project", &project, "--port", &port, "--raw"])
}

fn names(value: &Value) -> Vec<String> {
	value
		.as_array()
		.unwrap()
		.iter()
		.map(|name| name.as_str().unwrap().to_owned())
		.collect()
}

/// The service children of a written place, by name, with their children
fn read_place(path: &std::path::Path) -> (WeakDom, Vec<(String, Vec<String>)>) {
	let dom = rbx_binary::from_reader(fs::read(path).unwrap().as_slice()).unwrap();

	let services = dom
		.root()
		.children()
		.iter()
		.map(|id| {
			let service = dom.get_by_ref(*id).unwrap();
			let children = service
				.children()
				.iter()
				.map(|child| dom.get_by_ref(*child).unwrap().name.clone())
				.collect();

			(service.name.clone(), children)
		})
		.collect();

	(dom, services)
}

#[test]
fn rbxl_writes_the_place_and_serializes_only_what_moved() {
	let daemon = start_daemon(None);
	let place = Arc::new(Mutex::new(Place::default()));

	place.lock().unwrap().roots.insert("Workspace".into(), vec!["Map", "Spawns"]);

	let journal = place_plugin(&daemon, place.clone());
	let sandbox = CliSandbox::new();
	let path = daemon.root.join("place.rbxl");

	// First refresh: everything is captured
	let output = run_rbxl(&sandbox, &daemon);
	let report = cli_json(&output);

	assert!(output.status.success(), "rbxl failed: {report}");
	assert_eq!(report["ok"], true);
	assert_eq!(report["written"], true);
	assert_eq!(names(&report["captured"]), ["Workspace", "Lighting"]);
	assert_eq!(report["instances"], 4);
	assert_eq!(report["path"].as_str().unwrap(), path.to_string_lossy());

	let (dom, services) = read_place(&path);

	assert_eq!(
		services,
		[
			("Workspace".to_owned(), vec!["Map".to_owned(), "Spawns".to_owned()]),
			("Lighting".to_owned(), vec![]),
		]
	);

	let workspace = dom.get_by_ref(dom.root().children()[0]).unwrap();
	let lighting = dom.get_by_ref(dom.root().children()[1]).unwrap();

	assert_eq!(workspace.properties.get(&ustr("Gravity")), Some(&Variant::Float32(50.0)));
	assert_eq!(workspace.properties.get(&ustr("CurrentCamera")), None);
	assert_eq!(lighting.properties.get(&ustr("ClockTime")), Some(&Variant::Float32(6.5)));

	// Every pinned blob was released once it was pulled
	assert!(place.lock().unwrap().blobs.is_empty());

	// Nothing moved: nothing is serialized and nothing is rewritten
	let before = fs::metadata(&path).unwrap().modified().unwrap();
	let captures = journal_ops(&journal)
		.iter()
		.filter(|op| *op == "place_file_capture")
		.count();

	let report = cli_json(&run_rbxl(&sandbox, &daemon));

	assert_eq!(report["written"], false);
	assert!(names(&report["captured"]).is_empty());
	assert_eq!(names(&report["reused"]), ["Workspace", "Lighting"]);
	assert_eq!(
		journal_ops(&journal)
			.iter()
			.filter(|op| *op == "place_file_capture")
			.count(),
		captures
	);
	assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);

	// Workspace moved: only Workspace is serialized again
	{
		let mut place = place.lock().unwrap();

		place.generations.insert("Workspace".into(), 1);
		place.roots.insert("Workspace".into(), vec!["Map", "Spawns", "Boss"]);
	}

	let report = cli_json(&run_rbxl(&sandbox, &daemon));

	assert_eq!(report["written"], true);
	assert_eq!(names(&report["captured"]), ["Workspace"]);
	assert_eq!(names(&report["reused"]), ["Lighting"]);

	let (_, services) = read_place(&path);

	assert_eq!(services[0].1, ["Map", "Spawns", "Boss"]);

	// No temporary file was left beside it
	let leftovers: Vec<String> = fs::read_dir(&daemon.root)
		.unwrap()
		.filter_map(|entry| entry.ok())
		.map(|entry| entry.file_name().to_string_lossy().into_owned())
		.filter(|name| name.ends_with(".tmp"))
		.collect();

	assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
}

#[test]
fn rbxl_without_a_studio_says_so() {
	let daemon = start_daemon(None);
	let sandbox = CliSandbox::new();

	let output = run_rbxl(&sandbox, &daemon);
	let report = cli_json(&output);

	assert!(!output.status.success());
	assert_eq!(report["ok"], false);
	assert_eq!(report["code"], "NO_PLUGIN");
	assert_eq!(report["exists"], false);
}

#[test]
fn rbxl_names_an_outdated_plugin() {
	let daemon = start_daemon(None);
	let _journal = spawn_cli_plugin(
		&daemon,
		"old-plugin",
		Arc::new(|_, _| CliAnswer::Failure("UNKNOWN_OP", "unknown op")),
	);
	let sandbox = CliSandbox::new();

	let output = run_rbxl(&sandbox, &daemon);
	let report = cli_json(&output);

	assert!(!output.status.success());
	assert_eq!(report["code"], "PLUGIN_OUTDATED");
	assert!(report["error"].as_str().unwrap().contains("wsync plugin install"));
}

#[test]
fn the_place_file_is_kept_out_of_git() {
	let dir = scratch_project();
	let git = |args: &[&str]| {
		Command::new("git")
			.arg("-C")
			.arg(dir.path())
			.args(args)
			.output()
			.map(|output| output.status.success())
			.unwrap_or(false)
	};

	if !git(&["init", "-q"]) {
		eprintln!("git is not available; skipping");
		return;
	}

	let daemon = start_daemon_in(dir);
	let place = Arc::new(Mutex::new(Place::default()));
	let _journal = place_plugin(&daemon, place);
	let sandbox = CliSandbox::new();

	let output = run_rbxl(&sandbox, &daemon);

	assert!(output.status.success(), "rbxl failed: {}", cli_json(&output));

	let exclude = fs::read_to_string(daemon.root.join(".git").join("info").join("exclude")).unwrap();

	assert!(
		exclude.lines().any(|line| line == "/place.rbxl"),
		"exclude file was:\n{exclude}"
	);

	let status = Command::new("git")
		.arg("-C")
		.arg(&daemon.root)
		.args(["status", "--porcelain", "--", "place.rbxl"])
		.output()
		.unwrap();

	assert!(String::from_utf8_lossy(&status.stdout).trim().is_empty());

	// A second daemon run does not add the line twice
	let output = run_rbxl(&sandbox, &daemon);

	assert!(output.status.success());

	let exclude = fs::read_to_string(daemon.root.join(".git").join("info").join("exclude")).unwrap();

	assert_eq!(exclude.lines().filter(|line| *line == "/place.rbxl").count(), 1);
}
