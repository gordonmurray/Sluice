# Paid-but-failed requests

The gateway settles payment before forwarding (`settle_before_execution`), so
paid routes reach the origin only after settlement. A client can therefore pay
for a request that fails with an origin error, timeout, or refused connection.

## The policy

No automatic retry, no automatic refund. The gateway attempts to record each
settlement in the payments table together with the HTTP status line the client received
(`origin_status`: the origin's own status, 502 when it was unreachable or
stalled past `ORIGIN_TIMEOUT_SECS` (default 30), 413 when the gateway refused
the request body). Refunds are an operator decision, made from the table:

```sql
SELECT tx_hash, payer, amount_micro_usdc, path, created_at
  FROM payments
 WHERE origin_status >= 400 OR origin_status IS NULL;
```

The query selects recorded failures for review, including rejected request bodies.
A 4xx response does not automatically imply a refund. Check the transaction,
network, asset, amount, and payer before an operator issues any refund.
A NULL status means the outcome is unknown or the row predates the status column.

## Why not retry or credit

- A blind origin retry turns every non-idempotent paid request (POST bodies,
  writes) into a double-execution risk the gateway cannot judge.
- Credits mean the gateway starts keeping balances, which is state it does
  not otherwise have and a scope boundary worth defending.
- Recording the outcome and refunding out of band keeps the gateway
  free of customer balances. Indexed rows connect a settlement to the
  recorded request outcome; missing rows still require investigation.

## Precision about what is recorded

`origin_status` is the response *status line*, recorded when origin headers
arrive; response bodies stream through the gateway unbuffered, so a 200 whose
body then truncates mid-stream is still recorded as 200. Origins that never
produce headers are bounded by `ORIGIN_TIMEOUT_SECS` (a connect/inter-read
timeout, not a whole-request cap) and recorded as 502.

The receipt is reported after the origin outcome is known. A gateway crash
mid-request can lose a receipt. Indexer delivery failures are logged without
retry, so an indexer outage can also lose records. The chain remains the source
of truth for settlement; it cannot reconstruct a missing origin outcome.
