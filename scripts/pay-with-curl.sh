#!/usr/bin/env bash
# Pay for one gateway request with nothing but curl, cast, and python3.
# Shows the x402 wire format without the client library: fetch the 402
# requirements, sign an EIP-3009 transferWithAuthorization (EIP-712,
# off-chain, no gas), retry with the Payment-Signature header.
# Rung 1 only — signs with the demo client key from docker-compose.yml
# and uses cast inside the anvil container, so nothing needs installing.
set -eu

GATEWAY="${GATEWAY:-http://localhost:8080}"
PATH_PAID="${PATH_PAID:-/firn/ns/demo/query}"
PK="${CLIENT_PRIVATE_KEY:-0x69935b0e457ed4de0c989163f1c1aa7cf899de6cff42b1829a42f4fa6a607da3}"
BODY='{"text": "gasless payments without ETH", "k": 3, "include_vector": false}'
sign() { docker exec -i sluice-anvil cast "$@"; }

# 1. Ask without paying: 402 plus base64 payment requirements.
REQS=$(curl -s -D - -o /dev/null -X POST "$GATEWAY$PATH_PAID" \
    -H 'content-type: application/json' -d "$BODY" \
    | tr -d '\r' | awk 'tolower($1)=="payment-required:" {print $2}')
DECODED=$(printf '%s' "$REQS" | base64 -d)
echo "402 requirements:"
printf '%s\n' "$DECODED" | python3 -m json.tool

# 2. Sign the transfer the requirements ask for.
FROM=$(sign wallet address --private-key "$PK")
NONCE=0x$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
NOW=$(date +%s)
TYPED=$(printf '%s' "$DECODED" | python3 -c "
import json, sys
req = json.load(sys.stdin)['accepts'][0]
print(json.dumps({
    'types': {
        'EIP712Domain': [
            {'name': 'name', 'type': 'string'},
            {'name': 'version', 'type': 'string'},
            {'name': 'chainId', 'type': 'uint256'},
            {'name': 'verifyingContract', 'type': 'address'}],
        'TransferWithAuthorization': [
            {'name': 'from', 'type': 'address'},
            {'name': 'to', 'type': 'address'},
            {'name': 'value', 'type': 'uint256'},
            {'name': 'validAfter', 'type': 'uint256'},
            {'name': 'validBefore', 'type': 'uint256'},
            {'name': 'nonce', 'type': 'bytes32'}]},
    'primaryType': 'TransferWithAuthorization',
    'domain': {'name': req['extra']['name'], 'version': req['extra']['version'],
               'chainId': int(req['network'].split(':')[1]),
               'verifyingContract': req['asset']},
    'message': {'from': '$FROM', 'to': req['payTo'], 'value': req['amount'],
                'validAfter': $NOW - 60, 'validBefore': $NOW + 600,
                'nonce': '$NONCE'}}))
")
SIG=$(printf '%s' "$TYPED" | sign wallet sign --data --from-file /dev/stdin --private-key "$PK")

# 3. Wrap signature + authorization in the x402 envelope and retry.
PAYMENT=$(printf '%s' "$DECODED" | python3 -c "
import base64, json, sys
reqs = json.load(sys.stdin)
typed = json.loads('''$TYPED''')['message']
env = {
    'x402Version': 2,
    'accepted': reqs['accepts'][0],
    'payload': {'signature': '$SIG', 'authorization': {
        'from': typed['from'].lower(), 'to': typed['to'].lower(),
        'value': typed['value'], 'validAfter': str(typed['validAfter']),
        'validBefore': str(typed['validBefore']), 'nonce': typed['nonce']}},
    'resource': {'url': '$GATEWAY$PATH_PAID'},
}
print(base64.b64encode(json.dumps(env).encode()).decode())
")
echo
echo "paid retry:"
curl -is -X POST "$GATEWAY$PATH_PAID" \
    -H 'content-type: application/json' \
    -H "Payment-Signature: $PAYMENT" \
    -d "$BODY"
echo
