//! Forwarding one request to a daemon (or the desktop broker) on loopback.
//!
//! Every forwarded request gets a connection of its own. Loopback connects
//! cost next to nothing, and a pooled connection would have to be kept apart
//! per upstream anyway. A WebSocket upgrade is forwarded as a request like
//! any other. Once the upstream answers `101 Switching Protocols`, both
//! upgraded connections are spliced byte for byte, so frames, pings and
//! closes pass through untouched.

use axum::{
	body::Body,
	http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode, Uri},
	response::IntoResponse,
	Json,
};
use hyper_util::rt::TokioIo;
use log::debug;
use serde_json::json;
use tokio::net::TcpStream;

/// Headers that describe one connection, not the request, and so never cross
/// the hub (RFC 9110 §7.6.1). An upgrade keeps `connection` and `upgrade`:
/// they are the upgrade
const HOP_BY_HOP: [&str; 8] = [
	"connection",
	"keep-alive",
	"proxy-authenticate",
	"proxy-authorization",
	"proxy-connection",
	"te",
	"trailer",
	"upgrade",
];

fn is_upgrade(headers: &HeaderMap) -> bool {
	headers.contains_key(header::UPGRADE)
		&& headers
			.get(header::CONNECTION)
			.and_then(|value| value.to_str().ok())
			.is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"))
}

pub fn bad_gateway(message: String) -> Response<Body> {
	(StatusCode::BAD_GATEWAY, Json(json!({ "error": message }))).into_response()
}

/// Sends `request` to `127.0.0.1:<port>` at `path_and_query` (the part of the
/// URL after the route) and returns the upstream's answer
pub async fn forward(mut request: Request<Body>, port: u16, path_and_query: &str) -> Response<Body> {
	let upgrade = is_upgrade(request.headers());
	let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut request));

	let stream = match TcpStream::connect(("127.0.0.1", port)).await {
		Ok(stream) => stream,
		Err(err) => return bad_gateway(format!("The daemon on port {port} did not accept a connection: {err}")),
	};

	stream.set_nodelay(true).ok();

	let (mut sender, connection) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
		Ok(pair) => pair,
		Err(err) => return bad_gateway(format!("Failed to talk to the daemon on port {port}: {err}")),
	};

	tokio::spawn(async move {
		if let Err(err) = connection.with_upgrades().await {
			debug!("Hub upstream connection ended: {err}");
		}
	});

	let (mut parts, body) = request.into_parts();

	parts.uri = match path_and_query.parse::<Uri>() {
		Ok(uri) => uri,
		Err(err) => return bad_gateway(format!("Cannot forward {path_and_query}: {err}")),
	};

	if !upgrade {
		for name in HOP_BY_HOP {
			parts.headers.remove(name);
		}
	}

	if let Ok(host) = HeaderValue::from_str(&format!("127.0.0.1:{port}")) {
		parts.headers.insert(header::HOST, host);
	}

	let mut response = match sender.send_request(Request::from_parts(parts, body)).await {
		Ok(response) => response,
		Err(err) => return bad_gateway(format!("The daemon on port {port} dropped the request: {err}")),
	};

	if let Some(client_upgrade) = client_upgrade {
		if response.status() == StatusCode::SWITCHING_PROTOCOLS {
			let upstream_upgrade = hyper::upgrade::on(&mut response);

			tokio::spawn(async move {
				let (client, upstream) = tokio::join!(client_upgrade, upstream_upgrade);

				match (client, upstream) {
					(Ok(client), Ok(upstream)) => {
						let mut client = TokioIo::new(client);
						let mut upstream = TokioIo::new(upstream);

						if let Err(err) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
							debug!("Hub WebSocket splice ended: {err}");
						}
					}
					(client, upstream) => debug!(
						"Hub WebSocket upgrade failed (client: {:?}, daemon: {:?})",
						client.err(),
						upstream.err()
					),
				}
			});

			let (parts, _) = response.into_parts();

			return Response::from_parts(parts, Body::empty());
		}
	}

	let (mut parts, body) = response.into_parts();

	for name in HOP_BY_HOP {
		parts.headers.remove(name);
	}

	Response::from_parts(parts, Body::new(body))
}
