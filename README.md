# Sluice

[![ci](https://github.com/gordonmurray/Sluice/actions/workflows/ci.yml/badge.svg)](https://github.com/gordonmurray/Sluice/actions/workflows/ci.yml)

Sluice is a self-hostable HTTP gateway that demonstrates payment per request
with x402 v2 and USDC on Base. A client requests a paid route, receives a
`402 Payment Required`, signs the payment, and retries. An external facilitator
verifies and settles the payment before Sluice forwards the request to the origin.

The repository includes the gateway, a small search origin, a payment-aware
client, a Postgres receipt indexer, and a Grafana dashboard. The complete local
example uses fake funds. You do not need a wallet, faucet, or cloud account.

```text
client -> Sluice gateway -> HTTP origin
                |
                +-> facilitator -> chain settlement
                |
                +-> receipt indexer -> Postgres
```

Sluice builds on [x402-rs](https://github.com/x402-rs/x402-rs), axum, and alloy.
The gateway does not hold private keys or broadcast chain transactions.
The facilitator broadcasts client-authorized transfers and pays transaction gas.

## Watch a paid request

![A local Sluice request returns 402, pays with mock USDC, and returns a search result with an indexed receipt](docs/images/payment-demo.gif)

Recorded against the offline demo with fake funds. The recording shows a free
health check, a decoded payment quote, a paid search, and settlement checks.
See [recording instructions](docs/recordings/README.md) to replay or reproduce it.

## Run the complete example

Install Docker Engine with Docker Compose, Bash, and Python 3. Allow several
minutes for the first build. Images and build dependencies require network access;
the offline chain itself does not use an external RPC.

```sh
git clone https://github.com/gordonmurray/Sluice.git
cd Sluice
./scripts/smoke.sh
```

The script starts the offline stack, runs the client, and checks:

- Health endpoints are free and unpaid searches return 402.
- A base-price search pays 50,000 micro-USDC ($0.05 in token units).
- An authenticated caller pays 20,000 micro-USDC ($0.02).
- A caller cannot obtain that discount by claiming another caller's identity.
- Both transactions succeed and the receiver gains exactly 70,000 micro-USDC.
- Both receipts reach Postgres with the expected amounts and response status.
- Administrative paths and traversal attempts are refused.

All amounts in this example are fake value. Run it without other clients sending
payments concurrently, because the script checks the exact receiver balance change.
The script leaves containers running for inspection and prints their shutdown command.

## Explore the running stack

```sh
curl -i localhost:8080/healthz
curl -i -X POST localhost:8080/firn/ns/demo/query
```

Open [Grafana](http://localhost:3001/d/sluice-payments) to inspect payment and request metrics.
See [observability](docs/observability.md) for dashboard semantics and receipt queries.
The `/firn` path prefix preserves compatibility with the optional Firn origin;
the default origin is the small search service included here.

To run the payment client again:

```sh
docker compose -f docker-compose.yml -f docker-compose.offline.yml run --rm client
```

Stop the offline stack:

```sh
docker compose -f docker-compose.yml -f docker-compose.offline.yml down
```

Postgres data survives shutdown; the local chain does not. Old receipt rows can
therefore refer to an earlier local chain. To discard the demo database as well,
add `--volumes` to the shutdown command. Use this only when those local records
are no longer needed.

The demo ports bind to localhost. All credentials in Compose are public local
test values. This configuration is not a public mainnet deployment.

## Troubleshooting

If startup or payment fails, inspect the services and recent logs:

```sh
docker compose -f docker-compose.yml -f docker-compose.offline.yml ps -a
docker compose -f docker-compose.yml -f docker-compose.offline.yml logs --tail=100
```

The gateway uses port 8080, Anvil uses 8545, and Grafana uses 3001 on localhost.
Stop conflicting local services before starting the demo. Stop the current stack
before switching between offline and fork modes. If a public fork RPC fails,
use offline mode or supply `BASE_FORK_RPC`.

The smoke script uses the committed demo prices and caller keys. Restore those
configuration values before running it after a pricing experiment.

## How payment works

1. An unpaid request returns 402 and a base64 `payment-required` header with the price, asset, network, and receiver.
2. The client signs an EIP-3009 authorization off-chain and retries with `Payment-Signature`.
3. The facilitator verifies and settles the authorization before the origin handles the paid request.
4. The response includes a `payment-response` settlement receipt. Sluice reports the origin status to the indexer asynchronously.

See [the wire-format walkthrough](docs/wire-format.md) for decoded examples and
`scripts/pay-with-curl.sh` for a local curl demonstration.

## Pricing and origin configuration

`config/rules.json` defines free and paid path prefixes. The longest matching
prefix wins, with matching limited to whole path segments. Unmatched paths are denied.
Rules currently match paths, not HTTP methods. A free `/` rule is a catch-all.

`config/callers.json` maps API keys to caller identities for discounted prices.
Clients send `x-sluice-api-key`; an unauthenticated caller claim receives the base price.
Keys load at startup. Rules reload every two seconds by default, and invalid edits
leave the previous rules active. Set `RULES_RELOAD_SECS=0` to disable reloads.
Mount the configuration directory, not individual files, when using reloads.

Set `ORIGIN_URL` to the upstream HTTP service and use `STRIP_PREFIX` when the
public route has a prefix the origin does not use. Keep the origin private so
clients cannot bypass payment. `/healthz` and `/metrics` belong to the gateway.

## Other configurations

To run against real USDC bytecode on a local Base fork, stop the offline stack first:

```sh
./scripts/smoke.sh fork
```

This still uses fake funds, but needs an upstream RPC. Set `BASE_FORK_RPC` if the
default public endpoint is unavailable. Do not mix fork and offline configurations
on a running stack.

To use [Firn](https://github.com/gordonmurray/firnflow) as the origin, clone it next
to this repository and use the override:

```sh
git clone https://github.com/gordonmurray/firnflow ../firnflow
docker compose -f docker-compose.yml -f docker-compose.firn.yml up -d --build
docker compose -f docker-compose.yml -f docker-compose.firn.yml run --rm client
```

The Firn override builds a separate project and requires its own compatible checkout.
[Base Sepolia setup](docs/testnet.md) remains planned; the local demonstration does
not establish testnet or mainnet readiness.

## Limits

- Payment settles before origin validation. A paid request can subsequently fail or return 413 for an oversized body.
- Requests are buffered up to 10 MiB. WebSockets and streaming uploads are unsupported.
- Responses stream through the proxy. Recorded status reflects headers, not complete body delivery.
- Receipt delivery is best-effort. A crash or indexer outage can leave a settled payment missing from Postgres.
- Refunds are operator decisions. There is no automatic retry, refund, or customer credit balance.

See [paid-but-failed requests](docs/paid-but-failed.md) for details.

## Development

Read [CONTRIBUTING.md](CONTRIBUTING.md) for build and test commands and the
[project plan](docs/project-plan.md) for the remaining work. CI runs Rust checks
and the offline payment demonstration. See [local verification](docs/verification.md)
for the latest recorded checks and their limits.

### Optional Bazaar discovery

Set `BAZAAR_CONFIG_PATH` to a public JSON file with `description` (1–500 characters)
and `bazaar` (the protocol's `info`, `schema`, and optional `routeTemplate` declaration).
This gateway option describes one GET service across its paid routes. Use it only
when every paid route shares that contract. The configuration is copied into
`PAYMENT-REQUIRED.extensions.bazaar`; clients must preserve the quoted extensions
in their payment payload. Keep product schemas in the consuming repository.

Use the facilitator's endpoint validator after deployment. Advertising an extension
is not evidence of catalog indexing: confirm a settled call and query the remote
catalog separately. No registration request or signing credential is sent by this option.
