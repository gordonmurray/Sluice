//! A self-contained demo origin so a fresh clone can run the full paid loop
//! without any external checkout. It mimics the slice of Firn's API the demo
//! meters — `GET /health`, `GET /metrics`, `POST /ns/{ns}/query` — over a
//! small built-in corpus with term-overlap ranking. It is a stand-in, not a
//! search engine; the flagship Firn demo is a compose override away
//! (docker-compose.firn.yml).

use std::env;

use axum::{
    Json, Router,
    extract::Path,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;

/// The same corpus scripts/seed-firn.sh loads into Firn (config/seed.json),
/// so both origins answer the demo query with recognisably similar results.
const CORPUS: [(u64, &str); 8] = [
    (1, "x402 is an open protocol for HTTP-native payments: a server answers 402 Payment Required with machine-readable requirements and the client retries with a signed payment."),
    (2, "EIP-3009 transferWithAuthorization lets a USDC holder sign a transfer off-chain; anyone can broadcast it, so the payer needs no ETH for gas."),
    (3, "A facilitator verifies signed payment authorizations and settles them on-chain. It never holds customer funds; the authorization is bound to amount, recipient, and validity window."),
    (4, "Sluice is a pay-per-request gateway: it prices routes from a rules table and forwards to the origin only after payment settles."),
    (5, "USDC on Base uses six decimal places, so 10000 atomic units equal one cent."),
    (6, "Full-text search ranks documents with BM25; hybrid search fuses BM25 and vector similarity using reciprocal rank fusion."),
    (7, "An API gateway terminates client connections, applies policy such as pricing or quotas, and reverse-proxies requests to backend origins."),
    (8, "Anvil can fork a live chain so contracts like USDC run locally against real state with fake value."),
];

#[derive(Deserialize)]
struct Query {
    text: String,
    #[serde(default = "default_k")]
    k: usize,
}

fn default_k() -> usize {
    3
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let bind = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:3000".to_string());

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/metrics", get(|| async { "# demo origin: no metrics\n" }))
        .route("/ns/{ns}/query", post(query));

    tracing::info!(%bind, "demo origin starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn query(Path(_ns): Path<String>, Json(q): Json<Query>) -> impl IntoResponse {
    let results: Vec<_> = rank(&q.text, q.k)
        .into_iter()
        .map(|(id, score, text)| json!({ "id": id, "score": score, "text": text }))
        .collect();
    (StatusCode::OK, Json(json!({ "results": results })))
}

/// Term-overlap ranking: score = matching query terms / query terms, tie
/// broken by id for determinism. Zero-score rows are dropped; if nothing
/// matches, the top-k by id come back instead so the demo never looks empty.
fn rank(text: &str, k: usize) -> Vec<(u64, f64, &'static str)> {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect();
    let mut scored: Vec<(u64, f64, &'static str)> = CORPUS
        .iter()
        .map(|&(id, doc)| {
            let lower = doc.to_lowercase();
            let hits = terms.iter().filter(|t| lower.contains(t.as_str())).count();
            let score = if terms.is_empty() {
                0.0
            } else {
                hits as f64 / terms.len() as f64
            };
            (id, score, doc)
        })
        .filter(|&(_, score, _)| score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    if scored.is_empty() {
        scored = CORPUS
            .iter()
            .map(|&(id, doc)| (id, 0.0, doc))
            .collect();
    }
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_query_finds_the_payment_docs() {
        let hits = rank("gasless payments without ETH", 3);
        assert!(!hits.is_empty());
        // "payments" hits doc 1 and "ETH" hits doc 2; they tie and the tie
        // breaks by id, deterministically.
        let ids: Vec<u64> = hits.iter().map(|h| h.0).collect();
        assert!(ids.contains(&1) && ids.contains(&2), "{ids:?}");
        assert!(hits.len() <= 3);
        assert!(hits.windows(2).all(|w| w[0].1 >= w[1].1), "sorted by score");
    }

    #[test]
    fn no_match_still_returns_k_results() {
        let hits = rank("zzz qqq", 2);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn k_caps_the_result_count() {
        assert_eq!(rank("payment", 1).len(), 1);
    }
}
