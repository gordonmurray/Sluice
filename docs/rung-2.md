# Rung 2 — Base Sepolia: what is needed to proceed

Rung 1 (offline anvil fork) is complete: milestones 1–5 verified. Rung 2
drops anvil and points the facilitator at Base Sepolia. Everything else
stays as-is (`docker-compose.testnet.yml` override, to be written when the
inputs below exist).

## Inputs Gordon must provide

1. **A Base Sepolia RPC URL.** The public `https://sepolia.base.org` works
   for smoke tests but rate-limits; an Alchemy/Infura/QuickNode free-tier
   endpoint is steadier. Goes to the facilitator's chain config (and nothing
   else — the gateway and client never talk to the chain).

2. **A funded facilitator wallet (Base Sepolia ETH for gas).** A fresh
   keypair you generate and keep out of the repo; it broadcasts settlements
   and needs testnet ETH only — get it from a Base Sepolia faucet (Coinbase
   developer faucet or Alchemy faucet). Injected as `FACILITATOR_PRIVATE_KEY`
   env, via `.env` (gitignored).

3. **A funded client wallet (Base Sepolia USDC).** A second fresh keypair;
   needs testnet USDC from Circle's faucet (https://faucet.circle.com,
   select Base Sepolia) and zero ETH (payments are gasless for the payer).
   Injected as `CLIENT_PRIVATE_KEY` env.

4. **A pay-to address.** Any address you control on Base Sepolia — can be
   the address of a third fresh keypair; it only receives USDC.

5. **The canonical Base Sepolia USDC contract address.** Confirm it from
   Circle's docs (https://developers.circle.com/stablecoins/usdc-contract-addresses)
   rather than trusting any cached value — CLAUDE.md's rule. As of writing
   Circle lists Base Sepolia USDC at `0x036CbD53842c5426634e7929541eC2318f3dCF7e`,
   but verify before use.

## What changes in the stack

- `docker-compose.testnet.yml`: removes `anvil` + `fund-usdc`, points the
  facilitator's chain config at `eip155:84532` (Base Sepolia) with the RPC
  URL, keys from `.env`.
- `config/rules.json`: unchanged. Gateway price tags must switch from
  `USDC::base()` to `USDC::base_sepolia()` — make the network an env var
  (`USDC_NETWORK=base|base-sepolia`) as part of the rung-2 change.
- Real keys never enter the repo or an image: `.env` locally, Secrets
  Manager at rung 3.

## Known rung-1 shortcuts to revisit before real money (rung 3)

- `x-sluice-caller` is an unauthenticated pricing hint; per-caller discounts
  need real identity first.
- The indexer's `/receipts` endpoint trusts the internal network; add a
  shared secret.
- Settle-before-execution means a client can pay for a request the origin
  then fails; decide the refund/retry policy.
- Grafana/Postgres credentials are compose-local defaults.
