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

/// Legacy inbound caller claim. It no longer influences pricing — identity
/// comes from the API key — but it is still stripped before forwarding so
/// nothing reaching the origin looks like gateway-vouched tenant identity.
const CALLER_HEADER: &str = "x-sluice-caller";

/// Header presenting a caller's API key. The key maps to a caller id via
/// the table at CALLERS_PATH; per-caller prices apply only to callers
/// authenticated this way. Stripped before forwarding to the origin.
const API_KEY_HEADER: &str = "x-sluice-api-key";

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
    /// API key -> caller id. Loaded once at startup (unlike the rules
    /// table, key changes are rare enough that a restart is acceptable).
    caller_keys: std::collections::HashMap<String, String>,
    pay_to: String,
    indexer_url: Option<String>,
    indexer_token: Option<String>,
}

/// The caller resolved from the API key, stamped on the request by
/// `stamp_decision` so pricing and receipts agree on identity.
#[derive(Clone)]
struct ResolvedCaller(Option<String>);

/// A poisoned lock means a panic while *holding* it; the writer never panics
/// mid-swap (the swap is a pointer store), so recover the value either way.
fn current_rules(shared: &SharedRules) -> Arc<RuleSet> {
    shared.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Request metrics, recorded once per proxied request in `stamp_decision`
/// (which wraps the x402 layer and the proxy, so 402 quotes, denials, and
/// proxied responses are all one observation each). Label cardinality is
/// bounded: three decisions, and HTTP statuses the gateway actually emits.
struct GatewayMetrics {
    registry: prometheus::Registry,
    requests: prometheus::IntCounterVec,
    duration: prometheus::HistogramVec,
}

fn metrics() -> &'static GatewayMetrics {
    use std::sync::OnceLock;
    static METRICS: OnceLock<GatewayMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let registry = prometheus::Registry::new();
        let requests = prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "sluice_gateway_requests_total",
                "Requests through the gateway by pricing decision and response status",
            ),
            &["decision", "status"],
        )
        .expect("static metric definition");
        let duration = prometheus::HistogramVec::new(
            prometheus::HistogramOpts::new(
                "sluice_gateway_request_duration_seconds",
                "Wall-clock request duration through the gateway by pricing decision",
            ),
            &["decision"],
        )
        .expect("static metric definition");
        registry
            .register(Box::new(requests.clone()))
            .expect("first registration");
        registry
            .register(Box::new(duration.clone()))
            .expect("first registration");
        GatewayMetrics {
            registry,
            requests,
            duration,
        }
    })
}

fn decision_label(d: Decision) -> &'static str {
    match d {
        Decision::Free => "free",
        Decision::Paid { .. } => "paid",
        Decision::Deny => "deny",
    }
}

async fn serve_metrics() -> Response {
    use prometheus::Encoder;
    let mut buf = Vec::new();
    let encoder = prometheus::TextEncoder::new();
    if let Err(e) = encoder.encode(&metrics().registry.gather(), &mut buf) {
        tracing::error!(error = %e, "cannot encode metrics");
        return (StatusCode::INTERNAL_SERVER_ERROR, "encode error").into_response();
    }
    (
        StatusCode::OK,
        [("content-type", encoder.format_type().to_string())],
        buf,
    )
        .into_response()
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
    // Optional: without a key table no caller authenticates and every
    // request is priced at the base rate.
    let caller_keys = match env::var("CALLERS_PATH").ok().filter(|s| !s.is_empty()) {
        Some(path) => {
            let json = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read caller keys at {path}"))?;
            let keys = parse_caller_keys(&json)?;
            tracing::info!(count = keys.len(), %path, "caller API keys loaded");
            keys
        }
        None => Default::default(),
    };
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

    let state = Arc::new(AppState {
        origin: origin.trim_end_matches('/').to_string(),
        strip_prefix,
        http: build_http_client(origin_timeout),
        rules,
        caller_keys,
        pay_to: format!("{pay_to}"),
        indexer_url,
        indexer_token,
    });

    let app = build_app(state, &facilitator_url, pay_to)?;

    tracing::info!(%bind, %origin, %facilitator_url, %pay_to, %rules_path, "sluice gateway starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// The full request-handling stack, exactly as served: stamp_decision
/// (outermost), the x402 payment layer, then the proxy handler. Factored out
/// of main so the integration tests drive the same stack production runs.
fn build_app(
    state: Arc<AppState>,
    facilitator_url: &str,
    pay_to: Address,
) -> anyhow::Result<Router> {
    // Settle before forwarding: the origin never does unpaid work, and the
    // settlement lands in the request extensions for the indexer.
    let x402 = X402Middleware::try_from(facilitator_url.to_string())
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

    // Layer order (outermost first): stamp_decision, x402, proxy — the
    // stamp must exist before the x402 layer prices the request.
    // Reserved gateway paths: /healthz and /metrics belong to the gateway
    // itself and are never proxied, whatever the rules table says (explicit
    // routes win over the fallback proxy). An origin's own endpoints at
    // those names stay reachable under its configured prefix, e.g.
    // /firn/metrics -> origin /metrics.
    Ok(Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(serve_metrics))
        .fallback_service(
            any(proxy)
                .layer(x402.with_dynamic_price(pricer))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    stamp_decision,
                ))
                .with_state(state.clone()),
        )
        .with_state(state))
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
    // Identity comes from the API key alone; a bare x-sluice-caller claim
    // is not consulted, so claiming a caller without credentials prices at
    // the base rate.
    let caller = resolve_caller(req.headers(), &st.caller_keys);
    let decision = if path_is_suspicious(req.uri().path()) {
        Decision::Deny
    } else {
        current_rules(&st.rules).decide(req.uri().path(), caller.as_deref())
    };
    req.headers_mut()
        .insert(DECISION_HEADER, encode_decision(decision));
    req.extensions_mut().insert(decision);
    req.extensions_mut().insert(ResolvedCaller(caller));

    // One observation per request, wrapping the x402 layer and the proxy:
    // a 402 quote, a denial, and a proxied response each count once, with
    // the status the client actually received.
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    let label = decision_label(decision);
    metrics()
        .duration
        .with_label_values(&[label])
        .observe(start.elapsed().as_secs_f64());
    metrics()
        .requests
        .with_label_values(&[label, resp.status().as_str()])
        .inc();
    resp
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
    // The caller resolved by stamp_decision — receipts must record the
    // identity the request was priced under, not a second resolution.
    let caller = req
        .extensions()
        .get::<ResolvedCaller>()
        .cloned()
        .unwrap_or(ResolvedCaller(None))
        .0;
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
        // Gateway-side identity and pricing headers must not reach the
        // origin: the API key is a credential, the caller claim would look
        // like vouched tenant identity, and the decision stamp is internal.
        if name.as_str() == CALLER_HEADER
            || name.as_str() == API_KEY_HEADER
            || name.as_str() == DECISION_HEADER
        {
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

/// The caller id for pricing: the API key presented in `x-sluice-api-key`,
/// looked up in the key table. Anything that does not resolve — no key, an
/// unknown key, a duplicated/empty/non-UTF-8 header — collapses to "no
/// caller", which prices at the base rate. Unknown keys are not rejected:
/// failing open to the base price means a rotated-out key degrades a
/// customer's discount, not their access.
///
/// The lookup is a plain HashMap hit; keys are expected to be high-entropy
/// random strings (the table maps them to ids), so hash-timing is not a
/// usable oracle the way string prefix comparison would be.
fn resolve_caller(
    headers: &axum::http::HeaderMap,
    keys: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let key = single_header_value(headers, API_KEY_HEADER)?;
    keys.get(key).cloned()
}

/// A header's value if it appears exactly once and is non-empty UTF-8 with
/// no surrounding whitespace. Credentials are matched byte-exactly — a
/// padded variant of a key is not an alternate spelling of it, it is a
/// different (unknown) key. Duplicates and junk bytes collapse to None.
fn single_header_value<'h>(headers: &'h axum::http::HeaderMap, name: &str) -> Option<&'h str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None; // duplicated header: refuse to pick one
    }
    let s = first.to_str().ok()?;
    if s.is_empty() || s.trim() != s {
        None
    } else {
        Some(s)
    }
}

/// Parse the CALLERS_PATH table: `{ "keys": { "<api key>": "<caller id>" } }`.
/// Strict on purpose — this file assigns price tiers. Duplicate keys are an
/// error (serde's default map handling would silently keep one mapping and
/// discard the other, reassigning a credential on a bad merge), and keys and
/// caller ids must be non-empty with no surrounding whitespace (a padded
/// caller id would silently fail to match its rules entry).
fn parse_caller_keys(
    json: &str,
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    struct KeyMap(std::collections::HashMap<String, String>);
    impl<'de> serde::Deserialize<'de> for KeyMap {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = KeyMap;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a map of API key to caller id")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut m: A,
                ) -> Result<KeyMap, A::Error> {
                    let mut out = std::collections::HashMap::new();
                    while let Some((k, v)) = m.next_entry::<String, String>()? {
                        if out.insert(k.clone(), v).is_some() {
                            return Err(serde::de::Error::custom(format!(
                                "duplicate API key {k:?}"
                            )));
                        }
                    }
                    Ok(KeyMap(out))
                }
            }
            d.deserialize_map(V)
        }
    }
    #[derive(serde::Deserialize)]
    struct CallersFile {
        keys: KeyMap,
    }
    let parsed: CallersFile =
        serde_json::from_str(json).context("caller keys file is not valid JSON")?;
    for (key, caller) in &parsed.keys.0 {
        anyhow::ensure!(
            !key.is_empty() && key.trim() == key && !caller.is_empty() && caller.trim() == caller,
            "API keys and caller ids must be non-empty with no surrounding whitespace \
             (offending entry: {key:?} -> {caller:?})"
        );
    }
    Ok(parsed.keys.0)
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

    // ---- integration harness: the full stack against a mock origin ----

    type OriginLog = Arc<std::sync::Mutex<Vec<(String, HeaderMap)>>>;

    /// A real HTTP origin (the proxy speaks reqwest, not tower) that records
    /// every request it sees and answers with deliberately hop-by-hop-laden
    /// headers so response-direction stripping is observable.
    async fn mock_origin() -> (String, OriginLog) {
        let log: OriginLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log2 = log.clone();
        let app = Router::new().fallback(move |req: Request| {
            let log = log2.clone();
            async move {
                log.lock()
                    .unwrap()
                    .push((req.uri().path().to_string(), req.headers().clone()));
                (
                    [
                        ("connection", "x-resp-hop"),
                        ("x-resp-hop", "should-not-escape"),
                        ("keep-alive", "timeout=5"),
                        ("x-origin-header", "kept"),
                    ],
                    "origin says hi",
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), log)
    }

    const ITEST_TABLE: &str = r#"{ "rules": [
        { "prefix": "/free", "pricing": "free" },
        { "prefix": "/paid", "price_usdc": "0.05",
          "caller_prices": { "tenant-a": "0.002" } }
    ] }"#;

    /// The production stack via build_app: stamp middleware, x402 layer
    /// (facilitator unreachable — nothing here pays successfully), proxy.
    async fn full_app() -> (Router, OriginLog) {
        let (origin, log) = mock_origin().await;
        let state = Arc::new(AppState {
            origin,
            strip_prefix: None,
            http: build_http_client(5),
            rules: Arc::new(RwLock::new(Arc::new(
                RuleSet::from_json(ITEST_TABLE).unwrap(),
            ))),
            caller_keys: test_keys(),
            pay_to: "0xpayto".into(),
            indexer_url: None,
            indexer_token: None,
        });
        let app = build_app(state, "http://127.0.0.1:1", Address::ZERO).unwrap();
        (app, log)
    }

    async fn send(
        app: Router,
        req: axum::http::Request<Body>,
    ) -> (StatusCode, axum::http::HeaderMap, bytes::Bytes) {
        use tower::ServiceExt;
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, headers, body)
    }

    fn advertised_amount(headers: &axum::http::HeaderMap) -> String {
        use base64::Engine;
        let b64 = headers
            .get("payment-required")
            .expect("402 must advertise payment requirements")
            .to_str()
            .unwrap();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        v["accepts"][0]["amount"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn denied_paths_never_reach_the_origin() {
        let (app, log) = full_app().await;

        // No rule covers /other.
        let (status, _, _) = send(
            app.clone(),
            axum::http::Request::builder()
                .uri("/other")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Same with a payment attached: a denied path gets no price tag, so
        // the payment must be ignored, not redeemed for access.
        let (status, _, _) = send(
            app.clone(),
            axum::http::Request::builder()
                .uri("/other")
                .header(
                    "payment-signature",
                    "eyJ4NDAyVmVyc2lvbiI6MiwicGF5bG9hZCI6Imp1bmsifQ==",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Suspicious paths are refused outright.
        let (status, _, _) = send(
            app.clone(),
            axum::http::Request::builder()
                .uri("/paid/../free")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        assert!(
            log.lock().unwrap().is_empty(),
            "denied requests must not touch the origin: {:?}",
            log.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn free_routes_are_proxied_without_payment_headers() {
        let (app, log) = full_app().await;
        let (status, headers, body) = send(
            app,
            axum::http::Request::builder()
                .uri("/free/thing")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], b"origin says hi");
        assert!(
            headers.get("payment-required").is_none(),
            "free routes must not demand payment"
        );
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn paid_routes_advertise_exactly_the_rules_amount() {
        let (app, log) = full_app().await;

        // Base price: 0.05 USDC = 50000 micro.
        let (status, headers, _) = send(
            app.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/paid/q")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(advertised_amount(&headers), "50000");

        // Per-caller price via a valid API key: 0.002 USDC = 2000 micro.
        let (status, headers, _) = send(
            app.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/paid/q")
                .header(API_KEY_HEADER, "itest-key-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(advertised_amount(&headers), "2000");

        // The issue's acceptance test: claiming a caller id without valid
        // credentials is priced at the base rate. A bare caller header —
        // even naming a discounted tenant — buys nothing.
        let (status, headers, _) = send(
            app.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/paid/q")
                .header(CALLER_HEADER, "tenant-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(advertised_amount(&headers), "50000");

        // An unknown key also falls back to the base rate (fail open to
        // full price, never to a discount).
        let (status, headers, _) = send(
            app.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/paid/q")
                .header(API_KEY_HEADER, "not-a-real-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(advertised_amount(&headers), "50000");

        // A valid key wins over a contradictory caller claim.
        let (status, headers, _) = send(
            app.clone(),
            axum::http::Request::builder()
                .method("POST")
                .uri("/paid/q")
                .header(API_KEY_HEADER, "itest-key-a")
                .header(CALLER_HEADER, "tenant-nobody")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(advertised_amount(&headers), "2000");

        assert!(
            log.lock().unwrap().is_empty(),
            "unpaid requests to paid routes must not touch the origin"
        );
    }

    #[tokio::test]
    async fn requests_are_counted_by_decision_on_the_metrics_endpoint() {
        let (app, _log) = full_app().await;
        for (uri, hdr) in [
            ("/free/x", None),
            ("/paid/x", None),
            ("/other", None),
            ("/paid/x", Some((API_KEY_HEADER, "itest-key-a"))),
        ] {
            let mut req = axum::http::Request::builder().method("POST").uri(uri);
            if let Some((k, v)) = hdr {
                req = req.header(k, v);
            }
            send(app.clone(), req.body(Body::empty()).unwrap()).await;
        }

        let (status, headers, body) = send(
            app,
            axum::http::Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/plain")),
        );
        // The metrics registry is process-wide and tests run in parallel,
        // so assert the series exist rather than exact counts.
        let text = String::from_utf8(body.to_vec()).unwrap();
        for series in [
            r#"sluice_gateway_requests_total{decision="free",status="200"}"#,
            r#"sluice_gateway_requests_total{decision="paid",status="402"}"#,
            r#"sluice_gateway_requests_total{decision="deny",status="404"}"#,
            r#"sluice_gateway_request_duration_seconds_bucket{decision="paid""#,
        ] {
            assert!(text.contains(series), "missing series {series}\n{text}");
        }
    }

    /// One request = exactly one observation, and scraping /metrics is not
    /// gateway traffic. Uses an origin that 404s plus a free rule so the
    /// (free, 404) label pair is unique to this test — the registry is
    /// process-wide and other tests increment other pairs concurrently.
    #[tokio::test]
    async fn one_request_is_one_observation_and_scrapes_are_not_counted() {
        let origin = Router::new().fallback(|| async { (StatusCode::NOT_FOUND, "nope") });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, origin).await.unwrap() });

        let table = r#"{ "rules": [ { "prefix": "/free", "pricing": "free" },
                                    { "prefix": "/metrics", "price_usdc": "9" } ] }"#;
        let state = Arc::new(AppState {
            origin: format!("http://{origin_addr}"),
            strip_prefix: None,
            http: build_http_client(5),
            rules: Arc::new(RwLock::new(Arc::new(RuleSet::from_json(table).unwrap()))),
            caller_keys: test_keys(),
            pay_to: "0xpayto".into(),
            indexer_url: None,
            indexer_token: None,
        });
        let app = build_app(state, "http://127.0.0.1:1", Address::ZERO).unwrap();

        let count = || {
            metrics()
                .requests
                .with_label_values(&["free", "404"])
                .get()
        };
        let before = count();
        let (status, _, _) = send(
            app.clone(),
            axum::http::Request::builder()
                .uri("/free/x")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND); // origin's 404, proxied
        assert_eq!(count(), before + 1, "exactly one observation per request");

        // /metrics is a reserved gateway path: even priced in the rules
        // table it serves the exposition (no 402), and scrapes are not
        // counted as gateway traffic.
        for _ in 0..2 {
            let (status, _, body) = send(
                app.clone(),
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                String::from_utf8(body.to_vec())
                    .unwrap()
                    .contains("sluice_gateway_requests_total")
            );
        }
        assert_eq!(count(), before + 1, "scrapes must not count as traffic");
    }

    #[tokio::test]
    async fn oversized_bodies_get_413_without_touching_the_origin() {
        let (app, log) = full_app().await;
        let (status, _, _) = send(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/free/upload")
                .body(Body::from(vec![0u8; MAX_BODY_BYTES + 1]))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn hop_by_hop_headers_are_stripped_in_both_directions() {
        let (app, log) = full_app().await;
        let (status, resp_headers, _) = send(
            app,
            axum::http::Request::builder()
                .uri("/free/echo")
                .header("connection", "x-req-hop")
                .header("x-req-hop", "should-not-arrive")
                .header("proxy-authorization", "Basic c2Vrcml0")
                .header("te", "trailers")
                .header(CALLER_HEADER, "tenant-a")
                .header(API_KEY_HEADER, "itest-key-a")
                .header("x-req-keep", "kept")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Request direction: what the origin actually received.
        let origin_headers = {
            let log = log.lock().unwrap();
            assert_eq!(log.len(), 1);
            log[0].1.clone()
        };
        for gone in [
            "connection",
            "x-req-hop", // nominated by Connection
            "proxy-authorization",
            "te",
            CALLER_HEADER,   // legacy caller claim, never forwarded
            API_KEY_HEADER,  // a credential; must never reach the origin
            DECISION_HEADER, // gateway-internal decision stamp
        ] {
            assert!(
                origin_headers.get(gone).is_none(),
                "{gone} must not reach the origin"
            );
        }
        assert_eq!(
            origin_headers
                .get("x-req-keep")
                .and_then(|v| v.to_str().ok()),
            Some("kept")
        );

        // Response direction: what the client got back.
        for gone in ["connection", "keep-alive", "x-resp-hop"] {
            assert!(
                resp_headers.get(gone).is_none(),
                "{gone} must not reach the client"
            );
        }
        assert_eq!(
            resp_headers
                .get("x-origin-header")
                .and_then(|v| v.to_str().ok()),
            Some("kept")
        );
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
            caller_keys: test_keys(),
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
        req.extensions_mut()
            .insert(ResolvedCaller(Some("tenant-a".into())));
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
        // Receipts record the identity the request was priced under — the
        // ResolvedCaller stamped by the middleware, not a re-resolution.
        assert_eq!(receipt["caller"], "tenant-a");
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
            caller_keys: test_keys(),
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

    fn test_keys() -> std::collections::HashMap<String, String> {
        [("itest-key-a".to_string(), "tenant-a".to_string())]
            .into_iter()
            .collect()
    }

    #[test]
    fn known_api_key_resolves_its_caller() {
        let mut h = HeaderMap::new();
        h.insert(API_KEY_HEADER, HeaderValue::from_static("itest-key-a"));
        assert_eq!(
            resolve_caller(&h, &test_keys()).as_deref(),
            Some("tenant-a")
        );
    }

    #[test]
    fn everything_else_resolves_to_no_caller() {
        // Unknown key.
        let mut h = HeaderMap::new();
        h.insert(API_KEY_HEADER, HeaderValue::from_static("who-dis"));
        assert_eq!(resolve_caller(&h, &test_keys()), None);

        // A bare caller claim with no key is not identity.
        let mut h = HeaderMap::new();
        h.insert(CALLER_HEADER, HeaderValue::from_static("tenant-a"));
        assert_eq!(resolve_caller(&h, &test_keys()), None);

        // Duplicated key header: refuse to pick one.
        let mut h = HeaderMap::new();
        h.append(API_KEY_HEADER, HeaderValue::from_static("itest-key-a"));
        h.append(API_KEY_HEADER, HeaderValue::from_static("other"));
        assert_eq!(resolve_caller(&h, &test_keys()), None);

        // Empty and non-UTF-8 values.
        let mut h = HeaderMap::new();
        h.insert(API_KEY_HEADER, HeaderValue::from_static(""));
        assert_eq!(resolve_caller(&h, &test_keys()), None);
        let mut h = HeaderMap::new();
        h.insert(API_KEY_HEADER, HeaderValue::from_bytes(b"\xff\xfe").unwrap());
        assert_eq!(resolve_caller(&h, &test_keys()), None);

        // Credentials match byte-exactly: a padded key is an unknown key.
        let mut h = HeaderMap::new();
        h.insert(API_KEY_HEADER, HeaderValue::from_static(" itest-key-a "));
        assert_eq!(resolve_caller(&h, &test_keys()), None);

        assert_eq!(resolve_caller(&HeaderMap::new(), &test_keys()), None);
    }

    #[test]
    fn caller_keys_file_parses_and_validates() {
        let keys =
            parse_caller_keys(r#"{ "keys": { "k1": "tenant-a", "k2": "tenant-b" } }"#).unwrap();
        assert_eq!(keys.get("k1").map(String::as_str), Some("tenant-a"));
        assert_eq!(keys.len(), 2);

        assert!(parse_caller_keys("not json").is_err());
        assert!(parse_caller_keys(r#"{ "keys": { "": "tenant-a" } }"#).is_err());
        assert!(parse_caller_keys(r#"{ "keys": { "k": " " } }"#).is_err());
        // Whitespace-padded entries would silently mismatch at runtime.
        assert!(parse_caller_keys(r#"{ "keys": { "k ": "tenant-a" } }"#).is_err());
        assert!(parse_caller_keys(r#"{ "keys": { "k": "tenant-a " } }"#).is_err());
        // A duplicated key must fail loudly, not silently pick one mapping.
        assert!(
            parse_caller_keys(r#"{ "keys": { "k": "tenant-a", "k": "tenant-b" } }"#).is_err()
        );
    }
}
