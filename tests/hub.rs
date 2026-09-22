//! The hub (src/hub, docs/hub.md): one loopback port in front of every
//! daemon. Each test runs real `wsync daemon start` processes in a sandbox
//! with its own hub port, so nothing here touches a hub on this machine.

mod common;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
	fs,
	io::{BufRead, BufReader},
	path::{Path, PathBuf},
	process::Child,
	sync::mpsc,
	thread,
	time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::Message;

use common::CliSandbox;

struct DaemonProcess {
	child: Child,
}

impl Drop for DaemonProcess {
	fn drop(&mut self) {
		self.child.kill().ok();
		self.child.wait().ok();
	}
}

fn place_project(base: &Path, name: &str, game_id: u64) -> PathBuf {
	let root = base.join(name);

	fs::create_dir_all(root.join("src")).unwrap();
	fs::write(root.join("src").join("Hello.luau"), "return \"hello\"\n").unwrap();

	let project = json!({
		"name": name,
		"tree": {
			"$className": "DataModel",
			"ReplicatedStorage": { "$path": "src" },
		},
		"gameId": game_id,
		"placeIds": [game_id],
	});

	fs::write(
		root.join("default.project.json"),
		serde_json::to_string_pretty(&project).unwrap(),
	)
	.unwrap();

	root.canonicalize().unwrap()
}

fn canonical(project: &Path) -> String {
	project.join("default.project.json").to_string_lossy().into_owned()
}

/// `wsync daemon start --raw` for `project`, once it has reported readiness
fn start(sandbox: &CliSandbox, project: &Path) -> (DaemonProcess, Value) {
	let mut child = sandbox.spawn(&[
		"daemon",
		"start",
		"--project",
		&project.to_string_lossy(),
		"--managed-by",
		"test",
		"--raw",
	]);

	let stdout = child.stdout.take().unwrap();
	let (line_tx, line_rx) = mpsc::channel();

	thread::spawn(move || {
		let mut reader = BufReader::new(stdout);
		let mut line = String::new();

		reader.read_line(&mut line).ok();
		line_tx.send(line).ok();

		let mut rest = String::new();

		while reader.read_line(&mut rest).is_ok_and(|read| read > 0) {
			rest.clear();
		}
	});

	let line = line_rx
		.recv_timeout(Duration::from_secs(30))
		.expect("daemon start never reported readiness");
	let report: Value = serde_json::from_str(line.trim()).unwrap_or_else(|err| panic!("not JSON ({err}): {line}"));

	assert_eq!(report["ok"], true, "{report}");

	(DaemonProcess { child }, report)
}

fn http() -> reqwest::blocking::Client {
	reqwest::blocking::Client::builder()
		.timeout(Duration::from_secs(3))
		.redirect(reqwest::redirect::Policy::none())
		.build()
		.unwrap()
}

fn get(url: &str) -> Option<(u16, Value)> {
	let response = http().get(url).send().ok()?;
	let status = response.status().as_u16();

	Some((status, response.json().unwrap_or(Value::Null)))
}

/// Polls until `check` holds, or panics naming `what`
fn eventually<T>(what: &str, timeout: Duration, mut check: impl FnMut() -> Option<T>) -> T {
	let deadline = Instant::now() + timeout;

	loop {
		if let Some(value) = check() {
			return value;
		}

		assert!(Instant::now() < deadline, "timed out waiting for {what}");
		thread::sleep(Duration::from_millis(100));
	}
}

fn route(project: &Path) -> String {
	wsync::hub::route_key(&canonical(project))
}

/// The daemon behind `project`'s route, as it answers through the hub
fn routed_hello(sandbox: &CliSandbox, project: &Path) -> Option<Value> {
	match get(&sandbox.hub_url(&format!("/p/{}/hello", route(project)))) {
		Some((200, hello)) => Some(hello),
		_ => None,
	}
}

fn listed(sandbox: &CliSandbox) -> Vec<Value> {
	get(&sandbox.hub_url("/daemons"))
		.and_then(|(_, listing)| listing["daemons"].as_array().cloned())
		.unwrap_or_default()
}

#[test]
fn daemons_register_and_are_reached_by_route() {
	let sandbox = CliSandbox::new();
	let project_a = place_project(sandbox.work.path(), "fixture-a", 5550001);
	let project_b = place_project(sandbox.work.path(), "fixture-b", 5550002);

	let (_a, report_a) = start(&sandbox, &project_a);
	let (_b, report_b) = start(&sandbox, &project_b);

	// Nobody picked a port: each daemon took one from the OS
	assert_ne!(report_a["port"], report_b["port"]);

	let (_, hub) = get(&sandbox.hub_url("/hello")).expect("a hub must be running");

	assert_eq!(hub["hub"], true);
	assert_eq!(hub["port"], sandbox.hub_port);
	assert!(
		hub.get("protocol").is_none(),
		"the hub must not look like a daemon to an older plugin"
	);

	for (project, report) in [(&project_a, &report_a), (&project_b, &report_b)] {
		let hello = eventually("the route to answer", Duration::from_secs(10), || {
			routed_hello(&sandbox, project)
		});

		assert_eq!(hello["canonicalProject"], canonical(project));
		assert_eq!(hello["bootId"], report["bootId"]);
	}

	let daemons = listed(&sandbox);

	assert_eq!(daemons.len(), 2, "{daemons:#?}");

	for daemon in &daemons {
		let key = daemon["route"].as_str().unwrap();

		assert_eq!(daemon["base"], format!("/p/{key}"));
		assert_eq!(
			wsync::hub::route_key(daemon["hello"]["canonicalProject"].as_str().unwrap()),
			key
		);
	}

	// An unknown route is a JSON 404, never a daemon's answer
	let (status, body) = get(&sandbox.hub_url("/p/0123456789abcdef/hello")).unwrap();

	assert_eq!(status, 404);
	assert!(body["error"].as_str().unwrap().contains("No daemon is registered"));
}

#[test]
fn a_stopped_daemon_leaves_and_a_restarted_one_keeps_its_route() {
	let sandbox = CliSandbox::new();
	let project = place_project(sandbox.work.path(), "fixture", 5550003);

	let (daemon, first) = start(&sandbox, &project);

	eventually("the first boot to register", Duration::from_secs(10), || {
		routed_hello(&sandbox, &project).filter(|hello| hello["bootId"] == first["bootId"])
	});

	// A killed daemon's socket closes with it, and its route goes at once
	drop(daemon);

	eventually("the route to disappear", Duration::from_secs(5), || {
		matches!(
			get(&sandbox.hub_url(&format!("/p/{}/hello", route(&project)))),
			Some((404, _))
		)
		.then_some(())
	});

	assert!(listed(&sandbox).is_empty());

	// The same project comes back on whatever port it gets, at the same route
	let (_daemon, second) = start(&sandbox, &project);

	assert_ne!(first["bootId"], second["bootId"]);

	eventually("the second boot to register", Duration::from_secs(10), || {
		routed_hello(&sandbox, &project).filter(|hello| hello["bootId"] == second["bootId"])
	});
}

#[test]
fn a_hub_that_dies_is_started_again_and_every_daemon_returns() {
	let sandbox = CliSandbox::new();
	let project = place_project(sandbox.work.path(), "fixture", 5550004);

	let (_daemon, report) = start(&sandbox, &project);

	eventually("the daemon to register", Duration::from_secs(10), || {
		routed_hello(&sandbox, &project)
	});

	let (_, hub) = get(&sandbox.hub_url("/hello")).unwrap();
	let pid = hub["pid"].as_u64().unwrap() as u32;

	wsync::util::kill_process(pid);

	eventually(
		"a new hub with the daemon registered again",
		Duration::from_secs(15),
		|| {
			let (_, hub) = get(&sandbox.hub_url("/hello"))?;

			(hub["pid"].as_u64()? as u32 != pid)
				.then(|| routed_hello(&sandbox, &project))
				.flatten()
				.filter(|hello| hello["bootId"] == report["bootId"])
		},
	);
}

#[test]
fn web_pages_are_turned_away() {
	let sandbox = CliSandbox::new();
	let project = place_project(sandbox.work.path(), "fixture", 5550005);
	let (_daemon, _) = start(&sandbox, &project);

	eventually("the daemon to register", Duration::from_secs(10), || {
		routed_hello(&sandbox, &project)
	});

	let url = sandbox.hub_url(&format!("/p/{}/hello", route(&project)));

	let from_a_page = http().get(&url).header("Origin", "https://example.com").send().unwrap();

	assert_eq!(from_a_page.status().as_u16(), 403);

	// DNS rebinding: a page on a name that resolves to 127.0.0.1
	let rebound = http()
		.get(&url)
		.header("Host", format!("rebind.example:{}", sandbox.hub_port))
		.send()
		.unwrap();

	assert_eq!(rebound.status().as_u16(), 403);

	// The desktop app's webview is not a web page
	let desktop = http().get(&url).header("Origin", "tauri://localhost").send().unwrap();

	assert_eq!(desktop.status().as_u16(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_clients_reach_their_daemon_through_the_hub() {
	let sandbox = CliSandbox::new();
	let project = place_project(sandbox.work.path(), "fixture", 5550006);
	let hub_port = sandbox.hub_port;

	let (daemon, key) = tokio::task::spawn_blocking(move || {
		let (daemon, _) = start(&sandbox, &project);

		eventually("the daemon to register", Duration::from_secs(10), || {
			routed_hello(&sandbox, &project)
		});

		let key = route(&project);

		((daemon, sandbox), key)
	})
	.await
	.unwrap();

	let url = format!("ws://127.0.0.1:{hub_port}/p/{key}/ws");
	let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

	let hello = json!({
		"type": "hello",
		"clientId": "test-watch",
		"role": "watch",
		"protocol": 1,
		"name": "hub test",
	});

	socket.send(Message::Text(hello.to_string())).await.unwrap();

	let reply = tokio::time::timeout(Duration::from_secs(10), async {
		while let Some(message) = socket.next().await {
			if let Ok(Message::Text(text)) = message {
				return serde_json::from_str::<Value>(&text).ok();
			}
		}

		None
	})
	.await
	.expect("no reply through the hub")
	.expect("the socket closed without a reply");

	assert_eq!(reply["type"], "hello", "{reply}");
	assert_eq!(reply["name"], "fixture", "{reply}");
	assert_eq!(reply["gameId"], 5550006, "{reply}");

	drop(socket);
	tokio::task::spawn_blocking(move || drop(daemon)).await.unwrap();
}
