//! MCP server for the matrix render engine.
//!
//! Protocol `2026-07-28`, served over Streamable HTTP or, for a spawning client, over
//! stdio. The application has no client-authentication path and all callers share
//! engine state.

pub mod files;
pub mod mcp;
pub mod state;
pub mod tools;

pub use state::{Engine, EngineError};
pub use tools::{FileValue, ToolError};

use mcp::{MatrixHandler, MediaBinaries};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;

/// The HTTP application: `/healthz` and the MCP transport at `/mcp`.
///
/// One constructor for the binary and wire-level tests, so both use the same assembly.
///
/// `allowed_hosts` is the complete `Host`-header allowlist for the MCP transport's
/// DNS-rebinding guard and must include each authority a client or proxy uses.
/// `/healthz` sits outside the guard.
pub fn router(
    engine: Arc<Engine>,
    binaries: MediaBinaries,
    allowed_hosts: Vec<String>,
    files: Option<Arc<files::FilePlane>>,
) -> axum::Router {
    let handler_files = files.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(MatrixHandler::new(engine.clone(), binaries.clone())
                .with_files(handler_files.clone()))
        },
        LocalSessionManager::default().into(),
        // Stateless for every protocol version, not only for clients negotiating
        // 2026-07-28: this server keeps nothing per connection, so a session would be a
        // fiction the transport had to maintain.
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_allowed_hosts(allowed_hosts),
    );

    let mut app = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .nest_service("/mcp", service);

    // The ingest route rides the same listener as `/mcp` deliberately. A trusted
    // intermediary reuses the addresses it pinned for the MCP endpoint only when the
    // transfer descriptor names that same host, so one shared origin is both the
    // simplest arrangement and the best-supported one: the operator's existing TLS
    // boundary and `Host` allowlist already cover it.
    if let Some(plane) = files {
        app = app.merge(files::ingest_router(plane));
    }
    app
}
