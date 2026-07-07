//! Settlement receipt indexer: accepts receipts from the gateway over HTTP
//! and writes them to Postgres so payments are queryable. Schema lives in
//! `migrations/` (applied automatically at startup) — no ad-hoc DDL.
//!
//! Trust boundary: `/receipts` is unauthenticated and must only be reachable
//! from the compose-internal network (it is not published to the host).
//! Anything that can reach it can write rows. Before this data backs real
//! accounting (rung 3), add gateway->indexer authentication.

use std::env;

use anyhow::Context;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;

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

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/receipts", post(ingest))
        .with_state(pool);

    tracing::info!(%bind, "sluice indexer starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ingest(
    State(pool): State<sqlx::PgPool>,
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
    .execute(&pool)
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
