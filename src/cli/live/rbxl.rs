//! `rbxl` — refresh `place.rbxl`, the connected Studio's place as a binary
//! place file beside the project file, and say where it is (docs/place-file.md).
//!
//! The daemon does the work (`POST /place-file`): it asks the plugin for every
//! service whose content changed since the last capture, pulls the serialized
//! bytes, and rewrites the file atomically. This command only asks for it now
//! instead of waiting for the daemon's own background refresh, which is what
//! a script about to open the file with Lune wants.

use anyhow::{bail, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::{json, Value};

use crate::{
	cli::client::{print_json, Client, Targeting},
	cli::live::transfer::human_size,
	wsync_info, wsync_warn,
};

/// Refresh place.rbxl, the connected Studio's place for offline tools like Lune
#[derive(Parser)]
pub struct Rbxl {
	#[command(flatten)]
	targeting: Targeting,

	/// Seconds to wait for Studio to serialize and hand over the place
	#[arg(long, value_name = "SECONDS", default_value_t = 300)]
	timeout: u64,

	/// Print the daemon's JSON report
	#[arg(long)]
	raw: bool,
}

impl Rbxl {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let endpoint = client.post_endpoint_with_timeout("/place-file", &json!({}), self.timeout * 1000)?;

		if endpoint.unavailable() {
			bail!(
				"This daemon build does not keep a place file — restart it on the current wsync \
				 (`wsync daemon restart --project <path>`)"
			);
		}

		let Some(report) = endpoint.json else {
			bail!(
				"The daemon answered /place-file with a non-JSON body (HTTP {}): {}",
				endpoint.status,
				endpoint.text
			);
		};

		if self.raw {
			print_json(&report);

			if report["ok"] != true {
				std::process::exit(1);
			}

			return Ok(());
		}

		if report["ok"] != true {
			let error = report["error"].as_str().unwrap_or("the refresh failed");

			if report["exists"] == true {
				wsync_warn!(
					"An older copy is still at {}; it was not refreshed",
					report["path"].as_str().unwrap_or_default().bold()
				);
			}

			bail!("{error}");
		}

		let path = report["path"].as_str().unwrap_or_default();
		let bytes = report["bytes"].as_u64().unwrap_or_default();
		let names = |key: &str| -> Vec<String> {
			report[key]
				.as_array()
				.map(|names| names.iter().filter_map(Value::as_str).map(str::to_owned).collect())
				.unwrap_or_default()
		};

		let captured = names("captured");
		let services = report["services"].as_u64().unwrap_or_default();

		let what = if captured.is_empty() {
			format!("all {services} services unchanged since the last capture")
		} else {
			format!(
				"{} of {services} services serialized again ({})",
				captured.len(),
				captured.join(", ")
			)
		};

		if report["written"] == true {
			wsync_info!("Wrote {} ({}): {what}", path.bold(), human_size(bytes));
		} else {
			wsync_info!("{} is current ({}): {what}", path.bold(), human_size(bytes));
		}

		Ok(())
	}
}
