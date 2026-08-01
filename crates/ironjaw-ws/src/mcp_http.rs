//! Native MCP Streamable-HTTP transport (`POST /mcp`).
//!
//! Replaces the supergateway stdio→HTTP bridge that used to front the brain for
//! the public Oracle connector. supergateway spawned one `ironjaw --mcp-server`
//! child per MCP session (~550 MB each) and never reaped it; every child opened
//! `brain.db`, so FTS init started failing with "database is locked" and recall
//! silently degraded to vector-only. Serving MCP from the gateway itself means
//! one process, one brain, one database handle.
//!
//! Only the JSON response mode of the Streamable HTTP transport is implemented.
//! The brain has no server-initiated messages, so the optional SSE stream (`GET`)
//! and session lifecycle (`DELETE`) are answered with 405, which the spec allows.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use ironjaw_mcp::McpServer;
use tracing::{debug, warn};

use crate::state::AppState;

/// Wiring for the native MCP endpoint. Present on [`AppState`] only when
/// `[server] mcp_token` is set; `None` means the route is never registered.
#[derive(Clone)]
pub struct McpHttp {
    /// The MCP server with brain tools registered and a handler bound to the
    /// gateway's own [`ironjaw_brain::Brain`].
    pub server: Arc<McpServer>,
    /// Bearer token every request must present.
    pub token: String,
}

/// Whether the request carries `Authorization: Bearer <token>`.
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    // An empty configured token can never match — the route is not registered in
    // that case, but do not let a config regression turn into an open endpoint.
    if token.is_empty() {
        return false;
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|presented| presented == token)
}

/// Whether a peer address is on this machine.
///
/// Loopback callers skip the bearer token. This is not a hole: any process on workstation
/// can already run `ironjaw --mcp-server` and get the same 17 tools with no
/// credential at all, so local-means-trusted is the level the brain already sits at.
/// It lets local tooling (health checks, scripts, curl) hit the brain without
/// handling the secret.
///
/// It was built for Claude Desktop, and that did NOT pan out — see the note in
/// `CHANGELOG.md`. Desktop's custom connectors require an OAuth sign-in service, not
/// a header, so it stays on the stdio transport. Do not re-derive that; test it
/// against a real Desktop build before assuming otherwise.
///
/// Consequence worth knowing: the public front door reaches this process through a
/// reverse SSH tunnel that lands on 127.0.0.1, so remote traffic is exempt too. That
/// path is gated by its 36-char secret URL and an IP allowlist in jh-exit-2's Caddy,
/// which is what it was always gated by; the token is a LAN guard, since the gateway
/// binds 0.0.0.0. Pointing the tunnel at workstation's LAN address would re-arm the token
/// for remote callers, at the cost of making the brain depend on that NIC being up.
fn is_local(peer: Option<&SocketAddr>) -> bool {
    // No ConnectInfo means the router was served without peer addresses. Treat that
    // as remote so a missing `into_make_service_with_connect_info` fails closed.
    peer.is_some_and(|a| a.ip().is_loopback())
}

/// Handle one JSON-RPC message (`POST /mcp`).
///
/// Returns the JSON-RPC response for requests, or 202 with no body for
/// notifications, which is what the Streamable HTTP spec requires.
/// Largest JSON-RPC message accepted. A `tools/call` payload is a few KB; this is
/// only here so a runaway body cannot buffer without bound.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

pub async fn mcp_post(State(state): State<AppState>, req: Request) -> Response {
    let Some(mcp) = state.mcp_http.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Taken off the request rather than via an `Option<ConnectInfo<_>>` extractor:
    // ConnectInfo has no optional extractor in axum 0.8, and a non-optional one would
    // 500 on any router served without `into_make_service_with_connect_info`.
    let peer_addr = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    let headers = req.headers().clone();

    if !is_local(peer_addr.as_ref()) && !authorized(&headers, &mcp.token) {
        warn!(peer = ?peer_addr, "MCP HTTP: rejected unauthorized request");
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(bytes) => match String::from_utf8(bytes.to_vec()) {
            Ok(s) => s,
            Err(_) => return (StatusCode::BAD_REQUEST, "body is not UTF-8").into_response(),
        },
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };

    debug!(bytes = body.len(), "MCP HTTP: request");

    // `handle_request` is synchronous and its tool handler blocks on the brain via
    // `block_in_place`, so this parks the worker properly instead of stalling it.
    match mcp.server.handle_request(&body) {
        Some(response) => ([(header::CONTENT_TYPE, "application/json")], response).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// `GET`/`DELETE /mcp` — no SSE stream and no sessions to terminate.
pub async fn mcp_unsupported() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn accepts_matching_bearer_token() {
        assert!(authorized(&headers_with("Bearer s3cret"), "s3cret"));
    }

    #[test]
    fn rejects_wrong_token() {
        assert!(!authorized(&headers_with("Bearer nope"), "s3cret"));
    }

    #[test]
    fn rejects_missing_header() {
        assert!(!authorized(&HeaderMap::new(), "s3cret"));
    }

    #[test]
    fn rejects_non_bearer_scheme() {
        assert!(!authorized(&headers_with("Basic s3cret"), "s3cret"));
    }

    #[test]
    fn empty_configured_token_never_authorizes() {
        assert!(!authorized(&headers_with("Bearer "), ""));
        assert!(!authorized(&HeaderMap::new(), ""));
    }

    #[test]
    fn loopback_peers_are_local() {
        assert!(is_local(Some(&"127.0.0.1:51234".parse().unwrap())));
        assert!(is_local(Some(&"[::1]:51234".parse().unwrap())));
    }

    #[test]
    fn lan_and_public_peers_are_not_local() {
        assert!(!is_local(Some(&"192.0.2.20:51234".parse().unwrap())));
        assert!(!is_local(Some(&"192.168.1.44:51234".parse().unwrap())));
        assert!(!is_local(Some(&"160.79.106.34:443".parse().unwrap())));
    }

    #[test]
    fn missing_connect_info_fails_closed() {
        assert!(!is_local(None));
    }
}
