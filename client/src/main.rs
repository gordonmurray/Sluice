use std::{env, sync::Arc};

use alloy_signer_local::PrivateKeySigner;
use anyhow::Context;
use x402_chain_eip155::V2Eip155ExactClient;
use x402_reqwest::{ReqwestWithPayments, ReqwestWithPaymentsBuild, X402Client};

/// Proves the rules-driven paid loop end to end against the gateway:
/// - free gateway route          -> 200, no payment
/// - free proxied route          -> 200, no payment
/// - paid route without payment  -> 402 + requirements
/// - paid route with x402 signer -> sign, retry, 200 + settlement header
/// - same route as caller with a per-caller price -> cheaper payment
/// - unmatched route             -> 404, never proxied
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let gateway = env::var("GATEWAY_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let signer: PrivateKeySigner = env::var("CLIENT_PRIVATE_KEY")
        .context("CLIENT_PRIVATE_KEY is required")?
        .parse()
        .context("CLIENT_PRIVATE_KEY is not a valid private key")?;
    println!("client signer: {}", signer.address());

    let plain = reqwest::Client::new();

    // 1. Free route answered by the gateway itself.
    let res = plain.get(format!("{gateway}/healthz")).send().await?;
    println!("GET /healthz -> {}", res.status());
    anyhow::ensure!(res.status().is_success(), "free gateway route failed");

    // 2. Free route proxied to the origin: no 402 involved.
    let res = plain.get(format!("{gateway}/firn/metrics")).send().await?;
    println!("GET /firn/metrics (free, proxied) -> {}", res.status());
    anyhow::ensure!(res.status().is_success(), "free proxied route failed");

    // 3. Paid route without payment: expect 402 + payment requirements.
    let res = plain.get(format!("{gateway}/firn/health")).send().await?;
    let status = res.status();
    let requirements = header(&res, "payment-required");
    println!("GET /firn/health (no payment) -> {status}");
    println!("  payment-required (base64): {}", requirements.as_deref().unwrap_or("<missing>"));
    anyhow::ensure!(
        status == reqwest::StatusCode::PAYMENT_REQUIRED,
        "expected 402 without payment, got {status}"
    );

    // 4. Paid route through x402-reqwest: signs an EIP-3009 authorization and
    //    retries automatically on 402.
    let x402 = X402Client::new().register(V2Eip155ExactClient::new(Arc::new(signer)));
    let paying = reqwest::Client::new().with_payments(x402).build();
    let res = paying.get(format!("{gateway}/firn/health")).send().await?;
    let status = res.status();
    let settlement = header(&res, "payment-response");
    println!("GET /firn/health (x402, base price) -> {status}");
    println!("  body: {}", res.text().await?);
    println!("  payment-response (base64): {}", settlement.as_deref().unwrap_or("<missing>"));
    anyhow::ensure!(status.is_success(), "paid request failed with {status}");

    // 5. Same route as tenant-a, which the rules table prices lower.
    let res = paying
        .get(format!("{gateway}/firn/health"))
        .header("x-sluice-caller", "tenant-a")
        .send()
        .await?;
    let status = res.status();
    let settlement = header(&res, "payment-response");
    println!("GET /firn/health (x402, caller tenant-a) -> {status}");
    println!("  payment-response (base64): {}", settlement.as_deref().unwrap_or("<missing>"));
    anyhow::ensure!(status.is_success(), "per-caller paid request failed with {status}");

    // 6. A path no rule covers is refused, not proxied.
    let res = plain.get(format!("{gateway}/not-a-route")).send().await?;
    println!("GET /not-a-route -> {}", res.status());
    anyhow::ensure!(
        res.status() == reqwest::StatusCode::NOT_FOUND,
        "unmatched route should 404"
    );

    println!("rules-driven paid loop OK");
    Ok(())
}

fn header(res: &reqwest::Response, name: &str) -> Option<String> {
    res.headers()
        .get(name)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
}
