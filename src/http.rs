//! Authenticated Streamable HTTP transport shared by multiple MCP clients.
//!
//! One process binds one address and every compatible client addresses the same
//! `/mcp` endpoint, so all of them share one configured identity, one mode and
//! one database concurrency budget. Nothing is stored outside the process: a
//! second daemon on the same address fails at bind time instead of duplicating
//! the service.
//!
//! Every MCP request must carry exactly one `Authorization: Bearer <token>`
//! header with the value of `MSSQL_MCP_TOKEN`. This is a static access gate for
//! a locally bound daemon, not OAuth. The rmcp session is disabled, so each
//! request is served statelessly, and the rmcp Host and Origin validation stays
//! enabled: a supplied untrusted `Host` or any supplied `Origin` is refused
//! because the transport is not a browser CORS endpoint. The transport is also
//! stateless, so it never offers the native approval prompt: `--trust-elicitation`
//! is rejected at configuration time for HTTP and mutations fail closed without
//! the explicit `--yolo` bypass.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use subtle::ConstantTimeEq;

use crate::config::Settings;
use crate::server::McpServer;

/// Largest accepted request body, enforced while streaming so that a declared
/// or missing `Content-Length` and chunked uploads are bounded the same way.
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const UNAUTHORIZED_BODY: &str = r#"{"code":"unauthorized","message":"send exactly one Authorization: Bearer header with the configured token"}"#;

pub async fn run(mut settings: Settings, trust_elicitation: bool) -> Result<(), String> {
    let listen = settings.http_listen;
    let Some(token) = settings.token.take() else {
        return Err("MSSQL_MCP_TOKEN is required for the HTTP transport".to_owned());
    };
    let server = Arc::new(McpServer::new(settings, trust_elicitation).map_err(str::to_owned)?);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_max_request_body_bytes(MAX_REQUEST_BODY_BYTES)
            .enforce_origin_validation(),
    );
    let app = Router::new()
        .route_service("/mcp", service)
        .layer(middleware::from_fn_with_state(
            BearerAuth {
                token: Arc::from(token.as_str()),
            },
            authorize_request,
        ));
    let listener = bind(listen).await?;
    axum::serve(listener, app)
        .await
        .map_err(|error| format!("MCP HTTP server stopped unexpectedly: {error}"))
}

async fn bind(listen: SocketAddr) -> Result<tokio::net::TcpListener, String> {
    tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| match error.kind() {
            io::ErrorKind::AddrInUse => format!(
                "HTTP listen address {listen} is already in use; stop the other MCP HTTP server \
                 or choose another address with --http-listen"
            ),
            _ => format!("cannot listen on HTTP address {listen}: {error}"),
        })
}

#[derive(Clone)]
struct BearerAuth {
    token: Arc<str>,
}

async fn authorize_request(
    State(auth): State<BearerAuth>,
    request: Request,
    next: Next,
) -> Response {
    if authorize(request.headers(), &auth.token) {
        next.run(request).await
    } else {
        unauthorized()
    }
}

/// Accept exactly one `Authorization: Bearer` header matching `token`.
///
/// Duplicate headers are rejected even when one of them is correct, and the
/// comparison does not reveal the expected value through timing. The token is
/// never echoed back.
fn authorize(headers: &HeaderMap, token: &str) -> bool {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some((scheme, credential)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer")
        && bool::from(credential.as_bytes().ct_eq(token.as_bytes()))
}

fn unauthorized() -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, UNAUTHORIZED_BODY).into_response();
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}
