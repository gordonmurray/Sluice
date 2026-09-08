# Authenticated facilitator and durable GET delivery

The gateway supports Coinbase CDP request authentication and an optional SQLite journal for small paid GET responses.
These options do not change the default local demo. They do not establish complete production accounting or backup recovery.

## Coinbase authentication

Set `FACILITATOR_URL=https://api.cdp.coinbase.com/platform/v2/x402` and `CDP_CREDENTIALS_PATH` to a protected JSON file with `api_key_id` and `api_key_secret`.
The current adapter accepts CDP Ed25519 secrets only. It validates the key pair, signs a fresh request-bound JWT for each call, and disables redirects.
Tokens expire after 120 seconds. Credentials, bearer tokens, payment signatures and provider response bodies are excluded from adapter errors.
Restart the gateway after rotating the credential file.
Set `PUBLIC_BASE_URL` to the externally visible HTTPS origin so x402 resources have the correct URL behind a load balancer.

## Durable response mode

Set `PAYMENT_JOURNAL_PATH` to a file on protected persistent storage and set `PUBLIC_BASE_URL`.
The journal uses SQLite WAL with full synchronous commits. Its main database is capped at approximately 256 MiB; full or unavailable storage fails closed.
Monitor capacity and back it up using SQLite's backup API. Do not copy only the main file while WAL writes are active.

This mode supports paid GET requests without query strings or request bodies, and responses of at most 64 KiB.
The origin must implement pure read-only GETs: the gateway fetches and buffers the result before charging.
Non-200 origin responses are returned without settlement. The product origin owns validation, freshness, conflicts, rate limits and its error contract.
A successful prepared response is persisted only after facilitator verification succeeds, before settlement is attempted.
The journal binds a payment payload to the requested URL and makes authorization nonce reuse unique across requests.
It stores the response, nonce and public payment terms; it does not store the payment signature.

Settlement intent is committed before the external call. A completed settlement is committed before returning the prepared response.
An identical request and payment payload retrieves the same response and transaction after a restart without another settlement.
Concurrent requests can claim an authorization only once. A response from a newer origin snapshot does not replace a saved paid result.
Payment headers are stripped before origin forwarding.

A process crash or provider timeout during settlement leaves an unknown outcome. The gateway returns 503 and refuses to settle that attempt again.
The client must retain the original payment payload securely and must not sign a replacement automatically.
The operator can inspect `authorization_details` and reconcile the on-chain nonce and transfer.
`scripts/reconcile-payments.py` checks Base and Base Sepolia USDC transfers without signing keys. It defaults to read-only; `--write` stores only successful transfers supported by matching authorization and transfer logs.
Older attempts, unsupported assets, canceled nonces and inconclusive results remain blocked for manual investigation.
There is no automatic refund or automatic resubmission.

The journal proves settlement state and the availability of a stored response, not that the client received every byte.
The legacy indexer notification remains best-effort; the journal is the durable source in this mode.
Off-host backups, restore drills, operational reconciliation and client spending policy remain deployment responsibilities.

## Validation

Unit and HTTP-stack tests cover request-bound JWT signatures, destination restrictions, response retention across restart, concurrent replay, known error responses and ambiguous provider failures.
A Base Sepolia integration against CDP completed a real test-token settlement and verified the receiver balance. This is testnet evidence, not mainnet readiness.

The offline payment smoke still passed with two indexed receipts and the expected receiver balance delta.
A recovery drill copied the testnet journal, removed its recorded outcome, and reconstructed the successful transfer from chain logs without a new payment.
