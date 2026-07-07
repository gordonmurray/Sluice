use std::{env, sync::Arc};

use alloy_signer_local::PrivateKeySigner;
use anyhow::Context;
use serde_json::json;
use x402_chain_eip155::V2Eip155ExactClient;
use x402_reqwest::{ReqwestWithPayments, ReqwestWithPaymentsBuild, X402Client};

/// Proves pay-per-query search end to end against the gateway:
/// - gateway + origin health stay free
/// - a Firn full-text search without payment -> 402 + requirements
/// - the same search paid via x402 -> 200 + ranked results
/// - the same search as tenant-a -> cheaper per-caller price
/// - admin routes (upsert) are not reachable through the gateway
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let gateway = env::var("GATEWAY_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let signer: PrivateKeySigner = env::var("CLIENT_PRIVATE_KEY")
        .context("CLIENT_PRIVATE_KEY is required")?
        .parse()
        .context("CLIENT_PRIVATE_KEY is not a valid private key")?;
    println!("client signer: {}", signer.address());

    let plain = reqwest::Client::new();
    let search = json!({
        "text": "gasless payments without ETH",
        "k": 3,
        "include_vector": false
    });
    let query_url = format!("{gateway}/firn/ns/demo/query");

    // 1. Free routes: the gateway's own health and Firn's, proxied.
    for path in ["/healthz", "/firn/health"] {
        let res = plain.get(format!("{gateway}{path}")).send().await?;
        println!("GET {path} -> {}", res.status());
        anyhow::ensure!(res.status().is_success(), "free route {path} failed");
    }

    // 2. Search without payment: expect 402 + payment requirements.
    let res = plain.post(&query_url).json(&search).send().await?;
    let status = res.status();
    println!("POST /firn/ns/demo/query (no payment) -> {status}");
    println!(
        "  payment-required (base64): {}",
        header(&res, "payment-required").as_deref().unwrap_or("<missing>")
    );
    anyhow::ensure!(
        status == reqwest::StatusCode::PAYMENT_REQUIRED,
        "expected 402 without payment, got {status}"
    );

    // 3. The same search, paid: sign EIP-3009, retry, ranked results.
    let x402 = X402Client::new().register(V2Eip155ExactClient::new(Arc::new(signer)));
    let paying = reqwest::Client::new().with_payments(x402).build();
    // reqwest-middleware's builder has no .json() without an extra feature;
    // set the body by hand to avoid the dependency.
    let res = paying
        .post(&query_url)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&search)?)
        .send()
        .await?;
    let status = res.status();
    let settlement = header(&res, "payment-response");
    let body: serde_json::Value = res.json().await?;
    println!("POST /firn/ns/demo/query (x402, base price) -> {status}");
    print_hits(&body);
    println!(
        "  payment-response (base64): {}",
        settlement.as_deref().unwrap_or("<missing>")
    );
    anyhow::ensure!(status.is_success(), "paid search failed with {status}");
    anyhow::ensure!(
        !body["results"].as_array().map(Vec::is_empty).unwrap_or(true),
        "paid search returned no results"
    );

    // 4. Same search as tenant-a, which the rules table prices lower.
    let res = paying
        .post(&query_url)
        .header("x-sluice-caller", "tenant-a")
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&search)?)
        .send()
        .await?;
    let status = res.status();
    println!("POST /firn/ns/demo/query (x402, caller tenant-a) -> {status}");
    println!(
        "  payment-response (base64): {}",
        header(&res, "payment-response").as_deref().unwrap_or("<missing>")
    );
    anyhow::ensure!(status.is_success(), "per-caller paid search failed with {status}");

    // 5. Admin writes are not exposed: no rule covers upsert.
    let res = plain
        .post(format!("{gateway}/firn/ns/demo/upsert"))
        .json(&json!({"rows": []}))
        .send()
        .await?;
    println!("POST /firn/ns/demo/upsert -> {}", res.status());
    anyhow::ensure!(
        res.status() == reqwest::StatusCode::NOT_FOUND,
        "admin route should 404 through the gateway"
    );

    // 6. Path traversal under the paid prefix must be rejected, not priced
    //    and normalized into an admin route by downstream URL parsing.
    let res = plain
        .post(format!("{gateway}/firn/ns/demo/query/../upsert"))
        .json(&json!({"rows": []}))
        .send()
        .await?;
    println!("POST /firn/ns/demo/query/../upsert -> {}", res.status());
    anyhow::ensure!(
        res.status() == reqwest::StatusCode::BAD_REQUEST
            || res.status() == reqwest::StatusCode::NOT_FOUND,
        "traversal path should be refused, got {}",
        res.status()
    );

    println!("pay-per-query search OK");
    Ok(())
}

fn print_hits(body: &serde_json::Value) {
    if let Some(results) = body["results"].as_array() {
        for hit in results {
            let text = hit["text"].as_str().unwrap_or("<no text>");
            let text: String = text.chars().take(70).collect();
            println!(
                "  hit id={} score={:.3} {}…",
                hit["id"], hit["score"].as_f64().unwrap_or(0.0), text
            );
        }
    }
}

fn header(res: &reqwest::Response, name: &str) -> Option<String> {
    res.headers()
        .get(name)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
}
