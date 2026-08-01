use crate::a2a;
use crate::dashboard;
use crate::handler::handle_connection;
use crate::mcp_http;
use crate::mobile_api::{self, MobileApiState};
use crate::openai_compat;
use crate::state::AppState;
use axum::{
    extract::{ws::WebSocketUpgrade, Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Json, Router,
};
use std::net::SocketAddr;
use tracing::info;

/// HTTP health check endpoint (GET /health)
async fn health_check(State(state): State<AppState>) -> Json<serde_json::Value> {
    let uptime = (chrono::Utc::now() - state.started_at).num_seconds();
    Json(serde_json::json!({
        "status": "ok",
        "server": "ironjaw",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "v4",
        "uptime_secs": uptime,
    }))
}

/// WebSocket upgrade handler (GET /ws)
async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.max_frame_size(65_536) // 64 KiB per frame
        .max_message_size(1_048_576) // 1 MiB per message (reassembled)
        .on_upgrade(move |socket| handle_connection(socket, state))
}

/// Serve agent-generated UI content from `data/a2ui/`.
async fn serve_a2ui(Path(path): Path<String>) -> Response {
    let base = std::path::Path::new("data/a2ui");
    let file_path = base.join(&path);

    // Security: reject obvious path traversal
    if path.contains("..") {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }

    // Security: canonicalize both paths and verify the file is under the base directory.
    // This catches encoded traversals that bypass the literal ".." check.
    let canon_base = match base.canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let canon_file = match file_path.canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    if !canon_file.starts_with(&canon_base) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match tokio::fs::read(&canon_file).await {
        Ok(bytes) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.to_string())], bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Build the axum Router
pub fn build_router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/health", get(health_check))
        .route("/ws", any(ws_upgrade))
        .route("/", any(ws_upgrade)) // OpenClaw compat: app connects to /
        .route("/dashboard", get(dashboard::dashboard_root))
        .route("/dashboard/{*path}", get(dashboard::dashboard_file))
        .route("/__ironjaw__/a2ui/{*path}", get(serve_a2ui));

    // Conditionally add OpenAI-compatible /v1/* endpoints
    if state.openai_compat {
        info!("Registering OpenAI-compatible /v1/* routes");
        router = router
            .route("/v1/models", get(openai_compat::list_models))
            .route("/v1/models/{model_id}", get(openai_compat::get_model))
            .route(
                "/v1/chat/completions",
                post(openai_compat::chat_completions),
            )
            .route("/v1/embeddings", post(openai_compat::embeddings));
    }

    // Native MCP endpoint — registered only when `[server] mcp_token` is set.
    if state.mcp_http.is_some() {
        info!("Registering native MCP endpoint at POST /mcp");
        router = router.route(
            "/mcp",
            post(mcp_http::mcp_post)
                .get(mcp_http::mcp_unsupported)
                .delete(mcp_http::mcp_unsupported),
        );
    }

    // Finalize main router state, then merge stateless/independently-stated routers
    let dashboard_state = state.clone();
    let mut app = router.with_state(state);

    // Dashboard REST API (/api/dashboard/*) + Command Center (/api/command/*)
    info!("Registering dashboard REST API routes");
    app = app.merge(dashboard::dashboard_routes(dashboard_state));

    // Mobile API (/api/mobile/*) -- uses its own MobileApiState
    info!("Registering mobile API routes");
    app = app.merge(mobile_api::mobile_api_routes(MobileApiState::new()));

    // A2A (Agent-to-Agent) protocol endpoints
    info!("Registering A2A protocol routes");
    let a2a_base_url = "http://localhost:18794".to_string();
    app = app.merge(a2a::a2a_router(&a2a_base_url));

    app
}

/// Start the WebSocket server and block until shutdown
pub async fn serve(host: &str, port: u16, state: AppState) -> anyhow::Result<()> {
    let app = build_router(state);
    let addr: SocketAddr = format!("{host}:{port}").parse()?;

    info!(
        address = %addr,
        "Ironjaw WebSocket server starting"
    );

    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;

    // SO_REUSEADDR means two different things on the two platforms, and only one
    // of them is what we want.
    //
    // On Unix it does what the name suggests: bind over a socket still sitting in
    // TIME_WAIT, so a restart does not have to wait out the timeout. An address
    // that is actively LISTENING is still refused.
    //
    // On Windows it also lets a completely separate process bind an address this
    // process is already listening on, as long as both set the option. Both sockets
    // then exist and which one receives a given connection is undefined. For a
    // gateway that authenticates clients with a bearer token that is a hijack:
    // anything running as the same user takes delivery of connections meant for us,
    // auth frames included.
    //
    // Measured on Windows 11, two sockets on 127.0.0.1 and the same port:
    //   both set SO_REUSEADDR  -> second bind SUCCEEDS (the hijack)
    //   neither sets it        -> second bind refused, WSAEADDRINUSE
    //   only the second sets it -> second bind refused
    //
    // So setting it unconditionally was the thing creating the hole. It is Unix-only
    // now, and Windows listeners have no TIME_WAIT problem that needs the workaround.
    //
    // Residual, not closed here: Windows still allows a bind to a *specific* address
    // when an existing socket holds the *wildcard* 0.0.0.0 on that port, even with
    // neither side setting SO_REUSEADDR. Closing that needs SO_EXCLUSIVEADDRUSE
    // (setsockopt option -5) on this socket, which socket2 does not surface.
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;

    socket.set_nonblocking(true)?;
    socket.bind(&socket2::SockAddr::from(addr))?;
    socket.listen(1024)?;
    let listener = tokio::net::TcpListener::from_std(std::net::TcpListener::from(socket))?;
    info!(address = %addr, "Listening");

    // Peer addresses are needed so /mcp can recognise loopback callers; see
    // `mcp_http::is_local`. Handlers take `Option<ConnectInfo<_>>`, so routers served
    // without this (the test harnesses) still work — they just look remote.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}
