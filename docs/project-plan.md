# Sluice payment example plan

Sluice is a public, self-hostable example of an end-to-end x402 payment project.
Downstream services can consume it as a versioned dependency and supply their own HTTP origins.

## 1. Public repository cleanup

- Keep the scope focused on the gateway, demo, and payment integration.
- Consolidate public development guidance in CONTRIBUTING.md.
- Keep instructions current and reliability claims consistent with tested behavior.
- Keep the README centered on a runnable payment example and its boundaries.

Acceptance: a reader can understand, run, and contribute to Sluice using the repository documentation.

## 2. Reproducible local payment proof

- Keep locked Rust dependency resolution and pin container dependencies.
- Provide a scripted offline demo with bounded readiness waits.
- Check unpaid quotes, paid responses, transaction receipts, receiver balance, and Postgres records.
- Run the same script in CI. Preserve a separate fork mode for real USDC bytecode.
- Document startup, expected output, diagnostics, shutdown, and reset behavior.

Acceptance: a fresh checkout completes the local payment proof without wallets, faucets, or external chain RPCs.
Image pulls and builds still require network access unless dependencies are cached.

## 3. Failure behavior and reusable boundaries

- Document free routes, caller discounts, denied paths, request limits, and streamed response limits.
- Exercise payment rejection, origin failures, and receipt delivery failures.
- Keep settlement before paid origin work and keep the origin pluggable.
- Define recovery expectations before claiming production-grade payment accounting.

Acceptance: tests and documentation distinguish payment settlement, origin response, and receipt persistence.

## 4. Public-network example

- Add a separate Base Sepolia configuration after checking current upstream facilitator and token support.
- Use dedicated test credentials and a bounded payment-aware client.
- Document hosted facilitator authentication or a private facilitator deployment.
- Record real testnet evidence; never infer it from the offline demo.

Acceptance: an external client completes a testnet request with a returned transaction and indexed receipt.
This phase needs testnet credentials and funded test wallets. It is not a prerequisite for the offline example.

## 5. Release as a reusable dependency

- Publish versioned application images after local and CI checks pass.
- Document a minimal HTTP origin contract and configuration needed by a downstream Compose stack.
- Record proxy overhead separately from chain settlement latency.
- Keep release notes specific about tested configurations and remaining limitations.

Acceptance: another repository can pin a Sluice release and supply its own origin without copying gateway code.

## Current implementation

The cleanup and local payment automation are implemented. Both local chain modes passed,
and the paid origin-failure behavior was exercised against the real local settlement path.
See [local verification](verification.md) for commands, results, and limitations.
The automated offline check is configured in CI; its first remote run remains pending.

Further failure coverage, receipt recovery design, the public-network example,
and release publication remain follow-up work. They are not completed by the local demo.

## Evidence

Record the checks actually run in pull requests or release notes.
A local payment demonstration does not establish mainnet readiness or durable accounting.
Keep public-network and publishing steps pending until their external inputs and release checks are complete.
