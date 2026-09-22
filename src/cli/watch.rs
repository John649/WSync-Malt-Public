use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::Value;
use std::{
	io::{self, Write},
	path::PathBuf,
};
use tungstenite::{connect, Message};
use uuid::Uuid;

use crate::{constants::PROTOCOL_VERSION, daemon, ext::PathExt, hub, project, wsync_info, wsync_warn};

/// Watch the project daemon's live event feed over WebSocket
#[derive(Parser)]
pub struct Watch {
	/// Project path
	#[arg()]
	project: Option<PathBuf>,

	/// Project path (named alternative to the positional project)
	#[arg(long = "project", value_name = "PATH", conflicts_with = "project")]
	project_flag: Option<PathBuf>,

	/// Talk to the daemon on this port directly instead of through the hub
	#[arg(short = 'P', long)]
	port: Option<u16>,

	/// Print one-line summaries instead of full frames
	#[arg(long)]
	compact: bool,

	/// Print frames as NDJSON with no status banner
	#[arg(long)]
	raw: bool,
}

impl Watch {
	pub fn main(self) -> Result<()> {
		let address = match self.port {
			Some(port) => format!("127.0.0.1:{port}"),
			None => self.route()?,
		};

		let url = format!("ws://{address}/ws");
		let (mut socket, _) = connect(&url).with_context(|| format!("Failed to connect to {url}"))?;

		let hello = serde_json::json!({
			"type": "hello",
			"clientId": format!("cli-watch-{}", Uuid::new_v4()),
			"role": "watch",
			"protocol": PROTOCOL_VERSION,
			"name": "wsync watch",
		});

		socket.send(Message::Text(hello.to_string()))?;

		if !self.raw {
			wsync_info!("Watching the daemon at {} (Ctrl+C to stop)", address.bold());
		}

		loop {
			let message = match socket.read() {
				Ok(message) => message,
				Err(tungstenite::Error::ConnectionClosed) => break,
				Err(err) => bail!("Connection lost: {err}"),
			};

			let text = match message {
				Message::Text(text) => text,
				Message::Close(_) => break,
				// Protocol pings are answered by tungstenite automatically
				_ => continue,
			};

			let Ok(frame) = serde_json::from_str::<Value>(&text) else {
				continue;
			};

			match frame.get("type").and_then(Value::as_str) {
				// Answer heartbeats and keep them out of the feed
				Some("ping") => {
					socket.send(Message::Text(r#"{"type":"pong"}"#.into())).ok();
				}
				Some("pong") => {}
				Some("shutdown") => {
					self.print_frame(&frame, &text);

					if !self.raw {
						wsync_warn!(
							"Daemon closed the connection: {}",
							frame.get("reason").and_then(Value::as_str).unwrap_or("unknown reason")
						);
					}

					break;
				}
				_ => self.print_frame(&frame, &text),
			}
		}

		Ok(())
	}

	/// The project's route on the hub, without the scheme
	fn route(&self) -> Result<String> {
		let project_path = project::resolve(
			self.project_flag
				.clone()
				.or_else(|| self.project.clone())
				.unwrap_or_default(),
		)?;

		if !project_path.exists() {
			bail!(
				"No project file at {}; pass --project <path>",
				project_path.to_string().bold()
			);
		}

		let canonical = daemon::canonical_project(&project_path)?;

		Ok(format!("127.0.0.1:{}/p/{}", hub::port(), hub::route_key(&canonical)))
	}

	fn print_frame(&self, frame: &Value, text: &str) {
		if self.compact {
			println!("{}", summarize(frame));
		} else {
			// NDJSON: one frame per line, exactly as received
			println!("{text}");
		}

		// This is a live feed usually consumed through a pipe; Rust block
		// buffers stdout there, so every line must flush immediately
		io::stdout().flush().ok();
	}
}

/// One-line human summary of a protocol frame
fn summarize(frame: &Value) -> String {
	let kind = frame.get("type").and_then(Value::as_str).unwrap_or("?");

	match kind {
		"hello" => format!(
			"hello  {} v{} (gameId {})",
			frame.get("name").and_then(Value::as_str).unwrap_or("?"),
			frame.get("version").and_then(Value::as_str).unwrap_or("?"),
			frame.get("gameId").map(|id| id.to_string()).unwrap_or("none".into()),
		),
		"event" => match frame.get("topic").and_then(Value::as_str) {
			Some("sync-activity") => {
				let count = |key: &str| frame.get(key).and_then(Value::as_u64).unwrap_or(0);
				let names = frame
					.get("names")
					.and_then(Value::as_array)
					.map(|names| names.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
					.unwrap_or_default();

				format!(
					"sync   {} +{} ~{} -{}  {}",
					frame.get("direction").and_then(Value::as_str).unwrap_or("?"),
					count("additions"),
					count("updates"),
					count("removals"),
					names
				)
			}
			Some("plugin-status") => format!(
				"plugin {} ({})",
				if frame.get("connected").and_then(Value::as_bool).unwrap_or(false) {
					"connected"
				} else {
					"disconnected"
				},
				frame.get("name").and_then(Value::as_str).unwrap_or("?"),
			),
			Some("daemon") => format!("daemon {}", frame.get("note").and_then(Value::as_str).unwrap_or("")),
			topic => format!("event  {}", topic.unwrap_or("?")),
		},
		"shutdown" => format!(
			"close  {} ({})",
			frame.get("reason").and_then(Value::as_str).unwrap_or("?"),
			frame.get("code").and_then(Value::as_str).unwrap_or("?"),
		),
		other => other.to_owned(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn named_and_positional_project_forms_parse() {
		let named = Watch::try_parse_from(["watch", "--project", "place.json"]).unwrap();
		let legacy = Watch::try_parse_from(["watch", "place.json"]).unwrap();
		assert_eq!(named.project_flag, legacy.project);
		assert!(Watch::try_parse_from(["watch", "place.json", "--project", "other.json"]).is_err());
	}
}
