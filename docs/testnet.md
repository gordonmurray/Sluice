# Base Sepolia example: planned

The runnable example uses a local chain and fake funds. A public-network example
is a separate step; this repository does not yet include a testnet Compose file
or recorded testnet settlement evidence.

## Required inputs

- A Base Sepolia RPC endpoint.
- A dedicated facilitator wallet with testnet ETH for transaction gas, or a compatible hosted facilitator.
- A separate payer wallet with testnet USDC.
- A receiver address controlled by the operator.
- Current network, token address, and EIP-712 domain values from official documentation.

Keep credentials outside Git. Do not use the local demo keys on a public network.

## Implementation work

1. Add a testnet configuration that excludes Anvil and the local funding scripts.
2. Point the facilitator at the selected network and supply its credentials privately.
3. Configure the gateway asset and receiver for the same network.
4. Add a client with explicit network, asset, receiver, and maximum-price checks before signing.
5. Run one test payment from a separate machine.
6. Record the quote, final response, settlement transaction, receiver payment, and indexed receipt.

The gateway already supports `ASSET_ADDRESS`, `ASSET_CHAIN_ID`, `ASSET_NAME`,
`ASSET_VERSION`, and `ASSET_DECIMALS`. These override the default Base USDC asset
only when `ASSET_ADDRESS` is set. A chain-id override alone does not switch networks.
`PAY_TO` supplies the receiver. Do not add a second configuration mechanism without a demonstrated need.

The current URL-only facilitator setup does not establish support for hosted
facilitator authentication. Inspect the installed x402 client API and current
provider requirements before implementing that integration.

## Acceptance

An unpaid request returns the intended testnet requirements. A bounded client
payment settles on that network, returns useful origin data, and reaches the
receipt index. Check the transaction independently of the HTTP response.

Keep [paid-but-failed behavior](paid-but-failed.md) explicit. A public-network test
must not imply that receipt delivery is durable or that mainnet deployment is complete.
