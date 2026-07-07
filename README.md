# Sluice

Sluice is a self-hostable, pay-per-request gateway. It sits in front of any
HTTP origin and enforces payment before forwarding the request: no payment
gets a `402 Payment Required` with machine-readable requirements, and a
signed USDC payment gets verified, settled on-chain, and proxied through on
the retry.

I built it because charging for an API by the request normally means API
keys, accounts, and an invoicing run. With the
[x402 protocol](https://www.x402.org) the request itself carries the payment
(the `exact` scheme, USDC on Base via EIP-3009), gasless for the client, who
signs off-chain and never needs ETH. The gateway process never submits chain
transactions. A [facilitator](https://github.com/x402-rs/x402-rs) verifies
the client's signed authorisation and broadcasts the settlement, paying the
gas. Sluice settles before forwarding, so the origin never does unpaid work.

```
client ──► gateway (axum + x402-axum) ──► origin (any HTTP service)
                 │
                 ▼ verify / settle
           facilitator ──► Base (USDC, EIP-3009 transferWithAuthorization)
```

The demo meters [Firn](https://github.com/gordonmurray/firnflow), a
multi-tenant vector and full-text search engine, at $0.05 a query ($0.02 for
one configured caller). The origin is pluggable; any request/response HTTP
service can be metered the same way (request bodies are capped at 10 MiB,
and WebSockets and streaming uploads are not supported yet). Pricing is a
table edit, not a code change.

## How a paid request works

1. `POST /firn/ns/demo/query` with no payment returns `402` plus a
   `payment-required` header (base64-encoded x402 requirements: price, asset,
   network, pay-to address).
2. The client signs an EIP-3009 `transferWithAuthorization` for exactly that
   amount (off-chain, no gas) and retries with a `Payment-Signature` header.
3. The gateway asks the facilitator to verify and settle the authorisation
   on-chain, then proxies the request to the origin.
4. The response carries a `payment-response` header with the settlement
   transaction hash.

The client pays per request and the operator receives USDC. The facilitator
in between never takes custody; the signed authorisation is bound to the
exact token, amount, recipient, validity window, and nonce.

## Running locally (no real money)

Everything runs in docker. Anvil forks Base mainnet, so the real USDC
bytecode runs against real forked state locally, with no real-money
settlement.

The demo origin, Firn, builds from a sibling checkout:

```sh
git clone https://github.com/gordonmurray/sluice
git clone https://github.com/gordonmurray/firnflow   # next to sluice/
cd sluice
```

```sh
docker compose up -d --build                # the whole stack, seeded and funded
curl -i localhost:8080/healthz              # 200, free
curl -i -X POST localhost:8080/firn/ns/demo/query   # 402 + payment requirements
docker compose run --rm client              # 402 -> sign -> retry -> 200
```

Prove the settlement landed:

```sh
docker exec sluice-anvil cast call 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913 \
  "balanceOf(address)(uint256)" 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC \
  --rpc-url http://127.0.0.1:8545
```

One client run pays for two searches (50000 + 20000 micro-USDC, six
decimals), so the pay-to balance grows by 70000 per run. The same settlements
are queryable in Postgres and visible in Grafana at
`localhost:3001/d/sluice-payments` (revenue, paid requests, settle latency).

Every credential in `docker-compose.yml` (private keys, MinIO, Postgres,
Grafana) is a local demo value for the fork, and none of them are suitable
for a deployment.

## Repo layout

| Path | What |
|---|---|
| `gateway/` | axum reverse proxy with x402-axum payment enforcement |
| `client/` | test client over x402-reqwest; proves the paid loop |
| `rules/` | route/caller to price policy, table-driven |
| `indexer/` | settlement receipts to Postgres |
| `config/` | mounted service configs, rules table, Grafana provisioning |
| `scripts/` | USDC funding on the fork, Firn seeding, dockerised cargo |
| `docs/` | build notes and [gotchas](docs/gotchas.md) hit along the way |

Built local-first: the offline anvil loop above is the primary dev
environment. Base Sepolia comes next ([what that needs](docs/testnet.md)),
then production with real USDC on Base mainnet.
