//! Settlement receipt indexer: accepts receipts from the gateway over HTTP
//! and writes them to Postgres so payments are queryable. Schema lives in
//! `migrations/` (applied automatically at startup) — no ad-hoc DDL.
//!
//! Trust boundary: `/receipts` requires the shared token in `INDEXER_TOKEN`
//! (sent by the gateway as a bearer token); anything without it gets 401.
//! The token is required at startup — if it were optional, forgetting it
//! would silently disable auth. Network isolation (the port is not published
//! to the host) remains the outer wall, not the only one.

use std::{env, sync::Arc};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use subtle::ConstantTimeEq;

/// What the gateway sends after a payment settles. Field names mirror the
/// x402 SettleResponse plus the request context the gateway adds.
#[derive(Debug, Deserialize)]
struct Receipt {
    tx_hash: String,
    network: String,
    payer: String,
    pay_to: String,
    amount_micro_usdc: i64,
    path: String,
    caller: Option<String>,
    success: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let database_url = env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let token = env::var("INDEXER_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .context("INDEXER_TOKEN is required (non-empty shared secret; the gateway must send the same value)")?;
    let bind = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8090".to_string());

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .context("cannot connect to Postgres")?;
    sqlx::migrate!("../migrations")
        .run(&pool)
        .await
        .context("migrations failed")?;

    let app = router(Arc::new(App { pool, token }));

    tracing::info!(%bind, "sluice indexer starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

struct App {
    pool: sqlx::PgPool,
    token: String,
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/receipts",
            post(ingest).layer(middleware::from_fn_with_state(app.clone(), require_token)),
        )
        .with_state(app)
}

/// Auth runs as middleware, before any body extraction: an unauthenticated
/// caller gets a uniform 401 and never exercises JSON parsing or the body
/// limit.
async fn require_token(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if !authorized(req.headers(), &app.token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response();
    }
    next.run(req).await
}

async fn ingest(
    State(app): State<Arc<App>>,
    Json(r): Json<Receipt>,
) -> impl IntoResponse {
    let res = sqlx::query(
        "INSERT INTO payments
             (tx_hash, network, payer, pay_to, amount_micro_usdc, path, caller, success)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (network, tx_hash) DO NOTHING",
    )
    .bind(&r.tx_hash)
    .bind(&r.network)
    .bind(&r.payer)
    .bind(&r.pay_to)
    .bind(r.amount_micro_usdc)
    .bind(&r.path)
    .bind(&r.caller)
    .bind(r.success)
    .execute(&app.pool)
    .await;

    match res {
        Ok(done) => {
            if done.rows_affected() == 0 {
                // Retries are expected; a duplicate with *different* fields
                // would be hidden here too, hence the log.
                tracing::info!(tx_hash = %r.tx_hash, "duplicate receipt ignored");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, tx_hash = %r.tx_hash, "failed to index receipt");
            (StatusCode::INTERNAL_SERVER_ERROR, "insert failed").into_response()
        }
    }
}

/// Exactly `Authorization: Bearer <token>` — byte-exact scheme on purpose:
/// the gateway (reqwest `bearer_auth`) is the only intended client, and this
/// is not a public API, so no RFC 9110 case-insensitive scheme parsing.
/// Equal-length comparison is constant-time (`subtle`); the token's length
/// is not treated as secret.
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(presented) = value.strip_prefix("Bearer ") else {
        return false;
    };
    presented.as_bytes().ct_eq(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_auth(v: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static(v));
        h
    }

    #[test]
    fn correct_bearer_token_is_authorized() {
        assert!(authorized(&headers_with_auth("Bearer sekrit"), "sekrit"));
    }

    #[test]
    fn everything_else_is_unauthorized() {
        for bad in [
            "Bearer wrong",
            "Bearer sekrit ", // trailing junk is not the token
            "Bearer",
            "Bearer ",
            "bearer sekrit", // scheme is case-sensitive here; the gateway sends "Bearer"
            "Basic sekrit",
            "sekrit",
        ] {
            assert!(!authorized(&headers_with_auth(bad), "sekrit"), "{bad:?}");
        }
        assert!(!authorized(&HeaderMap::new(), "sekrit"));

        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff\xfe").unwrap(),
        );
        assert!(!authorized(&h, "sekrit"));
    }

    /// Router with a lazy pool: no database behind it, so any test that gets
    /// past auth into the insert would fail — which is exactly what proves
    /// auth runs first.
    fn test_router() -> Router {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://nobody@localhost:1/nothing")
            .unwrap();
        router(Arc::new(App {
            pool,
            token: "sekrit".into(),
        }))
    }

    async fn status_of(
        auth: Option<&'static str>,
        body: &'static str,
    ) -> StatusCode {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri("/receipts")
            .header("content-type", "application/json");
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let req = req.body(axum::body::Body::from(body)).unwrap();
        test_router().oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn receipts_rejects_unauthenticated_before_reading_the_body() {
        // Garbage bodies still get a uniform 401: auth precedes extraction.
        assert_eq!(status_of(None, "not json").await, StatusCode::UNAUTHORIZED);
        assert_eq!(
            status_of(Some("Bearer wrong"), r#"{"tx_hash":"0x1"}"#).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn receipts_with_the_token_reaches_body_parsing() {
        // Right token + malformed body: past auth, rejected by the Json
        // extractor rather than as unauthorized.
        let status = status_of(Some("Bearer sekrit"), "not json").await;
        assert_ne!(status, StatusCode::UNAUTHORIZED);
        assert!(status.is_client_error(), "got {status}");
    }

    #[tokio::test]
    async fn healthz_needs_no_token() {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .uri("/healthz")
            .body(axum::body::Body::empty())
            .unwrap();
        let status = test_router().oneshot(req).await.unwrap().status();
        assert_eq!(status, StatusCode::OK);
    }
}
