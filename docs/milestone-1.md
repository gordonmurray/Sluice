# Milestone 1 — one paid request, end to end

## What I built

- `gateway/` — axum service with two routes: `GET /healthz` (free, answered
  by the gateway) and `GET /firn/health` (paid: 0.01 USDC on Base, x402-axum
  layer, reverse-proxied to the Firn origin with the `/firn` prefix
  stripped). The gateway never touches the chain; verify/settle happens in
  the facilitator.
- `client/` — x402-reqwest test client proving the loop: free 200, bare 402
  with payment requirements, signed EIP-3009 retry, 200 plus settlement
  header.
- `docker-compose.yml` — rung 1 stack: anvil (forking Base mainnet), a
  one-shot `fund-usdc`, the prebuilt x402 facilitator, MinIO plus Firn
  (built from the local `../firnflow` checkout), and the gateway.
- `config/facilitator.json` — facilitator config: chain `eip155:8453` over
  `http://anvil:8545`, scheme `v2-eip155-exact`, signer from
  `$FACILITATOR_PRIVATE_KEY`.
- `scripts/fund-usdc.sh` — mints test USDC to the client account by
  impersonating the real USDC masterMinter on the fork (anvil cheatcodes).
- `scripts/cargo.sh` — dockerised cargo wrapper (see the surprises below).

Accounts (fake value only): anvil dev account #0 is the facilitator signer,
anvil dev account #2 is pay-to, and the client is a fresh keypair generated
for this repo (`0xa749...43DA`). The EIP-7702 surprise below explains why it
cannot be an anvil dev account.

## How to run

Commands as of this milestone; later milestones move the paid route.

```sh
docker compose up -d --build       # anvil, facilitator, minio, firn, gateway
curl -i localhost:8080/healthz     # 200, free
curl -i localhost:8080/firn/health # 402 + payment requirements
docker compose run --rm client     # 402 -> sign -> retry -> 200
```

Settlement proof against the fork:

```sh
docker exec sluice-anvil cast call 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913 \
  "balanceOf(address)(uint256)" 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC \
  --rpc-url http://127.0.0.1:8545
```

The pay-to balance grows by 10000 (0.01 USDC, 6 decimals) per paid request.

## Verified output

From a clean slate (`docker compose down -v && docker compose up -d --build`,
fund-usdc runs automatically):

```
GET /healthz                    -> 200 OK (no payment)
GET /firn/health  (no payment)  -> 402 Payment Required
   payment-required (decoded): scheme=exact network=eip155:8453 amount=10000
   payTo=0x3C44...93BC asset=0x8335...2913 (USDC) method=eip3009
GET /firn/health  (x402 client) -> 200 OK, body "ok" (Firn /health via proxy)
   payment-response (decoded): {"network":"eip155:8453",
     "payer":"0xa7490FFD6fFAF9C629a1E1Be4875E6b7700943DA","success":true,
     "transaction":"0xea3aaed2981d649f8f35bfaab21b54bc7fee6a3a96653bbcf0af20fbbd139f30"}
pay-to USDC balance: 0 before -> 10000 after (exactly 0.01 USDC, 6 decimals)
cast receipt <tx>: status 1, to=USDC contract, selector 0xe3ee160e
                   (transferWithAuthorization), from=facilitator signer
```

## Review

Codex (gpt-5.5) reviewed the milestone diff. Applied: strip
`Connection`-nominated hop-by-hop headers both ways, stream origin responses
instead of buffering them unbounded, return 413 (not 502) for oversized
request bodies, and fix stale account docs. Deferred: the `/firn` prefix
hardcoded in the gateway (milestone 2's rules layer makes routes
table-driven), compose healthcheck-based startup ordering (the x402 layer
fetches facilitator config lazily, so ordering has not bitten), and moving
rung-1 keys to `.env` (they are documented fake-value dev keys; the testnet
override will use real secrets handling). Codex's own bwrap sandbox cannot
run on this host (user namespaces blocked), so it reviewed the diff piped in
via stdin.

## What surprised me

- **Anvil's publicly known dev accounts are unusable as x402 payers on a
  Base mainnet fork.** Their keys are public, so people have installed
  EIP-7702 delegations on them on the real chain (`cast code` shows
  `0xef0100...`). Forked USDC's SignatureChecker then treats them as
  ERC-1271 contract wallets and rejects plain ECDSA payment authorisations
  with `FiatTokenV2: invalid signature`, even though the signature is
  provably correct. The client uses a fresh keypair generated for this repo
  instead. Dev accounts are still fine as the facilitator signer (sends
  normal transactions) and as pay-to (only receives).
- **A forked anvil's clock freezes at the fork block.** EIP-3009
  authorisations carry `validAfter` near wall clock, so verification fails
  with `authorization is not yet valid` once the fork is minutes old. Fixed
  with `--block-time 5` so chain time tracks wall clock.
- **The host had no usable Rust toolchain.** rustup/cargo installed fine,
  but there is no C toolchain (`cc`/`ld`) on the host and no passwordless
  sudo, so cargo cannot link binaries. All builds run inside the
  `rust:1.96-bookworm` image via `scripts/cargo.sh` (registry cache in a
  named volume, incremental target/ on the bind mount). "Nothing beyond
  docker and cargo" effectively became "nothing beyond docker".
- **The facilitator's README config format is ahead of its released image.**
  The 2.0.1 image wants `{"id": "...", "chains": "eip155:8453"}` (a string
  pattern), not the `{"scheme": ..., "chains": [...]}` shown in the README
  on main.
- **x402-chain-eip155 2.0.1 does not compile without its `telemetry`
  feature** (it references `tracing` outside the feature gate), so the
  feature is on.
- **x402-reqwest 2.0.1 is built against reqwest 0.13.** A 0.12 workspace pin
  silently produces a different `reqwest::Client` type and `with_payments`
  "does not exist". Also, reqwest 0.13 renamed the `rustls-tls` feature to
  `rustls`.
