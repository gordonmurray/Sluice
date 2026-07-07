#!/bin/sh
# Fund the client account with USDC on the anvil fork of Base mainnet.
# Uses anvil cheatcodes to impersonate the real USDC masterMinter and mint
# test funds. Rung 1 only — fake value, publicly known dev accounts.
set -eu

RPC="${RPC:-http://anvil:8545}"
# Canonical USDC on Base mainnet; valid here because anvil forks Base state.
USDC="${USDC:-0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913}"
CLIENT="${CLIENT_ADDRESS:?CLIENT_ADDRESS is required}"
AMOUNT="${AMOUNT:-1000000000}" # 1000 USDC (6 decimals)

MASTER=$(cast call "$USDC" "masterMinter()(address)" --rpc-url "$RPC")
echo "USDC masterMinter: $MASTER"

# 1 ETH for gas, then impersonate and mint.
cast rpc anvil_setBalance "$MASTER" 0xDE0B6B3A7640000 --rpc-url "$RPC" > /dev/null
cast rpc anvil_impersonateAccount "$MASTER" --rpc-url "$RPC" > /dev/null
cast send "$USDC" "configureMinter(address,uint256)" "$MASTER" "$AMOUNT" \
    --from "$MASTER" --unlocked --rpc-url "$RPC" > /dev/null
cast send "$USDC" "mint(address,uint256)" "$CLIENT" "$AMOUNT" \
    --from "$MASTER" --unlocked --rpc-url "$RPC" > /dev/null
cast rpc anvil_stopImpersonatingAccount "$MASTER" --rpc-url "$RPC" > /dev/null

echo "client $CLIENT USDC balance: $(cast call "$USDC" "balanceOf(address)(uint256)" "$CLIENT" --rpc-url "$RPC")"
