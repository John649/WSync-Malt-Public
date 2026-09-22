use anyhow::{bail, Result};
use clap::Parser;
use colored::Colorize;
use log::{debug, info};
use std::{
	path::PathBuf,
	process,
	sync::{
		atomic::{AtomicU16, Ordering},
		Arc,
	},
	thread,
};

use crate::{
	config::Config,
	core::Core,
	ext::PathExt,
	hub, integration,
	program::{Program, ProgramName},
	project::{self, Project},
	server::{self, Server, ServerIdentity},
	sessions, wsync_error, wsync_info, wsync_warn,
};

/// Start local server and listen for file changes
#[derive(Parser)]
pub struct Serve {
	/// Project path
	#[arg()]
	project: Option<PathBuf>,

	/// Project path (named alternative to the positional project)
	#[arg(long = "project", value_name = "PATH", conflicts_with = "project")]
	project_flag: Option<PathBuf>,

	/// Session identifier
	#[arg()]
	session: Option<String>,

	/// Server host name
	#[arg(short = 'H', long)]
	host: Option<String>,

	/// Server port
	#[arg(short = 'P', long)]
	port: Option<u16>,

	/// Generate sourcemap every time files change
	#[arg(short, long)]
	sourcemap: bool,

	/// Whether to run using roblox-ts
	#[arg(short, long)]
	ts: bool,

	/// Run WSync asynchronously
	#[arg(short = 'A', long = "async")]
	run_async: bool,

	/// Spawn the WSync child process (internal)
	#[arg(long, hide = true)]
	wsync_spawn: bool,
}

impl Serve {
	pub fn main(self) -> Result<()> {
		let project_path = project::resolve(
			self.project_flag
				.clone()
				.or_else(|| self.project.clone())
				.unwrap_or_default(),
		)?;

		Config::load_workspace(project_path.get_parent());
		let config = Config::new();

		if !self.wsync_spawn && (self.run_async || config.run_async) {
			return self.spawn();
		}

		let sourcemap_path = if self.sourcemap || config.with_sourcemap {
			Some(project_path.with_file_name("sourcemap.json"))
		} else {
			None
		};

		if !project_path.exists() {
			bail!(
				"No project files found in {}. Run {} to create new one",
				project_path.get_parent().to_string().bold(),
				"wsync init".bold(),
			);
		}

		let project = Project::load(&project_path)?;

		if !project.is_place() {
			bail!("Cannot serve non-place project!");
		}

		let use_wally = config.use_wally || (config.detect_project && project.is_wally());
		let use_ts = self.ts || config.ts_mode || (config.detect_project && project.is_ts());

		if use_wally {
			integration::check_wally_packages(&project.workspace_dir);
		}

		if use_ts {
			debug!("Starting roblox-ts");

			let working_dir = project_path.get_parent();

			let child = Program::new(ProgramName::Npx)
				.message("Failed to serve roblox-ts project")
				.current_dir(working_dir)
				.arg("rbxtsc")
				.arg("--watch")
				.spawn()?;

			if child.is_none() {
				return Ok(());
			}
		}

		let core = Core::new(project, true)?;
		let host = self.host.unwrap_or(core.host().unwrap_or(config.host.clone()));
		// No pin means whatever port the OS hands out: the hub is how
		// everything reaches this daemon (src/hub)
		let port = self.port.or(core.port()).unwrap_or(0);

		if port != 0 && !server::is_port_free(&host, port) {
			bail!("Port {} is already in use", port.to_string().bold());
		}

		let core = Arc::new(core);

		if let Some(path) = sourcemap_path {
			let core = core.clone();
			let queue = core.queue();

			queue.subscribe_internal().unwrap();
			core.sourcemap(Some(path.clone()), false)?;

			wsync_info!("Generated sourcemap at: {}", path.to_string().bold());

			thread::spawn(move || loop {
				let _message = queue.get_change(0).unwrap();

				info!("Regenerating sourcemap..");

				match core.sourcemap(Some(path.clone()), false) {
					Ok(()) => (),
					Err(err) => {
						wsync_error!("Failed to regenerate sourcemap: {}", err);
					}
				}
			});
		}

		let server = Server::new(core, &host, port, ServerIdentity::unmanaged()).with_hub(true);
		let bound = Arc::new(AtomicU16::new(port));

		let ready: Box<dyn FnOnce(u16) + Send> = {
			let host = host.clone();
			let bound = bound.clone();
			let session = self.session.clone();
			let run_async = config.run_async;

			Box::new(move |port| {
				bound.store(port, Ordering::Relaxed);

				if let Err(err) = sessions::add(session, Some(host.clone()), Some(port), process::id(), run_async) {
					wsync_warn!("Failed to record this session: {}", err);
				}

				wsync_info!(
					"Serving on: {} (through the hub at {}), project: {}",
					server::format_address(&host, port).bold(),
					hub::url("").bold(),
					project_path.to_string().bold()
				);
			})
		};

		drop(config);
		server.start(Some(ready))?;

		let port = bound.load(Ordering::Relaxed);

		// Graceful stop (`/stop` route): remove this session's entry
		if let Ok(Some(session)) = sessions::get(None, Some(host.clone()), Some(port)) {
			sessions::remove(&session).ok();
		}

		Ok(())
	}

	fn spawn(self) -> Result<()> {
		Program::new(ProgramName::Wsync).args(self.spawn_args()).spawn()?;
		Ok(())
	}

	fn spawn_args(self) -> Vec<String> {
		let mut args = vec![String::from("serve")];

		if let Some(project) = self.project_flag.or(self.project) {
			args.push(project.to_string());
		}

		if let Some(session) = self.session {
			args.push(session);
		}

		if let Some(host) = self.host {
			args.push("--host".into());
			args.push(host)
		}

		if let Some(port) = self.port {
			args.push("--port".into());
			args.push(port.to_string());
		}

		if self.sourcemap {
			args.push("--sourcemap".into());
		}

		if self.ts {
			args.push("--ts".into());
		}

		args
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn named_project_survives_child_arguments() {
		let command = Serve::try_parse_from(["serve", "--project", "a project/default.project.json"]).unwrap();
		let child = Serve::try_parse_from(command.spawn_args()).unwrap();
		assert_eq!(child.project, Some(PathBuf::from("a project/default.project.json")));
	}

	#[test]
	fn legacy_project_and_session_remain_supported() {
		let command = Serve::try_parse_from(["serve", "place.json", "my-session"]).unwrap();
		let child = Serve::try_parse_from(command.spawn_args()).unwrap();
		assert_eq!(child.project, Some(PathBuf::from("place.json")));
		assert_eq!(child.session.as_deref(), Some("my-session"));
		assert!(Serve::try_parse_from(["serve", "place.json", "--project", "other.json"]).is_err());
	}
}
