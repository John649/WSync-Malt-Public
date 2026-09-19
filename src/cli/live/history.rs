//! Change-history control: `waypoint`, `undo`, `redo`.
//! Undo and redo acknowledge a request; inspect Studio to verify its result.

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use serde_json::json;

use crate::{
	cli::client::{field, print_ok, Client, Targeting},
	wsync_info,
};

/// Create a named Studio change-history waypoint
#[derive(Parser)]
pub struct Waypoint {
	#[command(flatten)]
	targeting: Targeting,

	/// Waypoint label, as it appears in Studio's history
	#[arg(long, value_name = "LABEL")]
	name: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Waypoint {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("waypoint", json!({ "name": self.name }), self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		let name = field(&value, "name");

		wsync_info!(
			"Set the change-history waypoint {}",
			if name.is_empty() { self.name.as_str() } else { name }.bold()
		);

		Ok(())
	}
}

/// Ask Studio to undo one change-history entry
#[derive(Parser)]
pub struct Undo {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Undo {
	pub fn main(self) -> Result<()> {
		step(&self.targeting, "undo", self.raw)
	}
}

/// Ask Studio to redo the last undone change-history entry
#[derive(Parser)]
pub struct Redo {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Redo {
	pub fn main(self) -> Result<()> {
		step(&self.targeting, "redo", self.raw)
	}
}

/// `undo`/`redo` differ only in the op name: ChangeHistoryService reports
/// nothing about what moved, so both answer `{requested:true}` and the CLI
/// reports the request, not an outcome
fn step(targeting: &Targeting, op: &str, raw: bool) -> Result<()> {
	let client = Client::connect(targeting)?;
	let value = client.value(op, json!({}), raw)?;

	if raw {
		print_ok(&value);

		return Ok(());
	}

	wsync_info!(
		"Asked Studio to {op} one change-history entry — inspect the result with `{}`",
		"wsync tree".bold()
	);

	Ok(())
}
