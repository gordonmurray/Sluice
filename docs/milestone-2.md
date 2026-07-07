# Milestone 2 — rules layer

## What was built

- `rules/` crate: table-driven route/caller → price policy. A `RuleSet` loads
  from JSON and answers `decide(path, caller) -> Free | Paid{micro_usdc} |
  Deny`. Longest-prefix match on whole path segments; per-caller prices
  override the rule's base price; anything unmatched is denied. Prices are
  parsed from decimal strings into micro-USDC without floats. The crate knows
  nothing about x402, chains, or assets — 13 unit tests, no dependencies
  beyond serde.
- Gateway integration: the hardcoded paid route is gone. A fallback route
  covers all paths, with x402-axum's `with_dynamic_price` deriving price tags
  per request from the rules table (`Paid` → price tag, `Free` → no tag,
  `Deny` → no tag + 404 in the handler behind the layer). Caller id comes
  from the `x-sluice-caller` header — a pricing hint, not authenticated
  identity, in rung 1.
- The `/firn` coupling moved out of gateway code: `STRIP_PREFIX` env decides
  what prefix to strip before proxying (was a Codex review deferral from M1).
- `config/rules.json` (mounted): `/firn/metrics` free, `/firn/health` at
  0.01 USDC with `tenant-a` priced at 0.002.

## How to run

```sh
docker compose up -d --build
docker compose run --rm client
scripts/cargo.sh test -p rules   # unit tests
```

## Verified output

```
GET /healthz                        -> 200 (gateway, free)
GET /firn/metrics (free, proxied)   -> 200, no payment involved
GET /firn/health  (no payment)      -> 402, requirements amount=10000
GET /firn/health  (x402 base price) -> 200, settled tx 0xfbd2...
GET /firn/health  (x402 tenant-a)   -> 200, settled tx 0x0ac9...
GET /not-a-route                    -> 404, never proxied
pay-to balance delta: exactly 12000 micro-USDC (0.01 + 0.002)
cargo test -p rules: 13 passed, 0 failed
```

## Review

Codex reviewed the diff. Applied: duplicate/empty/non-UTF-8 caller headers
now deterministically collapse to "no caller" via one shared helper (so the
pricer and the proxy can never disagree, and nobody gets charged then
rejected); the caller header is stripped before forwarding (it must not reach
the origin looking like tenant identity); `STRIP_PREFIX` is segment-aware
(`/firnabc` no longer becomes `abc`); the rules loader rejects conflicting
pricing, unknown `pricing` values, `caller_prices` on free rules, and
non-absolute prefixes. Test count: 17 rules + 3 gateway.

Deferred, documented: per-caller pricing rides an *unauthenticated* header —
fine for rung-1 discounts on a local fork, but real caller identity needs
authentication before this feature carries real money (Codex is right that a
"pricing hint" with a cheaper price is a discount anyone can claim). Also
deferred: path canonicalization (rules match the raw path; encoded slashes
and dot-segments are not normalized) and a mock-origin integration test
proving denied paths never reach the origin.

## What surprised me

- x402-axum's dynamic pricing hook (`with_dynamic_price`) made the "empty
  price tags = free" path trivial — the same closure covers free, paid, and
  per-caller pricing, so the gateway needed no custom middleware.
- The dynamic-price closure's third parameter is `Option<&Url>` where `Url`
  is the `url` crate's type; `reqwest::Url` is the same type re-exported, so
  no new dependency was needed to name it.
- Denial needs enforcing *behind* the payment layer: a rule miss produces no
  price tag, which the x402 layer reads as "free — pass through". The proxy
  handler re-checks the table and 404s. Forgetting that would quietly expose
  every unlisted origin path for free.
