# Changelog

All notable changes to Sluice are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Pay-per-request gateway implementing the x402 `exact` scheme: an unpaid request to a metered path returns `402 Payment Required` with base64-encoded x402 requirements (price, asset, network, pay-to address) in a `payment-required` header, and a request carrying a signed EIP-3009 authorisation is verified and settled through a facilitator, then proxied to the origin. USDC on Base, gasless for the client.
- Settle-before-forward flow: settlement lands on-chain before the request reaches the origin, so the origin never does unpaid work. The gateway holds no private key and no balance; the facilitator broadcasts the settlement and pays the gas.
- Pluggable origins: any request/response HTTP service can be metered behind the gateway. A built-in demo search origin ships in the repo so a fresh clone runs the whole paid loop on its own, and a compose override meters Firn instead.
- Pricing rules table in `config/rules.json` with hot reload: the gateway re-reads the file on an interval (`RULES_RELOAD_SECS`, `0` disables) and swaps the table in atomically; a malformed edit is logged and ignored while the previous table keeps serving. Pricing is a table edit, not a code change.
- Per-caller pricing: API keys in `config/callers.json` map to caller ids presented as an `x-sluice-api-key` header; unauthenticated callers are priced at the base rate.
- Path canonicalisation policy: routes are matched on the raw path and ambiguous paths are rejected, so pricing rules cannot be sidestepped by encoding.
- Settlement recording with an operator-driven refund policy: every settlement is recorded with the origin's own outcome (`origin_status`), and refunds are an explicit operator decision made from the payments table rather than an automatic gateway action. No automatic retry, no automatic credit, so the gateway stays stateless and the money trail stays auditable.
- Payments indexer: settlements are persisted to Postgres and queryable out of band.
- Observability: Prometheus metrics at `/metrics` and a Grafana dashboard covering revenue, paid requests, settle latency, and gateway decisions by pricing outcome (`paid/402`, `paid/200`, `deny/404`, `paid/502`). `/healthz` and `/metrics` are the gateway's own paths and are never proxied.
- Shared-token requirement between the gateway and the indexer.
- Request bodies capped at 10 MiB (`413` when exceeded).
- Offline development mode: a compose override runs a fresh anvil chain with Base's chain id and a pre-compiled mock EIP-3009 token (`contracts/MockUSDC.sol`) in place of USDC, with the x402 infrastructure contracts installed from committed bytecode snapshots, so the full loop runs with no internet and no real-money settlement.
- Local development stack via Docker Compose: anvil forking Base mainnet, the gateway, the demo origin, Postgres, and Grafana, with the client funded and the demo corpus seeded.
- Integration tests exercising the full gateway stack against a counting mock origin.
- Continuous integration running the workspace test suite on pull requests and `main`.
- Documentation: a README covering what Sluice is and the paid-request flow with a raw-curl walkthrough, `docs/testnet.md` (Base Sepolia prerequisites), and `docs/paid-but-failed.md` (the paid-but-failed policy).

[Unreleased]: https://github.com/gordonmurray/Sluice/commits/main
