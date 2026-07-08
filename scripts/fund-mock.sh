#!/bin/sh
# Fund the client with mock USDC on the offline (no-fork) anvil chain. The
# mock token's mint is open, so no impersonation dance — any unlocked anvil
# account can call it.
set -eu

RPC="${RPC:-http://anvil:8545}"
TOKEN="${TOKEN_ADDRESS:?TOKEN_ADDRESS is required}"
CLIENT="${CLIENT_ADDRESS:?CLIENT_ADDRESS is required}"
AMOUNT="${AMOUNT:-1000000000}" # 1000 mock USDC (6 decimals)
# anvil dev account #1; publicly known, fake value only.
FUNDER="${FUNDER:-0x70997970C51812dc3A010C7d01b50e0d17dc79C8}"

cast send "$TOKEN" "mint(address,uint256)" "$CLIENT" "$AMOUNT" \
    --from "$FUNDER" --unlocked --rpc-url "$RPC" > /dev/null

echo "client $CLIENT mock USDC balance: $(cast call "$TOKEN" "balanceOf(address)(uint256)" "$CLIENT" --rpc-url "$RPC")"
