//! Optional durable delivery for small, read-only paid GET responses.
//! Responses are prepared before settlement and kept with the payment outcome.
//! Unknown settlement outcomes fail closed; no automatic second authorization.
use crate::{
    AppState, Decision,
    facilitator::{ClientKind, Error},
};
use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use x402_types::{
    facilitator::Facilitator,
    proto::{SettleRequest, SettleResponse, SupportedResponse, VerifyRequest, VerifyResponse},
};

const MAX_RESPONSE: usize = 64 * 1024;
#[derive(Clone)]
pub struct Prepared {
    id: String,
    binding: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}
tokio::task_local! { static PREPARED: Prepared; }
pub struct Journal(Mutex<Connection>);
impl Journal {
    pub fn open(path: &str) -> Result<Self, Error> {
        let connection = Connection::open(path).map_err(|_| Error::Journal)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000; PRAGMA max_page_count=65536;
        CREATE TABLE IF NOT EXISTS attempts (
          id TEXT PRIMARY KEY, binding TEXT NOT NULL, authorization TEXT NOT NULL UNIQUE,
          body BLOB NOT NULL, headers TEXT NOT NULL, phase TEXT NOT NULL, authorization_details TEXT NOT NULL,
          settlement TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );").map_err(|_|Error::Journal)?;
        Ok(Self(Mutex::new(connection)))
    }
    fn prepare(&self, p: &Prepared, authorization: &str, details: &Value) -> Result<(), Error> {
        let c = self.0.lock().map_err(|_| Error::Journal)?;
        c.execute("INSERT INTO attempts(id,binding,authorization,body,headers,phase,authorization_details) VALUES(?1,?2,?3,?4,?5,'prepared',?6) ON CONFLICT(id) DO NOTHING",
            params![p.id,p.binding,authorization,p.body,serde_json::to_string(&p.headers).map_err(|_|Error::Journal)?,details.to_string()]).map_err(|_|Error::Journal)?;
        Ok(())
    }
    fn get(&self, id: &str) -> Result<Option<(Prepared, String, Option<String>)>, Error> {
        let c = self.0.lock().map_err(|_| Error::Journal)?;
        c.query_row(
            "SELECT binding,body,headers,phase,settlement FROM attempts WHERE id=?1",
            [id],
            |r| {
                let headers: String = r.get(2)?;
                let headers = serde_json::from_str(&headers).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok((
                    Prepared {
                        id: id.into(),
                        binding: r.get(0)?,
                        body: r.get(1)?,
                        headers,
                    },
                    r.get(3)?,
                    r.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| Error::Journal)
    }
    fn claim(&self, id: &str) -> Result<(), Error> {
        let c = self.0.lock().map_err(|_| Error::Journal)?;
        let changed = c
            .execute(
                "UPDATE attempts SET phase='settling' WHERE id=?1 AND phase='prepared'",
                [id],
            )
            .map_err(|_| Error::Journal)?;
        if changed != 1 {
            return Err(Error::Uncertain);
        }
        Ok(())
    }
    fn finish(&self, id: &str, result: &SettleResponse) -> Result<(), Error> {
        let phase = if result.0["success"] == true {
            "settled"
        } else {
            "uncertain"
        };
        let c = self.0.lock().map_err(|_| Error::Journal)?;
        c.execute(
            "UPDATE attempts SET phase=?2,settlement=?3 WHERE id=?1 AND phase='settling'",
            params![id, phase, result.0.to_string()],
        )
        .map_err(|_| Error::Journal)?;
        Ok(())
    }
}
fn hash(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}
fn authorization(value: &Value) -> Result<String, Error> {
    let payload = &value["paymentPayload"];
    let accepted = &payload["accepted"];
    let auth = &payload["payload"]["authorization"];
    let fields = [
        &accepted["network"],
        &accepted["asset"],
        &auth["from"],
        &auth["nonce"],
    ];
    if payload["x402Version"] != 2
        || accepted["scheme"] != "exact"
        || fields.iter().any(|v| !v.is_string())
    {
        return Err(Error::Journal);
    }
    Ok(hash(&json!(
        fields
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_ascii_lowercase())
            .collect::<Vec<_>>()
    )))
}
#[derive(Clone)]
pub struct Guarded {
    pub client: ClientKind,
    pub journal: Option<Arc<Journal>>,
}
impl Facilitator for Guarded {
    type Error = Error;
    async fn supported(&self) -> Result<SupportedResponse, Error> {
        self.client.supported().await
    }
    async fn verify(&self, request: &VerifyRequest) -> Result<VerifyResponse, Error> {
        let response = self.client.verify(request).await?;
        if response.0["isValid"] == true
            && let Some(journal) = &self.journal
        {
            let value = serde_json::to_value(request).map_err(|_| Error::Journal)?;
            let p = PREPARED
                .try_with(Clone::clone)
                .map_err(|_| Error::Journal)?;
            if p.id != hash(&value["paymentPayload"]) {
                return Err(Error::Journal);
            }
            let payload = &value["paymentPayload"];
            let auth = &payload["payload"]["authorization"];
            let details = json!({"network":payload["accepted"]["network"],"asset":payload["accepted"]["asset"],"payer":auth["from"],"receiver":auth["to"],"amount":auth["value"],"nonce":auth["nonce"],"valid_after":auth["validAfter"],"valid_before":auth["validBefore"]});
            journal.prepare(&p, &authorization(&value)?, &details)?;
        }
        Ok(response)
    }
    async fn settle(&self, request: &SettleRequest) -> Result<SettleResponse, Error> {
        if let Some(journal) = &self.journal {
            let value = serde_json::to_value(request).map_err(|_| Error::Journal)?;
            let id = hash(&value["paymentPayload"]);
            let p = PREPARED
                .try_with(Clone::clone)
                .map_err(|_| Error::Journal)?;
            if id != p.id {
                return Err(Error::Journal);
            }
            // settle-before-execution in x402-axum calls settle directly.
            // Validate explicitly before allocating a durable attempt.
            if self.verify(request).await?.0["isValid"] != true {
                return Err(Error::InvalidPayment);
            }
            journal.claim(&id)?; // Full-sync commit precedes the external side effect.
            let response = self.client.settle(request).await?;
            journal.finish(&id, &response)?;
            Ok(response)
        } else {
            self.client.settle(request).await
        }
    }
}
fn failure(status: StatusCode, code: &str) -> Response {
    (status,axum::Json(json!({"error":{"code":code,"message":"Do not create a replacement payment for an uncertain outcome. Retry the identical request and payment, or contact the operator."}}))).into_response()
}
fn response(p: &Prepared, settlement: Option<&str>) -> Result<Response, Error> {
    let mut response = Response::new(Body::from(p.body.clone()));
    for (name, value) in &p.headers {
        response.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Journal)?,
            HeaderValue::from_str(value).map_err(|_| Error::Journal)?,
        );
    }
    response.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("private, no-store"),
    );
    if let Some(settlement) = settlement {
        response.headers_mut().insert(
            "payment-response",
            HeaderValue::from_str(&STANDARD.encode(settlement)).map_err(|_| Error::Journal)?,
        );
    }
    Ok(response)
}
pub fn prepared_response(journal: Option<&Journal>) -> Option<Response> {
    PREPARED
        .try_with(|p| {
            if let Some(journal) = journal {
                match journal.get(&p.id) {
                    Ok(Some((saved, phase, settlement))) if phase == "settled" => {
                        response(&saved, settlement.as_deref()).unwrap_or_else(|_| {
                            failure(StatusCode::SERVICE_UNAVAILABLE, "journal_unavailable")
                        })
                    }
                    _ => failure(StatusCode::SERVICE_UNAVAILABLE, "payment_outcome_unknown"),
                }
            } else {
                response(p, None).unwrap_or_else(|_| {
                    failure(StatusCode::SERVICE_UNAVAILABLE, "response_unavailable")
                })
            }
        })
        .ok()
}

fn payment_id(headers: &HeaderMap) -> Result<Option<(String, Value)>, Error> {
    let Some(value) = headers.get("payment-signature") else {
        return Ok(None);
    };
    if value.as_bytes().len() > 16384 {
        return Err(Error::Journal);
    }
    let decoded = STANDARD
        .decode(value.as_bytes())
        .map_err(|_| Error::Journal)?;
    let payload: Value = serde_json::from_slice(&decoded).map_err(|_| Error::Journal)?;
    if payload["x402Version"] != 2 {
        return Err(Error::Journal);
    }
    Ok(Some((hash(&payload), payload)))
}
pub async fn guard(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let Some(journal) = &st.journal else {
        return next.run(req).await;
    };
    if !matches!(
        req.extensions().get::<Decision>(),
        Some(Decision::Paid { .. })
    ) {
        return next.run(req).await;
    }
    if req.method() != axum::http::Method::GET {
        return failure(StatusCode::METHOD_NOT_ALLOWED, "paid_get_only");
    }
    if req.uri().query().is_some() {
        return failure(StatusCode::BAD_REQUEST, "query_not_supported");
    }
    let binding = format!("GET {}", req.uri());
    let payment = match payment_id(req.headers()) {
        Ok(v) => v,
        Err(_) => return failure(StatusCode::BAD_REQUEST, "invalid_payment"),
    };
    if let Some((id, payload)) = &payment {
        let Some(base) = &st.public_base else {
            return failure(StatusCode::SERVICE_UNAVAILABLE, "public_url_required");
        };
        if payload["resource"]["url"] != format!("{}{}", base.trim_end_matches('/'), req.uri()) {
            return failure(StatusCode::BAD_REQUEST, "payment_resource_mismatch");
        }
        match journal.get(id) {
            Ok(Some((p, phase, settlement))) => {
                if p.binding != binding {
                    return failure(StatusCode::CONFLICT, "payment_request_mismatch");
                }
                match phase.as_str() {
                    "settled" => {
                        return response(&p, settlement.as_deref()).unwrap_or_else(|_| {
                            failure(StatusCode::SERVICE_UNAVAILABLE, "journal_unavailable")
                        });
                    }
                    "prepared" => return run_guarded(journal, p, req, next).await,
                    "rejected" => return failure(StatusCode::PAYMENT_REQUIRED, "payment_rejected"),
                    _ => {
                        return failure(StatusCode::SERVICE_UNAVAILABLE, "payment_outcome_unknown");
                    }
                }
            }
            Ok(None) => {}
            Err(_) => return failure(StatusCode::SERVICE_UNAVAILABLE, "journal_unavailable"),
        }
    }
    let (parts, body) = req.into_parts();
    if to_bytes(body, 0).await.is_err() {
        return failure(StatusCode::BAD_REQUEST, "get_body_not_supported");
    }
    let req = Request::from_parts(parts, Body::empty());
    let mut probe = Request::new(Body::empty());
    *probe.uri_mut() = req.uri().clone();
    *probe.method_mut() = req.method().clone();
    *probe.headers_mut() = req.headers().clone();
    let result = match crate::forward(&st, probe).await {
        Ok(v) => v,
        Err(_) => return failure(StatusCode::SERVICE_UNAVAILABLE, "origin_unavailable"),
    };
    if result.status() != StatusCode::OK {
        return result;
    }
    let headers = result
        .headers()
        .iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                "content-type"
                    | "link"
                    | "content-security-policy"
                    | "x-content-type-options"
                    | "referrer-policy"
            )
        })
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
        .collect();
    let body = match to_bytes(result.into_body(), MAX_RESPONSE).await {
        Ok(v) => v.to_vec(),
        Err(_) => return failure(StatusCode::SERVICE_UNAVAILABLE, "response_not_prepared"),
    };
    let prepared = Prepared {
        id: payment.map(|v| v.0).unwrap_or_default(),
        binding,
        body,
        headers,
    };
    run_guarded(journal, prepared, req, next).await
}

async fn run_guarded(journal: &Journal, prepared: Prepared, req: Request, next: Next) -> Response {
    let id = prepared.id.clone();
    let result = PREPARED.scope(prepared, next.run(req)).await;
    if !id.is_empty() {
        match journal.get(&id) {
            Ok(Some((p, phase, settlement))) if phase == "settled" => {
                return response(&p, settlement.as_deref()).unwrap_or_else(|_| {
                    failure(StatusCode::SERVICE_UNAVAILABLE, "journal_unavailable")
                });
            }
            Ok(Some((_, phase, _))) if matches!(phase.as_str(), "settling" | "uncertain") => {
                return failure(StatusCode::SERVICE_UNAVAILABLE, "payment_outcome_unknown");
            }
            Ok(Some((_, phase, _))) if phase == "prepared" => {
                return failure(StatusCode::CONFLICT, "payment_in_progress");
            }
            Err(_) => return failure(StatusCode::SERVICE_UNAVAILABLE, "journal_unavailable"),
            _ => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prepared(id: &str) -> Prepared {
        Prepared {
            id: id.into(),
            binding: "GET /item".into(),
            body: b"original snapshot".to_vec(),
            headers: vec![],
        }
    }
    #[test]
    fn persists_response_and_blocks_second_settlement_after_restart() {
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let journal = Journal::open(file.path().to_str().unwrap()).unwrap();
            journal
                .prepare(&prepared("one"), "nonce", &json!({}))
                .unwrap();
            journal.claim("one").unwrap();
            assert!(journal.claim("one").is_err());
        }
        let journal = Journal::open(file.path().to_str().unwrap()).unwrap();
        assert_eq!(journal.get("one").unwrap().unwrap().1, "settling");
        assert!(
            journal
                .prepare(&prepared("different-signature"), "nonce", &json!({}))
                .is_err()
        );
        journal
            .finish(
                "one",
                &SettleResponse(json!({"success":true,"transaction":"0x123"})),
            )
            .unwrap();
        let (p, phase, settlement) = journal.get("one").unwrap().unwrap();
        assert_eq!(phase, "settled");
        assert_eq!(p.body, b"original snapshot");
        assert!(settlement.unwrap().contains("0x123"));
        assert!(journal.claim("one").is_err());
    }
}

#[cfg(test)]
mod integration {
    use super::*;
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use std::sync::{
        RwLock,
        atomic::{AtomicUsize, Ordering},
    };
    use tower::ServiceExt;
    use x402_chain_eip155::KnownNetworkEip155;
    use x402_types::networks::USDC;
    async fn server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }
    async fn app(
        file: &str,
        settles: Arc<AtomicUsize>,
        reads: Arc<AtomicUsize>,
        uncertain: bool,
    ) -> Router {
        let origin = server(Router::new().fallback(move |req: Request| {
            let reads = reads.clone();
            async move {
                if req.uri().path().ends_with("/missing") {
                    return (StatusCode::NOT_FOUND, "missing").into_response();
                }
                assert!(req.headers().get("payment-signature").is_none());
                let n = reads.fetch_add(1, Ordering::SeqCst);
                Json(json!({"snapshot":n})).into_response()
            }
        }))
        .await;
        let facilitator=server(Router::new()
            .route("/supported",get(||async{Json(json!({"kinds":[{"x402Version":2,"scheme":"exact","network":"eip155:8453"}]}))}))
            .route("/verify",post(||async{Json(json!({"isValid":true,"payer":"0x1111111111111111111111111111111111111111"}))}))
            .route("/settle",post(move ||{let settles=settles.clone();async move{
                settles.fetch_add(1,Ordering::SeqCst);
                if uncertain{return StatusCode::BAD_GATEWAY.into_response()}
                Json(json!({"success":true,"network":"eip155:8453","payer":"0x1111111111111111111111111111111111111111","transaction":"0xabc"})).into_response()
            }}))).await;
        let state = Arc::new(AppState {
            journal: Some(Arc::new(Journal::open(file).unwrap())),
            public_base: Some("https://example.test".into()),
            origin,
            strip_prefix: None,
            http: crate::build_http_client(2),
            rules: Arc::new(RwLock::new(Arc::new(
                rules::RuleSet::from_json(r#"{"rules":[{"prefix":"/paid","price_usdc":"0.001"}]}"#)
                    .unwrap(),
            ))),
            caller_keys: Default::default(),
            pay_to: "0x2222222222222222222222222222222222222222".into(),
            indexer_url: None,
            indexer_token: None,
        });
        crate::build_app(
            state,
            &facilitator,
            "0x2222222222222222222222222222222222222222"
                .parse()
                .unwrap(),
            USDC::base(),
        )
        .unwrap()
    }
    fn request(payment: Option<&str>) -> Request {
        let mut builder = Request::builder().uri("/paid/item");
        if let Some(p) = payment {
            builder = builder.header("payment-signature", p)
        }
        builder.body(Body::empty()).unwrap()
    }
    async fn payment(app: &Router) -> String {
        let quote = app.clone().oneshot(request(None)).await.unwrap();
        assert_eq!(quote.status(), StatusCode::PAYMENT_REQUIRED);
        let quote: Value = serde_json::from_slice(
            &STANDARD
                .decode(quote.headers()["payment-required"].as_bytes())
                .unwrap(),
        )
        .unwrap();
        STANDARD.encode(json!({"x402Version":2,"resource":quote["resource"],"accepted":quote["accepts"][0],"payload":{"signature":format!("0x{}","11".repeat(65)),"authorization":{"from":"0x1111111111111111111111111111111111111111","to":"0x2222222222222222222222222222222222222222","value":"1000","validAfter":"0","validBefore":"9999999999","nonce":format!("0x{}","22".repeat(32))}}}).to_string())
    }
    #[tokio::test]
    async fn replay_keeps_snapshot_and_settles_once_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("journal.db");
        let file = file.to_str().unwrap();
        let settles = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let router = app(file, settles.clone(), reads.clone(), false).await;
        let paid = payment(&router).await;
        let first = router.clone().oneshot(request(Some(&paid))).await.unwrap();
        let status = first.status();
        let first = to_bytes(first.into_body(), 65536).await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&first)
        );
        assert_eq!(settles.load(Ordering::SeqCst), 1);
        drop(router);
        let router = app(file, settles.clone(), reads.clone(), false).await;
        let (a, b) = tokio::join!(
            router.clone().oneshot(request(Some(&paid))),
            router.oneshot(request(Some(&paid)))
        );
        for response in [a.unwrap(), b.unwrap()] {
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().contains_key("payment-response"));
            assert_eq!(to_bytes(response.into_body(), 65536).await.unwrap(), first);
        }
        assert_eq!(settles.load(Ordering::SeqCst), 1);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn concurrent_first_requests_return_the_persisted_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("journal.db");
        let settles = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let router = app(file.to_str().unwrap(), settles.clone(), reads, false).await;
        let paid = payment(&router).await;
        let (a, b) = tokio::join!(
            router.clone().oneshot(request(Some(&paid))),
            router.clone().oneshot(request(Some(&paid)))
        );
        let replay = router.oneshot(request(Some(&paid))).await.unwrap();
        assert_eq!(replay.status(), StatusCode::OK);
        let stored = to_bytes(replay.into_body(), 65536).await.unwrap();
        let mut successes = 0;
        for response in [a.unwrap(), b.unwrap()] {
            if response.status() == StatusCode::OK {
                successes += 1;
                assert_eq!(to_bytes(response.into_body(), 65536).await.unwrap(), stored);
            } else {
                assert!(matches!(response.status().as_u16(), 409 | 503));
            }
        }
        assert!(successes >= 1);
        assert_eq!(settles.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_outcome_blocks_retry_and_bad_requests_do_not_settle() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("journal.db");
        let file = file.to_str().unwrap();
        let settles = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let router = app(file, settles.clone(), reads, true).await;
        for (method, path, status) in [
            ("POST", "/paid/item", 405),
            ("GET", "/paid/missing", 404),
            ("GET", "/paid/item?q=1", 400),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
        }
        assert_eq!(settles.load(Ordering::SeqCst), 0);
        let paid = payment(&router).await;
        for _ in 0..2 {
            let response = router.clone().oneshot(request(Some(&paid))).await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        assert_eq!(settles.load(Ordering::SeqCst), 1);
    }
}
