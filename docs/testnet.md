# Running against Base Sepolia

The local anvil loop proves the system with no real money. The next step is
the same stack against Base Sepolia: drop anvil and point the facilitator at
a testnet RPC. Everything else stays as it is (a
`docker-compose.testnet.yml` override, to be written once the inputs below
exist).

## Inputs to gather first

1. **A Base Sepolia RPC URL.** The public `https://sepolia.base.org` works
   for smoke tests but rate-limits; an Alchemy/Infura/QuickNode free-tier
   endpoint is steadier. It goes to the facilitator's chain config and
   nowhere else; the gateway and client never talk to the chain.

2. **A funded facilitator wallet (Base Sepolia ETH for gas).** A fresh
   keypair, kept out of the repo; it broadcasts settlements and needs
   testnet ETH only, from a Base Sepolia faucet (the Coinbase developer
   faucet or the Alchemy faucet). Injected as `FACILITATOR_PRIVATE_KEY` via
   `.env` (gitignored).

3. **A funded client wallet (Base Sepolia USDC).** A second fresh keypair;
   it needs testnet USDC from Circle's faucet (https://faucet.circle.com,
   select Base Sepolia) and zero ETH, since payments are gasless for the
   payer. Injected as `CLIENT_PRIVATE_KEY`.

4. **A pay-to address.** Any address you control on Base Sepolia; it only
   receives USDC.

5. **The canonical Base Sepolia USDC contract address.** Confirm it from
   Circle's docs
   (https://developers.circle.com/stablecoins/usdc-contract-addresses)
   rather than trusting any cached value. As of writing Circle lists Base
   Sepolia USDC at `0x036CbD53842c5426634e7929541eC2318f3dCF7e`, but verify
   before use.

## What changes in the stack

- `docker-compose.testnet.yml`: removes `anvil` and `fund-usdc`, points the
  facilitator's chain config at `eip155:84532` (Base Sepolia) with the RPC
  URL, keys from `.env`.
- `config/rules.json`: unchanged. The gateway's price tags must switch from
  `USDC::base()` to `USDC::base_sepolia()`, so make the network an env var
  (`USDC_NETWORK=base|base-sepolia`) as part of this change.
- Real keys never enter the repo or an image: `.env` locally, a secrets
  manager in production.

## Local shortcuts to revisit before real money

- `x-sluice-caller` is an unauthenticated pricing hint; per-caller discounts
  need real identity first.
- The indexer's `/receipts` endpoint trusts the internal network; add a
  shared secret.
- Settle-before-execution means a client can pay for a request the origin
  then fails; decide the refund/retry policy.
- Grafana and Postgres credentials are compose-local defaults.
