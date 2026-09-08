#!/usr/bin/env bash
# Check the complete local payment path. Leaves containers running for inspection.
set -euo pipefail
cd "$(dirname "$0")/.."

mode="${1:-offline}"
compose=(docker compose -f docker-compose.yml)
case "$mode" in
    offline)
        compose+=(-f docker-compose.offline.yml)
        asset=0x057ef64E23666F000b34aE31332854aCBd1c8544
        ;;
    fork) asset=0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913 ;;
    *) echo "Usage: $0 [offline|fork]" >&2; exit 2 ;;
esac
receiver=0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'echo "Payment check failed. Inspect: docker compose logs --tail=50" >&2' ERR

"${compose[@]}" up -d --build
for attempt in {1..60}; do
    if "${compose[@]}" exec -T --interactive=false gateway sh -c \
        'curl -fsS --max-time 2 http://localhost:8080/healthz && curl -fsS --max-time 2 http://facilitator:8080/supported && curl -fsS --max-time 2 http://indexer:8090/healthz' >/dev/null 2>&1; then
        break
    fi
    if [[ "$attempt" == 60 ]]; then
        echo "Gateway, facilitator, or indexer did not become ready within the readiness window" >&2
        exit 1
    fi
    sleep 2
done
balance() {
    "${compose[@]}" exec -T --interactive=false anvil cast call "$asset" 'balanceOf(address)(uint256)' "$receiver" \
        --rpc-url http://localhost:8545 | awk '{print $1}'
}
before=$(balance)

# Bound the client run, preserving its output and exit status.
python3 - "$work/client.log" "${compose[@]}" run --build --rm client <<'PY'
import pathlib
import subprocess
import sys
with pathlib.Path(sys.argv[1]).open('w') as output:
    try:
        result = subprocess.run(sys.argv[2:], stdout=output, stderr=subprocess.STDOUT, timeout=240)
    except subprocess.TimeoutExpired:
        print(pathlib.Path(sys.argv[1]).read_text())
        raise SystemExit('Client exceeded 240 seconds; inspect container logs')
print(pathlib.Path(sys.argv[1]).read_text())
raise SystemExit(result.returncode)
PY

after=$(balance)
python3 - "$work/client.log" "$work/receipts.tsv" "$before" "$after" <<'PY'
import base64
import json
import pathlib
import re
import sys
log = pathlib.Path(sys.argv[1]).read_text()
encoded = re.findall(r'payment-response \(base64\): (\S+)', log)
assert len(encoded) == 2, f'Expected two payment receipts, got {len(encoded)}'
receipts = [json.loads(base64.b64decode(value, validate=True)) for value in encoded]
assert int(sys.argv[4]) - int(sys.argv[3]) == 70000, 'Receiver did not gain exactly 70000 micro-USDC'
transactions = set()
with pathlib.Path(sys.argv[2]).open('w') as output:
    for receipt, amount in zip(receipts, (50000, 20000)):
        assert receipt['success'] is True, receipt
        assert receipt['network'] == 'eip155:8453', receipt
        tx = receipt['transaction']
        assert re.fullmatch(r'0x[0-9a-fA-F]{64}', tx), tx
        assert receipt['payer'].lower() == '0xa7490ffd6ffaf9c629a1e1be4875e6b7700943da', receipt
        transactions.add(tx)
        output.write(f'{tx}\t{amount}\n')
assert len(transactions) == 2, 'Payments must have distinct transactions'
PY

checked=0
while IFS=$'\t' read -r tx amount; do
    "${compose[@]}" exec -T --interactive=false anvil cast receipt "$tx" --json --rpc-url http://localhost:8545 > "$work/chain.json"
    python3 - "$work/chain.json" <<'PY'
import json
import sys
with open(sys.argv[1]) as source:
    receipt = json.load(source)
assert receipt['status'] in ('0x1', '0x01', 1), receipt
PY
    # Delivery is asynchronous. Match these transactions, not a historical row count.
    for attempt in {1..30}; do
        rows=$("${compose[@]}" exec -T --interactive=false postgres psql -U sluice -d sluice -Atc \
            "SELECT count(*) FROM payments WHERE network = 'eip155:8453' AND tx_hash = '$tx' AND amount_micro_usdc = $amount AND path = '/firn/ns/demo/query' AND origin_status = 200 AND success AND lower(pay_to) = lower('$receiver') AND lower(payer) = '0xa7490ffd6ffaf9c629a1e1be4875e6b7700943da';")
        [[ "$rows" == 1 ]] && break
        if [[ "$attempt" == 30 ]]; then
            echo "Receipt missing or incorrect in Postgres: $tx" >&2
            exit 1
        fi
        sleep 1
    done
    checked=$((checked + 1))
    echo "Settled and indexed: $tx ($amount micro-USDC)"
done < "$work/receipts.tsv"
[[ "$checked" == 2 ]]
echo "Payment check passed: two successful transactions, two indexed receipts, receiver +70000 micro-USDC."
echo "Containers remain running. Stop with: ${compose[*]} down"
