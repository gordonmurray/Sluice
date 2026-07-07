# Milestone 3 — Firn flagship demo: pay-per-query search

## What was built

The paid route now fronts a real search engine. Firn's `demo` namespace is
seeded with text documents and a BM25 full-text index; the gateway prices
`POST /firn/ns/demo/query` at 0.05 USDC (0.02 for `tenant-a`) while health
and metrics are free and everything else — including all admin/write routes —
is denied.

- `scripts/seed-firn.sh` + `config/seed.json`: one-shot compose service that
  waits for Firn, upserts 8 text rows (with small placeholder vectors — Firn
  requires a vector payload per row; BM25 carries the demo), then builds the
  FTS index and polls the async operation to completion.
- `config/rules.json`: the M3 pricing table. Only the query route costs
  money; seeding happens direct-to-Firn inside the compose network, not
  through the gateway.
- Client: pays for a search, prints the ranked hits, proves the per-caller
  price, and confirms `upsert` 404s through the gateway.

No gateway code changed. Monetizing a new origin endpoint was a rules-table
edit — which is the point of the design.

## How to run

```sh
docker compose up -d --build   # seed-firn runs automatically
docker compose run --rm client
```

## Verified output

```
GET  /healthz                              -> 200 (free)
GET  /firn/health                          -> 200 (free, proxied)
POST /firn/ns/demo/query (no payment)      -> 402, amount=50000
POST /firn/ns/demo/query (x402 base price) -> 200:
   hit id=2 score=1.710  EIP-3009 transferWithAuthorization lets a USDC holder…
   hit id=1 score=1.448  x402 is an open protocol for HTTP-native payments…
   hit id=4 score=0.947  Sluice is a pay-per-request gateway…
POST /firn/ns/demo/query (x402 tenant-a)   -> 200, settled
POST /firn/ns/demo/upsert                  -> 404 (admin not exposed)
pay-to balance delta: exactly 70000 micro-USDC (0.05 + 0.02)
```

The BM25 ranking is real: "gasless payments without ETH" ranks the EIP-3009
document first.

## Review

Codex's headline finding was real: prefix-priced routes + downstream URL
normalization meant `POST /firn/ns/demo/query/../upsert` would have been
priced as a query and delivered to Firn as an admin write. The gateway now
rejects paths with dot segments (plain or percent-encoded) or empty segments
— before pricing, so nobody pays for a request that will be refused. Verified
with `curl --path-as-is` (400); reqwest-based clients normalize client-side
and never even send it. Also applied: the seed script tolerates reruns and
ends with a probe query as its acceptance test, operation-id parsing can't
poll garbage, and the client's hit printer no longer slices mid-UTF-8.

## What surprised me

- **Firn rejects text-only rows.** Every row needs a vector payload (the
  first row fixes the namespace's kind and dimension), even for a pure FTS
  workload. Small 4-dim placeholder vectors satisfy the schema.
- **FTS is a build step, not a default.** Queries with `text` fail with
  "Cannot perform full text search unless an INVERTED index has been created"
  until you `POST /ns/{ns}/fts-index` and poll the async operation.
- **reqwest-middleware's builder is not reqwest's.** x402-reqwest returns a
  `reqwest_middleware` client whose `RequestBuilder` lacks `.json()` unless
  you enable an extra feature on a crate we don't otherwise depend on;
  setting the content-type and body by hand avoids the dependency.
- A stale detail from M2: mounted config is read at startup, so editing
  `rules.json` needs a container restart (`--force-recreate` after a rules
  change, or the old table keeps pricing).
