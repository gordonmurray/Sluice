use std::{env, sync::Arc};

use alloy_primitives::Address;
use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use x402_axum::X402Middleware;
use x402_chain_eip155::{KnownNetworkEip155, V2Eip155Exact};
use x402_types::networks::USDC;

/// Largest request body the proxy will buffer before forwarding.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

struct Upstream {
    origin: String,
    http: reqwest::Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let origin = env::var("ORIGIN_URL").context("ORIGIN_URL is required")?;
    let facilitator_url = env::var("FACILITATOR_URL").context("FACILITATOR_URL is required")?;
    let pay_to: Address = env::var("PAY_TO")
        .context("PAY_TO is required")?
        .parse()
        .context("PAY_TO is not a valid EVM address")?;
    let price = env::var("PRICE_USDC").unwrap_or_else(|_| "0.01".to_string());
    let bind = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    let x402 = X402Middleware::try_from(facilitator_url.clone())
        .map_err(|e| anyhow::anyhow!("invalid FACILITATOR_URL: {e}"))?;
    let price_tag = V2Eip155Exact::price_tag(
        pay_to,
        USDC::base()
            .parse(price.as_str())
            .map_err(|e| anyhow::anyhow!("PRICE_USDC is not a valid USDC amount: {e}"))?,
    );

    let upstream = Arc::new(Upstream {
        origin: origin.trim_end_matches('/').to_string(),
        http: reqwest::Client::new(),
    });

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/firn/health",
            get(proxy).layer(x402.with_price_tag(price_tag)),
        )
        .with_state(upstream);

    tracing::info!(%bind, %origin, %facilitator_url, %pay_to, %price, "sluice gateway starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Reverse-proxy the request to the origin, stripping the `/firn` prefix.
/// Payment enforcement happens in the x402 layer before this runs; the
/// gateway itself never touches the chain.
async fn proxy(State(up): State<Arc<Upstream>>, req: Request) -> Response {
    match forward(&up, req).await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::error!(error = %err, "proxy error");
            (StatusCode::BAD_GATEWAY, "upstream error").into_response()
        }
    }
}

async fn forward(up: &Upstream, req: Request) -> anyhow::Result<Response> {
    let (parts, body) = req.into_parts();

    let path = parts
        .uri
        .path()
        .strip_prefix("/firn")
        .unwrap_or(parts.uri.path());
    let url = match parts.uri.query() {
        Some(q) => format!("{}{}?{}", up.origin, path, q),
        None => format!("{}{}", up.origin, path),
    };

    let body = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return Ok(
                (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response()
            );
        }
    };
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?;

    let conn_named = connection_named(&parts.headers);
    let mut rb = up.http.request(method, &url);
    for (name, value) in &parts.headers {
        if !skip_header(name.as_str()) && !conn_named.contains(name.as_str()) {
            rb = rb.header(name.as_str(), value.as_bytes());
        }
    }

    let origin_resp = rb.body(body).send().await?;
    let status = origin_resp.status();
    let conn_named = connection_named(origin_resp.headers());

    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in origin_resp.headers() {
        if !skip_header(name.as_str()) && !conn_named.contains(name.as_str()) {
            builder = builder.header(name.as_str(), value.as_bytes());
        }
    }
    // Stream the origin body through instead of buffering it; origin
    // responses have no size cap.
    Ok(builder.body(Body::from_stream(origin_resp.bytes_stream()))?)
}

/// Header names nominated by a `Connection` header are hop-by-hop too
/// (RFC 9110 §7.6.1) and must not be forwarded.
fn connection_named(headers: &axum::http::HeaderMap) -> std::collections::HashSet<String> {
    headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .collect()
}

/// Hop-by-hop headers (plus host/content-length, which the HTTP clients set
/// themselves) that must not be forwarded in either direction.
fn skip_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
