# Gotchas

Real failures hit while building Sluice, one per section, each with the fix.
Kept as material for write-ups; new ones get added as they happen.

## Anvil's famous dev accounts are booby-trapped on mainnet forks

The classic anvil/hardhat dev accounts (`0xf39F...`, `0x7099...`, mnemonic
"test test ... junk") are the default identities for local EVM work. Fork
Base mainnet and use one to sign a USDC EIP-3009 payment authorisation, and
the token contract rejects it with `FiatTokenV2: invalid signature`, even
when you can prove byte-for-byte that the signature is correct for the
contract's own `DOMAIN_SEPARATOR()`.

The reason: those private keys are public, so on the real chain people have
installed EIP-7702 delegations on them (`cast code` shows `0xef0100...`, a
delegation designator, which is account code). USDC's `SignatureChecker`
sees code at the signer address, takes the ERC-1271 contract-wallet path
instead of `ecrecover`, and calls `isValidSignature(bytes32,bytes)` on the
squatter's delegate. Unless that contract returns the ERC-1271 magic value
`0x1626ba7e` for your digest, the check fails and USDC reverts.

Your fork inherits that state. The dev account still works as an EOA for
sending transactions, but it is not code-empty any more, and code-emptiness
is the property signature checkers actually test.

Fix: generate a fresh keypair for anything that signs EIP-712/EIP-3009
payloads on a mainnet fork. Dev accounts are still fine as gas payers
sending ordinary transactions, or as plain recipients on an ERC-20 ledger.

The debugging path that found it: query `DOMAIN_SEPARATOR()` on the fork,
recompute it manually (matched), recompute the full EIP-712 digest and
re-sign with the same key via `cast wallet sign --no-hash` (byte-identical
signature, so the client was right), simulate `transferWithAuthorization`
directly (still reverts), which leaves the account itself as the only
remaining variable. `cast code` on the signer. `0xef0100...`. Oh.

## A forked anvil's clock stops

`anvil --fork-url` pins chain time to the fork block's timestamp, and it
does not track wall clock; it only advances when a block mines (by default,
when a transaction arrives). EIP-3009 authorisations carry
`validAfter`/`validBefore` derived from the client's wall clock, so a few
minutes after starting the fork, every payment fails verification with
`FiatTokenV2: authorization is not yet valid`. The chain thinks it is still
"then" and the client signs for "now".

Fix: `anvil --fork-url ... --block-time 5`. Mining empty blocks keeps chain
time tracking wall time. One flag, and time-window signature schemes work.

## Two crates, same name, different types

The x402-reqwest crate extends `reqwest::Client` with `.with_payments(...)`.
Compile error: "no method named `with_payments` found for struct `Client`",
with the trait demonstrably in scope. Cause: the workspace pinned reqwest
0.12 while x402-reqwest 2.0.1 is built against reqwest 0.13, so there were
two `reqwest::Client` types in the dependency graph and the extension trait
was implemented for the other one. Rust never says "wrong crate version of
this type"; it says the method does not exist.

Related: reqwest 0.13 renamed the `rustls-tls` feature to `rustls`, so the
version bump also breaks feature flags.

## The README documents a config format the released image does not parse

The x402 facilitator README (main branch) shows
`"schemes": [{"scheme": "...", "chains": ["eip155:8453"]}]`. The released
2.0.1 docker image wants `{"id": "...", "chains": "eip155:8453"}`, a
different key and a string pattern instead of an array. `docker inspect`
the image for its `org.opencontainers.image.revision` label and read the
config structs at that exact git revision instead of trusting the README.

## Features that are not optional

`x402-chain-eip155` 2.0.1 fails to compile without its `telemetry` feature
because code outside the feature gate references `tracing`. If the upstream
examples all enable a feature, treat it as required regardless of what the
manifest implies.

## Prefix-priced routes plus URL normalisation is a traversal hole

The gateway priced routes by path prefix and matched the raw path, but URL
parsers downstream normalise dot segments. So
`POST /firn/ns/demo/query/../upsert` would have been priced as a paid query
and delivered to the origin as an admin write. The gateway now rejects paths
containing dot segments (plain or percent-encoded) or empty segments before
pricing. Codex spotted this one in review; `curl --path-as-is` confirmed it,
since well-behaved HTTP clients normalise the path before sending and never
even show you the problem.

## Quick ones

- **The settlement extension is `None` unless you opt in.** x402-axum
  settles after the handler by default; the
  `Extension<Option<SettleResponse>>` in its docs is only `Some` under
  `settle_before_execution()`. My first indexer run was green end to end
  and indexed nothing.
- **The facilitator exports OTLP over HTTP, not gRPC.** Pointing it at the
  collector's 4317 port produces only `BatchSpanProcessor.ExportError:
  network error`. Use 4318.
- **crates.io returns an error for requests without a User-Agent.** Set
  `-A "yourproject (contact)"` on curl or you get a policy-violation JSON.
- **Codex CLI cannot sandbox on hosts without unprivileged user
  namespaces** (`bwrap: loopback: Failed RTM_NEWADDR: Operation not
  permitted`). The fallback works fine: pipe the diff into `codex exec` via
  stdin so it never needs filesystem access.
- **A host can have cargo but no linker.** rustup installs fine without a C
  toolchain; every build then dies at link time. A 5-line wrapper running
  cargo in the official rust image (registry cache in a named volume,
  target/ on the bind mount) keeps builds incremental and the host clean.
