# CLAUDE.md — Sluice

## What this is
Sluice is a self-hostable, pay-per-request gateway. It sits in front of an HTTP
origin and enforces x402 payment before forwarding the request, settling in USDC
over the x402 protocol. The gateway inspects the payment and returns yes/no; a
facilitator does all the on-chain work.

Backend-agnostic by design. Firn (multi-tenant vector + full-text search on S3)
is the reference backend and the flagship demo: pay-per-query search. Any HTTP
origin works.

The Firn parallel, since that is the mental model:
- Firn   = rust api + foyer + lance on s3
- Sluice = axum gateway + x402-axum + x402-rs facilitator, USDC on Base

## Scope guardrails
- Sluice is the gateway and its glue. It does NOT reimplement the x402 protocol,
  payment verification, or settlement. That is the facilitator's job (pulled in).
- Keep the origin pluggable. Firn is a target, not a dependency. No Firn-specific
  logic in the gateway core.
- EVM only for v1 (Base). No Solana path yet.
- No glacier-themed names anywhere in this repo.

## Parts: build vs pull in
Built here (single Cargo workspace):
- `gateway/` — axum service. Reverse-proxies to the origin. x402-axum layer on
  paid routes. This is the reusable artifact.
- `rules/`   — route/caller to price policy: which paths, which callers, what
  price. The novel piece. Keep it a clean, table-driven, testable module that the
  gateway consumes. Can start as a module inside gateway and graduate to its own
  crate if it grows.
- `client/`  — thin test client over x402-reqwest. Signs a payment, retries,
  proves the loop end to end.
- `indexer/` — (milestone 4) consumes settlement receipts and writes them to
  Postgres so payments are queryable.

Pulled in (not written here):
- Firn — runs as a container; the reference origin.
- x402-rs — provides the facilitator binary plus the `x402-axum` and
  `x402-reqwest` crates. Do NOT fork it; depend on it.
- anvil (Foundry) — local EVM for offline dev.
- OTel collector, Prometheus, Grafana — x402-rs already emits OpenTelemetry, so
  this is configuration, not code.
- Postgres — only once the indexer exists.

## Repo layout
```
sluice/
├── Cargo.toml                    # workspace
├── gateway/                      # axum + x402-axum reverse proxy (built)
│   └── src/
├── rules/                        # price policy module (built)
│   └── src/
├── client/                       # x402-reqwest test client (built)
│   └── src/
├── indexer/                      # settlement -> Postgres (built, milestone 4)
│   └── src/
├── migrations/                   # indexer schema (migrations only)
├── config/                       # mounted service configs (facilitator.json)
├── scripts/                      # fund-usdc.sh, cargo.sh (dockerized cargo)
├── docker-compose.yml            # rung 1: anvil + facilitator + gateway + firn + otel
├── docker-compose.testnet.yml    # override: swap anvil for Base Sepolia RPC
├── deploy/                       # terraform for ECS/RDS (production, later)
├── docs/                         # layered build notes
└── CLAUDE.md
```

## Local development (docker)
Everything except the chain runs in docker compose; the chain is reached over
RPC. Nothing needs installing on the host beyond docker and cargo.

Three rungs. Local dev is rung 1, with rung 2 available for a real-chain smoke
test. Rung 3 is production (see below).

Rung 1 — offline, the primary dev loop:
- anvil in compose, forking Base mainnet state (`--fork-url <base-rpc>`) so the
  real USDC contract and its `transferWithAuthorization` behave correctly
  locally, with funded test accounts and fake value.
- facilitator points at anvil. gateway points at the facilitator. gateway
  reverse-proxies to Firn.
- No real money, tight loop.
- Fully air-gapped fallback: deploy a mock EIP-3009 token instead of forking, for
  when you have no RPC at all.

Rung 2 — testnet:
- Drop anvil. Point the facilitator RPC at Base Sepolia. Use the canonical Base
  Sepolia USDC (confirm the current address from Circle's docs; do not hardcode a
  guessed address).
- Everything else unchanged. This is `docker-compose.testnet.yml`.

## Build sequence (milestones)
Layered. Each step is demoable on its own; build the picture incrementally.

1. One paid request, end to end. Gateway with x402-axum on a single route,
   facilitator on anvil, proxying to a trivial Firn endpoint. Client pays, gets a
   result. Nothing else. (x402-axum wires up as `X402Middleware::try_from(<facilitator_url>)`
   applied as a layer with a price tag; keep it minimal.)
2. Rules layer. Route/caller to price: a free route, a paid route, a per-caller
   price. Table-driven and tested.
3. Firn flagship demo. Point the paid route at a real Firn search endpoint and
   monetize a tenant's queries. This is the part that is uniquely yours.
4. Payments indexer. Consume settlement receipts into Postgres. `SELECT` who paid
   for what. Schema via migrations only.
5. Observability. OTel on the settlement path, Grafana dashboard: paid requests,
   settle latency, revenue.
6. Real-chain smoke test on Base Sepolia (rung 2), then write it up.

## Progressing to production (rung 3)
Local proves it. Production is the finale, and the ECS/Terraform step doubles as
the production-infra proof.
- Compute: gateway and facilitator on ECS (Fargate). Firn wherever it already runs.
- Chain: point the facilitator RPC at Base mainnet via an RPC provider (or your
  own node). Real USDC, real cents.
- Secrets: facilitator wallet key in Secrets Manager, injected as an env var.
  Never in an image or in the repo.
- Data: S3 is already there. Indexer Postgres on RDS.
- Observability: OTel to a managed backend, or self-run Grafana.
- Terraform: standard module layout. Note `aws_lb_listener_rule` uses singular
  `condition` and `action`, not plurals.
- Bring the prod topology up on testnet first, then flip RPC + USDC to mainnet as
  the last change.

## Key gotchas
- Anvil's publicly known dev accounts must NOT be x402 payers on a mainnet
  fork: they carry real EIP-7702 delegations on Base, so forked USDC treats
  them as ERC-1271 wallets and rejects ECDSA authorizations. Use a fresh
  keypair for the client (see docker-compose.yml). Dev accounts remain fine as
  facilitator signer and pay-to.
- Run forked anvil with `--block-time N`, or chain time freezes at the fork
  block and EIP-3009 `validAfter` checks fail ("authorization is not yet valid").
- If the host lacks a C toolchain, cargo cannot link; use `scripts/cargo.sh`
  (cargo inside the rust image, cached, incremental).
- x402 gasless transfer relies on EIP-3009 `transferWithAuthorization`. USDC
  implements it natively; most tokens do not. Locally, fork Base or deploy a mock
  that supports it.
- The facilitator never holds funds. It verifies and broadcasts a client-signed
  transfer. Treat its wallet key as broadcast-only, but still a secret.
- Testnet before mainnet, always. Confirm the current Base Sepolia USDC address
  from Circle rather than trusting a cached value.
- Keep verify/settle inside the facilitator. If gateway code starts doing chain
  work, stop and rethink the boundary.

## Working agreement
- Propose before implementing. For any multi-file change, outline the plan first
  and wait for sign-off.
- Two attempts max on a failing fix. If it fails twice, stop, reassess, and
  propose a different approach.
- Schema changes via migrations only. No ad-hoc DDL.
- No AI attribution in commit messages.
- Keep this file tight. Update it as the shape changes; prefer small incremental
  edits that keep the picture whole.
