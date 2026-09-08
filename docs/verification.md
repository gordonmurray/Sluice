# Local verification

These checks ran on 2026-09-08 with Docker 29.8.0 and the repository's pinned images.
The Docker development compiler was Rust 1.96.1. All settlement amounts were fake local value.

| Check | Result |
| --- | --- |
| Formatting | Passed through `scripts/cargo.sh fmt --all -- --check` |
| Clippy | Passed for the locked workspace and all targets with warnings denied |
| Workspace tests | 46 passed |
| ShellCheck | Passed for the smoke script and Cargo wrapper |
| Compose configuration | Offline and Firn overrides parsed successfully |
| Offline payment demo | Two successful transactions, receiver +70,000 micro-USDC, both receipts indexed |
| Base fork payment demo | Two successful transactions, receiver +70,000 micro-USDC, both receipts indexed |
| Paid origin failure | Successful settlement, HTTP 502, indexed `origin_status=502` |

The final offline run checked these local transaction hashes:

```text
50000 micro-USDC  0x8942b1d446fad6aa64296e98653decbf870bc9aa38ea45c2b5edd76f4cb0bef0
20000 micro-USDC  0x23377e916e2bbeef398d8391a1e83eac21d06cde15a529f3a48f3e98853d441a
```

The origin-failure check stopped the origin, paid through `scripts/pay-with-curl.sh`,
and checked the response header and corresponding Postgres record before restarting the origin.
Its local transaction was `0xf1a9d593743732c1c0dee93c6c8b2caf4f8162c1c18f6c5c564f349023c8ff01`.

These hashes are local test evidence, not public explorer links. Local chain state
disappears when the chain container is recreated. Re-run `scripts/smoke.sh` to
produce fresh evidence; its checks match current transaction hashes rather than historical row counts.

## Not established by these checks

- The Firn override was checked as configuration, not run against a Firn checkout.
- The new GitHub Actions job was not run remotely; its smoke command passed locally.
- No Base Sepolia or mainnet payment was made.
- Hosted facilitator authentication and versioned image publication remain planned.
- Receipt delivery remains best-effort; these successful deliveries do not establish crash-safe accounting.
