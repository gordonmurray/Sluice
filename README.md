# Sluice

[![ci](https://github.com/gordonmurray/Sluice/actions/workflows/ci.yml/badge.svg)](https://github.com/gordonmurray/Sluice/actions/workflows/ci.yml)

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

The demo meters pay-per-query search at $0.05 a query ($0.02 for one
configured caller, authenticated by API key: `config/callers.json` maps
keys to caller ids, presented as `x-sluice-api-key`; unauthenticated caller
claims are priced at the base rate). Out of the box the origin is a small
built-in search stand-in, so this repo alone runs the whole loop; the
flagship demo meters [Firn](https://github.com/gordonmurray/firnflow), a
multi-tenant vector and full-text search engine, via a compose override.
The origin is pluggable; any request/response HTTP service can be metered
the same way (request bodies are capped at 10 MiB, and WebSockets and
streaming uploads are not supported yet). Pricing is a table edit, not a
code change.

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

A fresh clone of this repo is all you need; the default origin is the
built-in demo search service:

```sh
git clone https://github.com/gordonmurray/sluice
cd sluice
```

```sh
docker compose up -d --build                # the whole stack; corpus built in, client funded
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
`localhost:3001/d/sluice-payments` (revenue, paid requests, settle latency,
and gateway request rates/latency by pricing decision). The gateway exposes
Prometheus metrics at `/metrics`; that path and `/healthz` are the gateway's
own and are never proxied, though an origin's endpoints stay reachable under
their prefix, e.g. `/firn/metrics`.

This is the dashboard after a few minutes of demo traffic on the fork: 34
paid requests, $1.37 of fake USDC, and one request that paid, hit a stopped
origin, and landed in the refund queue. Settle latency sits around 7.5 s p50
here; most of that is waiting for the next anvil block, mined every 5
seconds on the fork.

![Grafana: revenue, paid requests, settle latency, and gateway decisions](docs/images/grafana-dashboard.png)

The decision panel separates quotes from sales: `paid / 402` is a price
quote served, `paid / 200` is a settled request that reached the origin,
`deny / 404` is a path no rule covers, and `paid / 502` is the one that
belongs in the refund queue.

![Grafana: gateway requests by pricing decision](docs/images/grafana-requests-by-decision.png)

### The same loop with curl

The demo client hides the wire format, so here it is in the raw. A request
without payment gets the quote:

```
$ curl -i -X POST localhost:8080/firn/ns/demo/query \
    -H 'content-type: application/json' \
    -d '{"text": "gasless payments without ETH", "k": 3}'

HTTP/1.1 402 Payment Required
payment-required: eyJ4NDAyVmVyc2lvbiI6MiwiZXJyb3IiOiJQYXltZW50LVNpZ25hdHVyZSBoZWFkZXIgaXMg…
content-length: 0
```

The `payment-required` header is base64. Decoded, it says exactly what to
sign: 50000 micro-USDC (six decimals, so $0.05) to the pay-to address, USDC
on Base, EIP-3009:

```json
{
  "x402Version": 2,
  "accepts": [{
    "scheme": "exact",
    "network": "eip155:8453",
    "amount": "50000",
    "payTo": "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
    "asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    "extra": { "assetTransferMethod": "eip3009", "name": "USD Coin", "version": "2" }
  }]
}
```

Paying is signing an EIP-712 `transferWithAuthorization` for that exact
amount and retrying with the result in a `Payment-Signature` header.
`scripts/pay-with-curl.sh` does the whole exchange with curl, `cast` (run
inside the anvil container) and python3, no x402 client library:

```
$ ./scripts/pay-with-curl.sh

HTTP/1.1 200 OK
content-type: application/json
payment-response: eyJuZXR3b3JrIjoiZWlwMTU1Ojg0NTMiLCJwYXllciI6IjB4YTc0OTBGRkQ2ZkZBRjlDNjI5…

{"results":[{"id":1,"score":0.25,"text":"x402 is an open protocol for HTTP-native payments: …
```

The `payment-response` header decodes to the settlement receipt, on-chain
before the origin did any work:

```json
{
  "network": "eip155:8453",
  "payer": "0xa7490FFD6fFAF9C629a1E1Be4875E6b7700943DA",
  "success": true,
  "transaction": "0xb796b89437aec331092839cdc967c7229d5f27d1cf9539dc7c6f8d8ce9a8aa7e"
}
```

### Offline mode

No internet (or a flaky RPC)? The offline override skips the fork entirely:
anvil runs a fresh chain with Base's chain id, and a minimal EIP-3009 token
(`contracts/MockUSDC.sol`, pre-compiled so no compiler is needed) stands in
for USDC, mirroring its EIP-712 domain fields (name, version, chain id;
the verifying contract necessarily differs, so signatures are not portable
between the forked and offline chains). The x402 infrastructure contracts the
facilitator expects on-chain are installed from committed bytecode
snapshots. Nothing dials out:

```sh
docker compose -f docker-compose.yml -f docker-compose.offline.yml up -d --build
docker compose -f docker-compose.yml -f docker-compose.offline.yml run --rm client
```

### Metering Firn instead

To meter the real Firn instead of the built-in stand-in, clone it next to
this repo and add the override:

```sh
git clone https://github.com/gordonmurray/firnflow   # next to sluice/
docker compose -f docker-compose.yml -f docker-compose.firn.yml up -d --build
docker compose -f docker-compose.yml -f docker-compose.firn.yml run --rm client
```

Every credential in the compose files (private keys, Postgres, Grafana, and
MinIO in the Firn override) is a local demo value for the fork, and none of them are suitable
for a deployment.

## Pricing

Prices live in `config/rules.json` and hot-reload: the gateway re-reads the
file every couple of seconds (`RULES_RELOAD_SECS`, 0 disables) and swaps the
table in atomically. A malformed edit is logged and ignored; the previous
table keeps serving until the file parses again.

Built local-first: the offline anvil loop above is the primary dev
environment. Base Sepolia comes next ([what that needs](docs/testnet.md)),
then production with real USDC on Base mainnet.
