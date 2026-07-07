# Sluice

A self-hostable, pay-per-request gateway. Sluice sits in front of any HTTP
origin and enforces payment before forwarding the request: no payment gets a
`402 Payment Required` with machine-readable payment requirements; a signed
USDC payment gets verified, settled on-chain, and proxied through — one retry.

Payments use the [x402 protocol](https://www.x402.org)'s `exact` scheme with
USDC on Base via EIP-3009 — gasless for the client, who signs off-chain and
never needs ETH. The gateway process never submits chain transactions: a
[facilitator](https://github.com/x402-rs/x402-rs) verifies the client's signed
authorization and broadcasts the settlement (paying the gas). Sluice settles
*before* forwarding, so the origin never does unpaid work.

```
client ──► gateway (axum + x402-axum) ──► origin (any HTTP service)
                 │
                 ▼ verify / settle
           facilitator ──► Base (USDC, EIP-3009 transferWithAuthorization)
```

The flagship demo monetizes [Firn](https://github.com/gordonmurray/firnflow)
(multi-tenant vector + full-text search): pay-per-query search. But the origin
is pluggable — anything that speaks HTTP can be metered.

## How a paid request works

1. `GET /firn/health` with no payment → `402` + a `payment-required` header
   (base64-encoded x402 requirements: price, asset, network, pay-to address).
2. The client signs an EIP-3009 `transferWithAuthorization` for exactly that
   amount — off-chain, no gas — and retries with a `Payment-Signature` header.
3. The gateway asks the facilitator to verify (and settle) the authorization
   on-chain, then proxies the request to the origin.
4. The response carries a `payment-response` header with the settlement
   transaction hash.

The client pays per request; the operator receives USDC; the facilitator in
between never takes custody — the signed authorization is bound to the exact
token, amount, recipient, validity window, and nonce.

## Quickstart (local, no real money)

Everything runs in docker. Anvil forks Base mainnet, so the real USDC bytecode
runs against real forked state locally — with no real-money settlement.

```sh
docker compose up -d --build     # anvil fork, facilitator, minio, firn, gateway
curl -i localhost:8080/healthz     # 200 — free route
curl -i localhost:8080/firn/health # 402 — payment required
docker compose run --rm client     # 402 -> sign -> retry -> 200 + tx hash
```

Prove the settlement landed:

```sh
docker exec sluice-anvil cast call 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913 \
  "balanceOf(address)(uint256)" 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC \
  --rpc-url http://127.0.0.1:8545
# grows by 10000 (0.01 USDC, 6 decimals) per paid request
```

All keys in `docker-compose.yml` are fake-value dev keys for the local fork.

## Repo layout

| Path | What |
|---|---|
| `gateway/` | axum reverse proxy with x402-axum payment enforcement |
| `client/` | test client over x402-reqwest; proves the paid loop |
| `rules/` | route/caller → price policy (milestone 2) |
| `indexer/` | settlement receipts → Postgres (milestone 4) |
| `config/` | mounted service configs (facilitator) |
| `scripts/` | USDC funding on the fork, dockerized cargo |
| `docs/` | one note per milestone: what was built, what surprised |

## Status

| Milestone | | |
|---|---|---|
| 1 | One paid request end to end (anvil fork) | ✅ [notes](docs/milestone-1.md) |
| 2 | Rules layer: route/caller → price | ✅ [notes](docs/milestone-2.md) |
| 3 | Firn flagship demo: pay-per-query search | ✅ [notes](docs/milestone-3.md) |
| 4 | Payments indexer → Postgres | ✅ [notes](docs/milestone-4.md) |
| 5 | Observability: OTel + Grafana | ✅ [notes](docs/milestone-5.md) |
| 6 | Base Sepolia smoke test | — |

Built local-first: rung 1 is the offline anvil loop above, rung 2 will swap in
Base Sepolia (`docker-compose.testnet.yml`), rung 3 is production on ECS with
real USDC on Base mainnet.
