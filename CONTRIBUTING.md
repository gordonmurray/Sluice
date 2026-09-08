# Contributing

Sluice demonstrates paid HTTP requests with x402, USDC, and a pluggable origin.
See the [project plan](docs/project-plan.md) for scope and acceptance checks.

## Development

Use Docker Engine with Compose, or a Rust toolchain with a C linker.
The Docker wrapper uses the same Rust version as the application build:

```sh
scripts/cargo.sh fmt --all -- --check
scripts/cargo.sh clippy --workspace --all-targets --locked -- -D warnings
scripts/cargo.sh test --workspace --locked
```

Use `scripts/cargo.sh fmt --all` to format changes.
Run `scripts/smoke.sh` for the complete local payment check. It requires Bash and Python 3.
The first build and image pull need network access; the offline chain does not use an external RPC.

## Boundaries

Keep pricing, proxying, and origin logic separate. Add reusable gateway features without origin-specific branches.
Use upstream x402 crates for the payment protocol. The facilitator verifies and broadcasts payments.
Keep schema changes in migrations and monetary amounts in integer micro-USDC.
Keep library errors explicit. Do not use `unwrap` or `expect` in library code.

Preserve the built-in demo and optional Firn integration.
Use small tests for isolated behavior and real local settlement checks for the payment path.
Never substitute mocked settlement for evidence that the complete demo works.

## Changes and documentation

Describe the problem, resulting behavior, and checks in each pull request.
Update the changelog under `Unreleased` for user-visible changes.
Keep examples copyable and explain limitations alongside the relevant behavior.
Do not commit real credentials. Demo keys are public test values for local chains only.

Receipt delivery is best-effort. The indexer can miss a payment after a crash or outage.
An origin status records response headers, not successful delivery of its complete body.
Keep those limits explicit in documentation and release notes.

## Dependency updates

Compose dependencies and Docker base images use immutable digests.
When updating a dependency, record its upstream version and replacement digest in the pull request.
Run the offline payment check after facilitator, chain tooling, or x402 dependency changes.
Run fork mode as well when changing fork initialization or real-token handling.
Keep the application and development Dockerfiles on the same Rust base.
The CI Rust version must match the compiler supplied by that base image.
