use std::{env, sync::Arc};

use alloy_signer_local::PrivateKeySigner;
use anyhow::Context;
use x402_chain_eip155::V2Eip155ExactClient;
use x402_reqwest::{ReqwestWithPayments, ReqwestWithPaymentsBuild, X402Client};

/// Proves the paid loop end to end against the gateway:
/// free route -> 200, paid route without payment -> 402, paid route with an
/// x402 signer -> sign, retry, 200 + settlement response header.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let gateway = env::var("GATEWAY_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let signer: PrivateKeySigner = env::var("CLIENT_PRIVATE_KEY")
        .context("CLIENT_PRIVATE_KEY is required")?
        .parse()
        .context("CLIENT_PRIVATE_KEY is not a valid private key")?;
    println!("client signer: {}", signer.address());

    let plain = reqwest::Client::new();

    // 1. Free route: no payment involved.
    let res = plain.get(format!("{gateway}/healthz")).send().await?;
    println!("GET /healthz -> {}", res.status());

    // 2. Paid route without payment: expect 402 + payment requirements.
    let res = plain.get(format!("{gateway}/firn/health")).send().await?;
    let status = res.status();
    let requirements = res
        .headers()
        .get("payment-required")
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    println!("GET /firn/health (no payment) -> {status}");
    println!(
        "  payment-required (base64): {}",
        requirements.as_deref().unwrap_or("<missing>")
    );
    anyhow::ensure!(
        status == reqwest::StatusCode::PAYMENT_REQUIRED,
        "expected 402 without payment, got {status}"
    );

    // 3. Paid route through x402-reqwest: signs an EIP-3009 authorization and
    //    retries automatically on 402.
    let x402 = X402Client::new().register(V2Eip155ExactClient::new(Arc::new(signer)));
    let paying = reqwest::Client::new().with_payments(x402).build();
    let res = paying.get(format!("{gateway}/firn/health")).send().await?;
    let status = res.status();
    let settlement = res
        .headers()
        .get("payment-response")
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    println!("GET /firn/health (x402) -> {status}");
    println!("  body: {}", res.text().await?);
    match settlement {
        Some(s) => println!("  payment-response (base64): {s}"),
        None => println!("  payment-response header missing"),
    }
    anyhow::ensure!(status.is_success(), "paid request failed with {status}");
    println!("paid loop OK");
    Ok(())
}
