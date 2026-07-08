use std::{
    env,
    sync::{Arc, RwLock},
    time::Duration,
};

use alloy_primitives::Address;
use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{any, get},
};
use rules::{Decision, RuleSet};
use x402_axum::X402Middleware;
use x402_chain_eip155::{KnownNetworkEip155, V2Eip155Exact};
use x402_types::networks::USDC;
use x402_types::proto::SettleResponse;

/// Largest request body the proxy will buffer before forwarding.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Header carrying the caller id for per-caller pricing. Unauthenticated in
/// rung 1 — treat it as a pricing hint, not an identity claim.
const CALLER_HEADER: &str = "x-sluice-caller";

/// Internal header carrying the per-request pricing decision from the
/// stamping middleware into the x402 price callback (which sees headers but
/// not request extensions). Stamped by the gateway only: inbound copies are
/// stripped before the decision is computed, and it is never forwarded to
/// the origin.
const DECISION_HEADER: &str = "x-sluice-decision";

/// The live rules table. Requests clone the inner `Arc` out (cheap, no lock
/// held across awaits); the reloader swaps a freshly parsed table in.
type SharedRules = Arc<RwLock<Arc<RuleSet>>>;

struct AppState {
    origin: String,
    strip_prefix: Option<String>,
    http: reqwest::Client,
    rules: SharedRules,
    pay_to: String,
    indexer_url: Option<String>,
    indexer_token: Option<String>,
}

/// A poisoned lock means a panic while *holding* it; the writer never panics
/// mid-swap (the swap is a pointer store), so recover the value either way.
fn current_rules(shared: &SharedRules) -> Arc<RuleSet> {
    shared.read().unwrap_or_else(|e| e.into_inner()).clone()
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
    let indexer_url = env::var("INDEXER_URL").ok().filter(|s| !s.is_empty());
    // The indexer requires its shared token; refusing to start beats every
    // receipt silently bouncing off a 401.
    let indexer_token = env::var("INDEXER_TOKEN").ok().filter(|s| !s.is_empty());
    if indexer_url.is_some() && indexer_token.is_none() {
        anyhow::bail!("INDEXER_TOKEN is required when INDEXER_URL is set");
    }
    let bind = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let origin_timeout: u64 = env::var("ORIGIN_TIMEOUT_SECS")
        .ok()
        .map(|s| s.parse())
        .transpose()
        .context("ORIGIN_TIMEOUT_SECS must be a whole number of seconds")?
        .unwrap_or(30);

    let rules_json = std::fs::read_to_string(&rules_path)
        .with_context(|| format!("cannot read rules table at {rules_path}"))?;
    let rules: SharedRules = Arc::new(RwLock::new(Arc::new(RuleSet::from_json(&rules_json)?)));

    // Re-read the rules file on a short interval and swap the table in
    // atomically; a malformed edit is logged and the old table keeps
    // serving. 0 disables reloading.
    let reload_secs: u64 = env::var("RULES_RELOAD_SECS")
        .ok()
        .map(|s| s.parse())
        .transpose()
        .context("RULES_RELOAD_SECS must be a whole number of seconds")?
        .unwrap_or(2);
    if reload_secs > 0 {
        let mut reloader = RulesReloader {
            shared: rules.clone(),
            last: rules_json.into_bytes(),
        };
        let rules_path = rules_path.clone();
        tokio::spawn(async move {
            // Log read failures on the transition only, not every tick.
            let mut read_failing = false;
            loop {
                tokio::time::sleep(Duration::from_secs(reload_secs)).await;
                match tokio::fs::read(&rules_path).await {
                    Ok(bytes) => {
                        read_failing = false;
                        match reloader.apply(bytes) {
                            Ok(true) => tracing::info!(%rules_path, "rules table reloaded"),
                            Ok(false) => {}
                            Err(e) => tracing::error!(
                                error = %e, %rules_path,
                                "invalid rules table; keeping the previous one"
                            ),
                        }
                    }
                    // Read failures keep the old table too.
                    Err(e) if !read_failing => {
                        read_failing = true;
                        tracing::error!(error = %e, %rules_path, "cannot read rules file");
                    }
                    Err(_) => {}
                }
            }
        });
    }

    // Settle before forwarding: the origin never does unpaid work, and the
    // settlement lands in the request extensions for the indexer.
    let x402 = X402Middleware::try_from(facilitator_url.clone())
        .map_err(|e| anyhow::anyhow!("invalid FACILITATOR_URL: {e}"))?
        .settle_before_execution();

    // Price tags come from the decision stamped by `stamp_decision` — the
    // rules table is read exactly once per request, so a mid-request reload
    // cannot price under one table and forward under another. Free and
    // denied requests get no price tag (the x402 layer then forwards them
    // untouched); denial is enforced by the proxy handler behind the layer.
    let usdc = USDC::base();
    let pricer = move |headers: &axum::http::HeaderMap,
                       _uri: &axum::http::Uri,
                       _base: Option<&reqwest::Url>| {
        // Missing/undecodable stamp cannot happen behind the middleware;
        // fail closed (no price tag, handler denies) if it somehow does.
        let decision = decode_decision(headers.get(DECISION_HEADER)).unwrap_or(Decision::Deny);
        let usdc = usdc.clone();
        async move {
            match decision {
                Decision::Paid { micro_usdc } => {
                    vec![V2Eip155Exact::price_tag(pay_to, usdc.amount(micro_usdc))]
                }
                Decision::Free | Decision::Deny => vec![],
            }
        }
    };

    let state = Arc::new(AppState {
        origin: origin.trim_end_matches('/').to_string(),
        strip_prefix,
        http: build_http_client(origin_timeout),
        rules,
        pay_to: format!("{pay_to}"),
        indexer_url,
        indexer_token,
    });

    // Layer order (outermost first): stamp_decision, x402, proxy — the
    // stamp must exist before the x402 layer prices the request.
    let app = Router::new()
        .route("/healthz", get(healthz))
        .fallback_service(
            any(proxy)
                .layer(x402.with_dynamic_price(pricer))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    stamp_decision,
                ))
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

/// Decide the request's pricing exactly once, before the x402 layer, and
/// stamp it on the request: as an extension for the proxy handler and as an
/// internal header for the x402 price callback (which sees headers, not
/// extensions). One read of the live rules table per request means a
/// concurrent reload cannot split a request across two tables — priced
/// under one, forwarded (or denied) under another.
async fn stamp_decision(
    State(st): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    // Never trust an inbound stamp.
    req.headers_mut().remove(DECISION_HEADER);
    let caller = caller_id(req.headers());
    let decision = if path_is_suspicious(req.uri().path()) {
        Decision::Deny
    } else {
        current_rules(&st.rules).decide(req.uri().path(), caller.as_deref())
    };
    req.headers_mut()
        .insert(DECISION_HEADER, encode_decision(decision));
    req.extensions_mut().insert(decision);
    next.run(req).await
}

fn encode_decision(d: Decision) -> HeaderValue {
    let s = match d {
        Decision::Free => "free".to_string(),
        Decision::Deny => "deny".to_string(),
        Decision::Paid { micro_usdc } => format!("paid:{micro_usdc}"),
    };
    // Always plain ASCII, so this cannot fail.
    HeaderValue::from_str(&s).expect("decision encoding is ASCII")
}

fn decode_decision(v: Option<&HeaderValue>) -> Option<Decision> {
    match v?.to_str().ok()? {
        "free" => Some(Decision::Free),
        "deny" => Some(Decision::Deny),
        s => Some(Decision::Paid {
            micro_usdc: s.strip_prefix("paid:")?.parse().ok()?,
        }),
    }
}

/// Reverse-proxy the request to the origin. Payment enforcement happens in
/// the x402 layer before this runs; the gateway itself never touches the
/// chain. Paths no rule covers are refused here (the layer attaches no price
/// tag to them, so they'd otherwise pass through unpaid).
async fn proxy(State(st): State<Arc<AppState>>, req: Request) -> Response {
    if path_is_suspicious(req.uri().path()) {
        return (StatusCode::BAD_REQUEST, "malformed path").into_response();
    }
    let caller = caller_id(req.headers());
    // The decision stamped before the x402 layer — the same one the request
    // was priced under. Fail closed if it is somehow absent.
    let decision = req
        .extensions()
        .get::<Decision>()
        .copied()
        .unwrap_or(Decision::Deny);
    if decision == Decision::Deny {
        return (StatusCode::NOT_FOUND, "no route").into_response();
    }

    // The x402 layer stores the settlement result as a request extension.
    // It is reported to the indexer only after the origin outcome is known,
    // so the receipt records what the payment actually bought — that is the
    // paid-but-failed policy (see migrations/0002): no automatic retry or
    // refund, but every settlement lands in the payments table with the
    // status the client got, and refunds are an operator decision from
    // there. The cost of reporting late: a gateway crash mid-request loses
    // the receipt (fire-and-forget could always drop one; the chain remains
    // the source of truth).
    let settlement = req
        .extensions()
        .get::<Option<SettleResponse>>()
        .cloned()
        .flatten();
    let path = req.uri().path().to_owned();

    let resp = match forward(&st, req).await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::error!(error = %err, "proxy error");
            (StatusCode::BAD_GATEWAY, "upstream error").into_response()
        }
    };
    if let Some(settlement) = settlement {
        report_settlement(
            &st,
            settlement,
            &path,
            caller.as_deref(),
            decision,
            resp.status().as_u16(),
        );
    }
    resp
}

/// Bounded patience with the origin. Without a timeout, an origin that
/// accepts the connection and then stalls leaves a *settled* request hanging
/// forever — never answered, never reported to the indexer. With it, a stall
/// becomes a 502 the client sees and the receipt records. This is a
/// connect/inter-read timeout, not a whole-request cap: long streamed
/// responses are fine as long as bytes keep flowing.
fn build_http_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(timeout_secs.min(10)))
        .read_timeout(Duration::from_secs(timeout_secs))
        .build()
        .expect("reqwest client construction cannot fail with static options")
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
        // The caller header is an unauthenticated gateway-side pricing hint,
        // and the decision stamp is gateway-internal; neither may reach the
        // origin.
        if name.as_str() == CALLER_HEADER || name.as_str() == DECISION_HEADER {
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

/// Swaps freshly parsed rules tables into the shared handle.
///
/// `last` holds the bytes of the previous read — good or bad — so each
/// distinct file content is parsed (and, when invalid, logged) exactly once
/// rather than on every poll tick. Requests are unaffected by mid-flight
/// swaps: `stamp_decision` reads the table once per request and the stamped
/// decision travels with it.
struct RulesReloader {
    shared: SharedRules,
    last: Vec<u8>,
}

impl RulesReloader {
    /// Ok(true): new table swapped in. Ok(false): file unchanged. Err: the
    /// new content does not parse; the running table is left untouched.
    fn apply(&mut self, bytes: Vec<u8>) -> Result<bool, rules::RuleError> {
        if bytes == self.last {
            return Ok(false);
        }
        let parsed = std::str::from_utf8(&bytes)
            .map_err(|e| rules::RuleError::Json(format!("rules file is not UTF-8: {e}")))
            .and_then(|s| RuleSet::from_json(s));
        self.last = bytes;
        match parsed {
            Ok(ruleset) => {
                *self.shared.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(ruleset);
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }
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

/// Fire-and-forget a settlement receipt to the indexer. Failures are logged,
/// never propagated — indexing is bookkeeping, not part of the paid request.
fn report_settlement(
    st: &AppState,
    settlement: SettleResponse,
    path: &str,
    caller: Option<&str>,
    decision: Decision,
    origin_status: u16,
) {
    let Some(indexer_url) = st.indexer_url.clone() else {
        return;
    };
    // Present whenever indexer_url is (enforced at startup).
    let Some(indexer_token) = st.indexer_token.clone() else {
        return;
    };
    let amount = match decision {
        Decision::Paid { micro_usdc } => micro_usdc as i64,
        _ => return, // settlement without a paid decision cannot happen
    };
    // Read the fields via JSON so this stays agnostic to the exact
    // SettleResponse struct layout (it is the wire format either way), but
    // validate them — a silently-null field would fail indexer-side and
    // masquerade as a transient error.
    let v = match serde_json::to_value(&settlement) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "cannot serialize settlement");
            return;
        }
    };
    let (Some(tx_hash), Some(network), Some(payer), Some(success)) = (
        v["transaction"].as_str(),
        v["network"].as_str(),
        v["payer"].as_str(),
        v["success"].as_bool(),
    ) else {
        tracing::error!(settlement = %v, "settlement missing expected fields; not indexing");
        return;
    };
    // amount/pay_to are what the gateway charged (see migrations/0001);
    // the v2 SettleResponse carries no amount to cross-check against.
    let receipt = serde_json::json!({
        "tx_hash": tx_hash,
        "network": network,
        "payer": payer,
        "pay_to": st.pay_to,
        "amount_micro_usdc": amount,
        "path": path,
        "caller": caller,
        "success": success,
        "origin_status": origin_status,
    });
    let http = st.http.clone();
    tokio::spawn(async move {
        let res = http
            .post(format!("{indexer_url}/receipts"))
            .bearer_auth(indexer_token)
            .json(&receipt)
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => tracing::error!(status = %r.status(), "indexer rejected receipt"),
            Err(e) => tracing::error!(error = %e, "cannot reach indexer"),
        }
    });
}

/// Path canonicalisation policy: match raw, forward raw, reject ambiguity.
///
/// Rules match the request path byte-for-byte and the origin receives it
/// unmodified. The gateway never decodes or normalises — any rewrite would be
/// a second interpretation of the path that can disagree with the origin's,
/// and the two disagreeing is exactly how a request gets priced as one route
/// and served as another.
///
/// The flip side: anything the origin might interpret differently than the
/// raw bytes the rules matched is rejected up front (400, no price tag):
///
/// - Percent-encoding, in any case. An origin that decodes `%2F` turns one
///   segment into two (`/a%2F..%2Fadmin` becomes `/a/../admin`), and an
///   encoded alias like `/firn/%68ealth` would be priced by the `/firn` rule
///   while the origin serves `/firn/health` — a cheaper prefix buying a more
///   expensive route. Rejecting `%` outright closes the whole class, encoded
///   dot segments and mixed-case variants included. HTTP request paths are
///   ASCII on the wire, so this also means no unicode ever reaches matching
///   and normalisation questions cannot arise.
/// - Backslashes: some origins treat `\` as `/`.
/// - Semicolons: servlet-style origins strip `;matrix=params` before
///   routing, so `/firn/health;v=1` would be priced by the shorter `/firn`
///   rule (it does not byte-match `/firn/health`) and served as
///   `/firn/health`.
/// - Dot segments and empty segments: URL parsers downstream collapse them,
///   so `/a/query/../admin` would be priced as `/a/query` and delivered as
///   `/a/admin`.
///
/// Trailing slashes are allowed and not equivalent: `/firn` and `/firn/` are
/// forwarded as-is and are distinct paths to the origin, but both match a
/// `/firn` rule (prefix matching is on whole segments), so they price the
/// same.
///
/// "Forwarded as-is" has one deliberate exception: the configured
/// `STRIP_PREFIX` removes a leading whole-segment prefix after pricing.
/// Rules price the external path; the origin sees the stripped one.
fn path_is_suspicious(path: &str) -> bool {
    path.contains('%')
        || path.contains('\\')
        || path.contains(';')
        || path.contains("//")
        || path.split('/').any(|seg| matches!(seg, "." | ".."))
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
    fn suspicious_paths_are_flagged() {
        for bad in [
            // dot segments, plain and percent-encoded
            "/a/query/../admin",
            "/a/./b",
            "/a/%2e%2e/b",
            "/a/%2E%2E/b",
            "/a/.%2e/b",
            "/a/%2e/b",
            // empty segments
            "/a//b",
            // percent-encoding anywhere, any case, decodable or not
            "/a%2Fb",
            "/a%2fb",
            "/a/%68ealth",
            "/a/b%2ec",
            "/a/b%20c",
            "/a/%zz",
            // backslashes, raw and encoded
            "/a\\b",
            "/a%5Cb",
            "/a%5cb",
            // semicolons: matrix-param stripping origins reroute these
            "/firn/health;v=1",
            "/firn/health/;v=1",
            "/firn;v=1/health",
        ] {
            assert!(path_is_suspicious(bad), "{bad} should be suspicious");
        }
        for ok in ["/", "/a/b", "/a.b/c", "/a/b.json", "/a/..b", "/a/b/", "/a-b_c~d/e"] {
            assert!(!path_is_suspicious(ok), "{ok} should be fine");
        }
    }

    fn table(price: &str) -> String {
        format!(r#"{{ "rules": [ {{ "prefix": "/p", "price_usdc": "{price}" }} ] }}"#)
    }

    fn price_of(shared: &SharedRules) -> Decision {
        current_rules(shared).decide("/p", None)
    }

    #[test]
    fn reload_swaps_a_valid_table_and_keeps_the_old_on_a_bad_one() {
        let initial = table("0.01");
        let shared: SharedRules = Arc::new(RwLock::new(Arc::new(
            RuleSet::from_json(&initial).unwrap(),
        )));
        let mut r = RulesReloader {
            shared: shared.clone(),
            last: initial.into_bytes(),
        };
        assert_eq!(price_of(&shared), Decision::Paid { micro_usdc: 10_000 });

        // Unchanged bytes: no reparse, no swap.
        assert_eq!(r.apply(table("0.01").into_bytes()), Ok(false));

        // A valid edit takes effect.
        assert_eq!(r.apply(table("0.09").into_bytes()), Ok(true));
        assert_eq!(price_of(&shared), Decision::Paid { micro_usdc: 90_000 });

        // Malformed JSON: error out, old table keeps serving...
        assert!(r.apply(b"{ not json".to_vec()).is_err());
        assert_eq!(price_of(&shared), Decision::Paid { micro_usdc: 90_000 });

        // ...and the same bad bytes are not reparsed on the next tick.
        assert_eq!(r.apply(b"{ not json".to_vec()), Ok(false));

        // Valid JSON that fails rule validation is rejected the same way.
        assert!(
            r.apply(br#"{ "rules": [ { "prefix": "/p" } ] }"#.to_vec())
                .is_err()
        );
        assert_eq!(price_of(&shared), Decision::Paid { micro_usdc: 90_000 });

        // Non-UTF-8 content is an error, not a panic.
        assert!(r.apply(vec![0xff, 0xfe, 0x00]).is_err());
        assert_eq!(price_of(&shared), Decision::Paid { micro_usdc: 90_000 });

        // Recovery: fixing the file swaps the fix in.
        assert_eq!(r.apply(table("0.05").into_bytes()), Ok(true));
        assert_eq!(price_of(&shared), Decision::Paid { micro_usdc: 50_000 });
    }

    type ReceiptRx = tokio::sync::mpsc::Receiver<(Option<String>, serde_json::Value)>;

    /// An in-process indexer stand-in that captures receipt POSTs, plus the
    /// state pointing the proxy at `origin` and at it.
    async fn state_with_receipt_capture(origin: String) -> (Arc<AppState>, ReceiptRx) {
        let (tx, rx) = tokio::sync::mpsc::channel::<(Option<String>, serde_json::Value)>(1);
        let capture = move |headers: axum::http::HeaderMap,
                            axum::Json(v): axum::Json<serde_json::Value>| {
            let tx = tx.clone();
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|h| h.to_str().ok())
                    .map(String::from);
                let _ = tx.send((auth, v)).await;
                StatusCode::NO_CONTENT
            }
        };
        let indexer = Router::new().route("/receipts", axum::routing::post(capture));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let indexer_addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, indexer).await.unwrap() });

        let table = r#"{ "rules": [ { "prefix": "/p", "price_usdc": "0.01" } ] }"#;
        let state = Arc::new(AppState {
            origin,
            strip_prefix: None,
            http: build_http_client(1),
            rules: Arc::new(RwLock::new(Arc::new(RuleSet::from_json(table).unwrap()))),
            pay_to: "0xpayto".into(),
            indexer_url: Some(format!("http://{indexer_addr}")),
            indexer_token: Some("sekrit".into()),
        });
        (state, rx)
    }

    /// A request as it arrives at the proxy for a settled paid route: the
    /// stamped decision and the x402 layer's settlement in the extensions.
    fn settled_request(body: Body) -> Request {
        let settlement: SettleResponse = serde_json::from_value(serde_json::json!({
            "network": "eip155:8453",
            "payer": "0xpayer",
            "success": true,
            "transaction": "0xabc",
        }))
        .expect("SettleResponse wire shape");
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri("/p")
            .body(body)
            .unwrap();
        req.extensions_mut()
            .insert(Decision::Paid { micro_usdc: 10_000 });
        req.extensions_mut().insert(Some(settlement));
        req
    }

    async fn next_receipt(rx: &mut ReceiptRx) -> (Option<String>, serde_json::Value) {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("settlement was not reported")
            .unwrap()
    }

    /// The paid-but-failed policy's acceptance test: origin down after
    /// payment. The client gets 502, and the settlement is still reported —
    /// with origin_status 502 — so the payments table can answer "who paid
    /// and got nothing" for the operator-driven refund flow.
    #[tokio::test]
    async fn paid_request_with_origin_down_returns_502_and_reports_it() {
        // An origin that refuses connections: bind a port, then drop it.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let (state, mut rx) = state_with_receipt_capture(format!("http://{dead_addr}")).await;
        let resp = proxy(State(state), settled_request(Body::empty())).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let (auth, receipt) = next_receipt(&mut rx).await;
        assert_eq!(auth.as_deref(), Some("Bearer sekrit"));
        assert_eq!(receipt["origin_status"], 502);
        assert_eq!(receipt["path"], "/p");
        assert_eq!(receipt["amount_micro_usdc"], 10_000);
        assert_eq!(receipt["tx_hash"], "0xabc");
        assert_eq!(receipt["payer"], "0xpayer");
        assert_eq!(receipt["success"], true);
    }

    /// An origin that accepts the connection and then stalls: the read
    /// timeout turns it into a 502 that is answered and reported, instead of
    /// a settled request hanging unreported forever.
    #[tokio::test]
    async fn paid_request_with_stalled_origin_times_out_and_reports_502() {
        let stall = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stall_addr = stall.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                if let Ok((sock, _)) = stall.accept().await {
                    held.push(sock); // accept, never respond
                }
            }
        });

        // The helper builds the client with a 1s read timeout.
        let (state, mut rx) = state_with_receipt_capture(format!("http://{stall_addr}")).await;
        let resp = proxy(State(state), settled_request(Body::empty())).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let (_, receipt) = next_receipt(&mut rx).await;
        assert_eq!(receipt["origin_status"], 502);
    }

    /// The origin's own status is recorded verbatim, 5xx or not.
    #[tokio::test]
    async fn origin_status_is_recorded_verbatim() {
        let origin = Router::new().fallback(|| async { (StatusCode::NOT_FOUND, "nope") });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, origin).await.unwrap() });

        let (state, mut rx) = state_with_receipt_capture(format!("http://{origin_addr}")).await;
        let resp = proxy(State(state), settled_request(Body::empty())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let (_, receipt) = next_receipt(&mut rx).await;
        assert_eq!(receipt["origin_status"], 404);
    }

    /// A settled request whose body the gateway itself refuses: the 413 is
    /// what the payment bought, and the receipt says so.
    #[tokio::test]
    async fn paid_request_with_oversized_body_reports_413() {
        // Origin must not be reached; a refusing port proves it wasn't
        // (reaching it would yield 502, not 413).
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let (state, mut rx) = state_with_receipt_capture(format!("http://{dead_addr}")).await;
        let big = Body::from(vec![0u8; MAX_BODY_BYTES + 1]);
        let resp = proxy(State(state), settled_request(big)).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let (_, receipt) = next_receipt(&mut rx).await;
        assert_eq!(receipt["origin_status"], 413);
    }

    #[test]
    fn decision_header_roundtrip() {
        for d in [
            Decision::Free,
            Decision::Deny,
            Decision::Paid { micro_usdc: 123 },
        ] {
            assert_eq!(decode_decision(Some(&encode_decision(d))), Some(d));
        }
        assert_eq!(decode_decision(None), None);
        for bad in ["paid:", "paid:x", "PAID:5", "", "gratis"] {
            assert_eq!(
                decode_decision(Some(&HeaderValue::from_static(bad))),
                None,
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn stamp_strips_spoofed_decisions_and_stamps_both_channels() {
        use tower::ServiceExt;

        let table = r#"{ "rules": [ { "prefix": "/p", "price_usdc": "0.01" } ] }"#;
        let rules: SharedRules =
            Arc::new(RwLock::new(Arc::new(RuleSet::from_json(table).unwrap())));
        let state = Arc::new(AppState {
            origin: "http://unused".into(),
            strip_prefix: None,
            http: reqwest::Client::new(),
            rules,
            pay_to: "0x0".into(),
            indexer_url: None,
            indexer_token: None,
        });

        // Echo what the layers downstream of the middleware actually see.
        async fn probe(req: Request) -> String {
            format!(
                "ext={:?} hdr={:?}",
                req.extensions().get::<Decision>(),
                req.headers()
                    .get(DECISION_HEADER)
                    .and_then(|v| v.to_str().ok())
            )
        }
        let app = Router::new().fallback_service(any(probe).layer(
            axum::middleware::from_fn_with_state(state, stamp_decision),
        ));

        // A client claiming "free" on a paid route gets its stamp replaced.
        let req = axum::http::Request::builder()
            .uri("/p")
            .header(DECISION_HEADER, "free")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let s = String::from_utf8(body.to_vec()).unwrap();
        assert!(s.contains("ext=Some(Paid { micro_usdc: 10000 })"), "{s}");
        assert!(s.contains(r#"hdr=Some("paid:10000")"#), "{s}");

        // Unmatched path: denied on both channels.
        let req = axum::http::Request::builder()
            .uri("/other")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let s = String::from_utf8(body.to_vec()).unwrap();
        assert!(s.contains("ext=Some(Deny)"), "{s}");
        assert!(s.contains(r#"hdr=Some("deny")"#), "{s}");
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
