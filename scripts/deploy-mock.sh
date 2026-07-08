#!/bin/sh
# Deploy the pre-compiled mock EIP-3009 token (contracts/MockUSDC.bin) on the
# offline anvil chain. The deployer must be at nonce 0 so the contract lands
# at the deterministic address the rest of the stack is configured with;
# idempotent across container restarts (skips if code is already there).
set -eu

RPC="${RPC:-http://anvil:8545}"
DEPLOYER_KEY="${DEPLOYER_KEY:?DEPLOYER_KEY is required}"
EXPECTED="${TOKEN_ADDRESS:?TOKEN_ADDRESS is required}"

if [ "$(cast code "$EXPECTED" --rpc-url "$RPC")" != "0x" ]; then
    echo "mock token already deployed at $EXPECTED"
else
    # Flags must precede --create: cast treats everything after the bytecode
    # as constructor arguments.
    cast send --private-key "$DEPLOYER_KEY" --rpc-url "$RPC" \
        --create "0x$(cat /MockUSDC.bin)" > /dev/null

    if [ "$(cast code "$EXPECTED" --rpc-url "$RPC")" = "0x" ]; then
        echo "deploy did not land at $EXPECTED — deployer nonce was not 0?" >&2
        exit 1
    fi
    echo "mock EIP-3009 token deployed at $EXPECTED"
fi

# The facilitator's startup checks require infrastructure contracts that
# exist on every real chain but not on a fresh one. Install their runtime
# bytecode (snapshotted from Base mainnet into contracts/) at the canonical
# addresses via anvil cheatcode. These run on every invocation — setCode is
# idempotent, and skipping them because the token happens to exist would
# leave a partially-initialised chain permanently broken:
#   - x402's universal signature validator (EIP-6492/1271/EOA)
#   - Permit2
#   - x402's gas-sponsoring contract
install() {
    cast rpc anvil_setCode "$1" "0x$(cat "$2")" --rpc-url "$RPC" > /dev/null
    echo "installed $2 at $1"
}
install 0xdAcD51A54883eb67D95FAEb2BBfdC4a9a6BD2a3B /SignatureValidator.bin
install 0x000000000022D473030F116dDEE9F6B43aC78BA3 /Permit2.bin
install 0x402085c248EeA27D92E8b30b2C58ed07f9E20001 /X402Sponsor.bin
