# Milestone 4 — payments indexer

## What was built

Settlements are now queryable. The flow: the x402 layer settles the payment
and stores the `SettleResponse` in the request extensions → the gateway
enriches it with request context (path, caller, price, pay-to) and
fire-and-forgets it to the indexer → the indexer writes Postgres.

- `indexer/` crate: a small axum service with `POST /receipts` →
  `INSERT ... ON CONFLICT (tx_hash) DO NOTHING` (idempotent by transaction
  hash). Applies `migrations/` at startup via sqlx's embedded migrator — no
  ad-hoc DDL anywhere.
- `migrations/0001_payments.sql`: the `payments` table — tx_hash (unique),
  network, payer, pay_to, amount_micro_usdc, path, caller, success,
  created_at, plus indexes on payer/path/created_at.
- Gateway: now runs `settle_before_execution()` — which both matches the
  documented design (the origin never does unpaid work) and is what makes
  the settlement visible to the handler. Receipt reporting is spawned off
  the request path; indexing failures are logged, never propagated.
- Compose: `postgres:17-alpine` (volume-backed, healthchecked) + `indexer`
  built from the same workspace image. Gateway gets `INDEXER_URL`.

Dependency note: the spec names Postgres but no driver; the indexer uses
`sqlx` (runtime-tokio + rustls), chosen because its embedded migrator
enforces the migrations-only rule at startup.

## How to run

```sh
docker compose up -d --build
docker compose run --rm client
docker exec sluice-postgres psql -U sluice -d sluice \
  -c "SELECT payer, amount_micro_usdc, path, caller, tx_hash FROM payments;"
```

## Verified output

After one client run (one anonymous query at 0.05, one tenant-a query at 0.02):

```
                   payer                    | amount | path                | caller   | success
 0xa7490FFD6fFAF9C629a1E1Be4875E6b7700943DA |  50000 | /firn/ns/demo/query |          | t
 0xa7490FFD6fFAF9C629a1E1Be4875E6b7700943DA |  20000 | /firn/ns/demo/query | tenant-a | t

SELECT path, caller, count(*), sum(amount_micro_usdc) ... GROUP BY 1,2:
 /firn/ns/demo/query | (anonymous) | 1 | 50000
 /firn/ns/demo/query | tenant-a    | 1 | 20000
```

Free routes produce no rows (nothing settles), and re-posting the same
receipt is a no-op.

## Review

Codex reviewed the diff. Applied: uniqueness is now `(network, tx_hash)`
(hashes are not globally unique across chains), CHECK constraints keep
zero/empty junk out, the gateway validates settlement fields before posting
(a null field would otherwise masquerade as a transient indexer error), and
the indexer logs ignored duplicates. Documented rather than changed:
`amount_micro_usdc`/`pay_to` record what the gateway charged — the same
in-process rules table that produced the verified payment requirements, but
not independent chain evidence, because the v2 SettleResponse carries no
amount (see the provenance comment in the migration); `/receipts` is
unauthenticated by design while it is compose-internal only, and needs a
shared secret before rung 3; settle-before-execution means a client can pay
for a request the origin then fails — the refund/retry policy is a rung-3
decision.

## What surprised me

- **The settlement extension is `None` unless you opt in.** x402-axum
  defaults to settling *after* the handler runs; the
  `Extension<Option<SettleResponse>>` the docs show is only `Some` under
  `settle_before_execution()`. The first indexer run silently indexed
  nothing — the client was green, Postgres was empty. Worth knowing: with
  the default ordering there is no clean place to observe the settlement
  server-side short of decoding your own response header.
- Settle-before-execution was the right call here anyway — Sluice's README
  had already promised "the origin never does unpaid work" — but it does
  mean a client can pay for a request the origin then 502s. That trade-off
  (who eats origin failures) deserves a deliberate decision before rung 3.
