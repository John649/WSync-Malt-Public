use anyhow::Result;
use clap::Parser;
use serde_json::json;
use std::path::PathBuf;

use crate::{cli::client::print_json, config::Config, ext::PathExt, studio, wsync_info};

/// Launch a new Roblox Studio instance
#[derive(Parser)]
pub struct Studio {
	/// Path to place or model to open
	#[arg()]
	path: Option<PathBuf>,

	/// Check if Roblox Studio is already running
	#[arg(short, long)]
	check: bool,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Studio {
	pub fn main(mut self) -> Result<()> {
		if self.check && studio::is_running(None)? {
			if self.raw {
				print_json(&json!({ "ok": true, "launched": false, "running": true }));
			} else {
				wsync_info!("Roblox Studio is already running!");
			}

			return Ok(());
		}

		if !self.raw {
			wsync_info!("Launching Roblox Studio..");
		}

		if let Some(path) = self.path.as_ref() {
			if Config::new().smart_paths && !path.exists() {
				let rbxl = path.with_file_name(path.get_name().to_owned() + ".rbxl");
				let rbxlx = path.with_file_name(path.get_name().to_owned() + ".rbxlx");

				if rbxl.exists() {
					self.path = Some(rbxl);
				} else if rbxlx.exists() {
					self.path = Some(rbxlx)
				}
			}
		}

		let pid = studio::launch(self.path)?;

		if self.raw {
			print_json(&json!({ "pid": pid }));
		}

		Ok(())
	}
}
