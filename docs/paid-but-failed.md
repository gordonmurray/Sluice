# Paid-but-failed requests

The gateway settles payment before forwarding (`settle_before_execution`), so
the origin never does unpaid work and every settlement is observable. The
trade-off: a client can pay for a request the origin then fails — a 5xx, a
timeout, an unreachable origin.

## The policy

No automatic retry, no automatic refund. Every settlement is recorded in the
payments table together with the HTTP status line the client received
(`origin_status`: the origin's own status, 502 when it was unreachable or
stalled past `ORIGIN_TIMEOUT_SECS` (default 30), 413 when the gateway refused
the request body). Refunds are an operator decision, made from the table:

```sql
SELECT tx_hash, payer, amount_micro_usdc, path, created_at
  FROM payments
 WHERE origin_status >= 500 OR origin_status IS NULL;
```

`tx_hash` and `payer` are exactly what an out-of-band refund needs. Rows with
a NULL `origin_status` predate the column (or the gateway could not determine
the outcome) and deserve the benefit of the doubt.

## Why not retry or credit

- A blind origin retry turns every non-idempotent paid request (POST bodies,
  writes) into a double-execution risk the gateway cannot judge.
- Credits mean the gateway starts keeping balances, which is state it does
  not otherwise have and a scope boundary worth defending (see CLAUDE.md).
- Recording the outcome and refunding out of band keeps the gateway
  stateless and the money trail auditable: the chain has the settlement, the
  table has what it bought.

## Precision about what is recorded

`origin_status` is the response *status line*, recorded when origin headers
arrive; response bodies stream through the gateway unbuffered, so a 200 whose
body then truncates mid-stream is still recorded as 200. Origins that never
produce headers are bounded by `ORIGIN_TIMEOUT_SECS` (a connect/inter-read
timeout, not a whole-request cap) and recorded as 502.

The receipt is reported after the origin outcome is known. A gateway crash
mid-request can lose a receipt (fire-and-forget always could); the chain
remains the source of truth for settlements, this table for what they bought.
