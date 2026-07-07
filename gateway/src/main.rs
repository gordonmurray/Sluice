use std::{env, sync::Arc};

use alloy_primitives::Address;
use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{any, get},
};
use rules::{Decision, RuleSet};
use x402_axum::X402Middleware;
use x402_chain_eip155::{KnownNetworkEip155, V2Eip155Exact};
use x402_types::networks::USDC;

/// Largest request body the proxy will buffer before forwarding.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Header carrying the caller id for per-caller pricing. Unauthenticated in
/// rung 1 — treat it as a pricing hint, not an identity claim.
const CALLER_HEADER: &str = "x-sluice-caller";

struct AppState {
    origin: String,
    strip_prefix: Option<String>,
    http: reqwest::Client,
    rules: Arc<RuleSet>,
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
    let rules_path = env::var("RULES_PATH").context("RULES_PATH is required")?;
    let strip_prefix = env::var("STRIP_PREFIX").ok().filter(|s| !s.is_empty());
    let bind = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    let rules_json = std::fs::read_to_string(&rules_path)
        .with_context(|| format!("cannot read rules table at {rules_path}"))?;
    let rules = Arc::new(RuleSet::from_json(&rules_json)?);

    let x402 = X402Middleware::try_from(facilitator_url.clone())
        .map_err(|e| anyhow::anyhow!("invalid FACILITATOR_URL: {e}"))?;

    // Price tags are derived per-request from the rules table. Free and
    // denied requests get no price tag (the x402 layer then forwards them
    // untouched); denial is enforced by the proxy handler behind the layer.
    let usdc = USDC::base();
    let pricer = {
        let rules = rules.clone();
        move |headers: &axum::http::HeaderMap, uri: &axum::http::Uri, _base: Option<&reqwest::Url>| {
            let caller = caller_id(headers);
            let decision = rules.decide(uri.path(), caller.as_deref());
            let usdc = usdc.clone();
            async move {
                match decision {
                    Decision::Paid { micro_usdc } => {
                        vec![V2Eip155Exact::price_tag(pay_to, usdc.amount(micro_usdc))]
                    }
                    Decision::Free | Decision::Deny => vec![],
                }
            }
        }
    };

    let state = Arc::new(AppState {
        origin: origin.trim_end_matches('/').to_string(),
        strip_prefix,
        http: reqwest::Client::new(),
        rules,
    });

    let app = Router::new()
        .route("/healthz", get(healthz))
        .fallback_service(
            any(proxy)
                .layer(x402.with_dynamic_price(pricer))
                .with_state(state.clone()),
        )
        .with_state(state);

    tracing::info!(%bind, %origin, %facilitator_url, %pay_to, %rules_path, "sluice gateway starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Reverse-proxy the request to the origin. Payment enforcement happens in
/// the x402 layer before this runs; the gateway itself never touches the
/// chain. Paths no rule covers are refused here (the layer attaches no price
/// tag to them, so they'd otherwise pass through unpaid).
async fn proxy(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let caller = caller_id(req.headers());
    if st.rules.decide(req.uri().path(), caller.as_deref()) == Decision::Deny {
        return (StatusCode::NOT_FOUND, "no route").into_response();
    }
    match forward(&st, req).await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::error!(error = %err, "proxy error");
            (StatusCode::BAD_GATEWAY, "upstream error").into_response()
        }
    }
}

async fn forward(st: &AppState, req: Request) -> anyhow::Result<Response> {
    let (parts, body) = req.into_parts();

    let mut path = parts.uri.path();
    if let Some(prefix) = &st.strip_prefix {
        path = strip_path_prefix(path, prefix);
    }
    let url = match parts.uri.query() {
        Some(q) => format!("{}{}?{}", st.origin, path, q),
        None => format!("{}{}", st.origin, path),
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
    let mut rb = st.http.request(method, &url);
    for (name, value) in &parts.headers {
        // The caller header is an unauthenticated gateway-side pricing hint;
        // never let it reach the origin looking like tenant identity.
        if name.as_str() == CALLER_HEADER {
            continue;
        }
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

/// The caller id used for pricing, or None. Deterministic on adversarial
/// input: duplicated, empty, or non-UTF-8 caller headers all collapse to
/// "no caller" (base price) — the same answer the pricing layer and the
/// proxy handler will both compute.
fn caller_id(headers: &axum::http::HeaderMap) -> Option<String> {
    let mut values = headers.get_all(CALLER_HEADER).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None; // duplicated header: refuse to pick one
    }
    let s = first.to_str().ok()?.trim();
    if s.is_empty() { None } else { Some(s.to_owned()) }
}

/// Strip `prefix` from `path` on whole-segment boundaries only: `/firn` is
/// stripped from `/firn/health` (→ `/health`) and `/firn` (→ `/`), but not
/// from `/firnabc`.
fn strip_path_prefix<'a>(path: &'a str, prefix: &str) -> &'a str {
    match path.strip_prefix(prefix) {
        Some("") => "/",
        Some(rest) if rest.starts_with('/') => rest,
        _ => path,
    }
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

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn strip_prefix_is_segment_aware() {
        assert_eq!(strip_path_prefix("/firn/health", "/firn"), "/health");
        assert_eq!(strip_path_prefix("/firn", "/firn"), "/");
        assert_eq!(strip_path_prefix("/firnabc", "/firn"), "/firnabc");
        assert_eq!(strip_path_prefix("/other", "/firn"), "/other");
    }

    #[test]
    fn caller_id_single_value() {
        let mut h = HeaderMap::new();
        h.insert(CALLER_HEADER, HeaderValue::from_static("tenant-a"));
        assert_eq!(caller_id(&h).as_deref(), Some("tenant-a"));
    }

    #[test]
    fn caller_id_rejects_duplicates_empties_and_junk() {
        let mut h = HeaderMap::new();
        h.append(CALLER_HEADER, HeaderValue::from_static("tenant-a"));
        h.append(CALLER_HEADER, HeaderValue::from_static("tenant-b"));
        assert_eq!(caller_id(&h), None);

        let mut h = HeaderMap::new();
        h.insert(CALLER_HEADER, HeaderValue::from_static(""));
        assert_eq!(caller_id(&h), None);

        let mut h = HeaderMap::new();
        h.insert(CALLER_HEADER, HeaderValue::from_bytes(b"\xff\xfe").unwrap());
        assert_eq!(caller_id(&h), None);

        assert_eq!(caller_id(&HeaderMap::new()), None);
    }
}
