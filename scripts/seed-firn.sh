#!/bin/sh
# One-shot: seed the demo namespace in Firn with text rows for full-text
# search. Idempotent — upsert by stable row id. Waits for Firn to come up
# (its image has no shell/curl for a compose healthcheck).
set -eu

FIRN="${FIRN_URL:-http://firnflow:3000}"
NS="${NAMESPACE:-demo}"

i=0
until curl -sf "$FIRN/health" > /dev/null; do
    i=$((i + 1))
    [ "$i" -ge 60 ] && { echo "firn did not come up"; exit 1; }
    sleep 2
done

curl -sf -X POST "$FIRN/ns/$NS/upsert" \
    -H 'content-type: application/json' \
    --data-binary @/seed.json
echo

# Full-text search needs a BM25 inverted index on the text column.
# The build is async: 202 + operation id, then poll. On reruns the create
# call may refuse (index exists) — fall through to the probe query, which
# is the actual acceptance test either way.
RESP=$(curl -s -X POST "$FIRN/ns/$NS/fts-index" || true)
OP=$(printf '%s' "$RESP" | sed -n 's/.*"operation_id":"\([^"{}]*\)".*/\1/p')
if [ -n "$OP" ]; then
    i=0
    while :; do
        STATUS=$(curl -s "$FIRN/operations/$OP" | sed -n 's/.*"status":"\([^"{}]*\)".*/\1/p')
        case "$STATUS" in
            succeeded) break ;;
            failed) echo "fts index build failed"; exit 1 ;;
        esac
        i=$((i + 1))
        [ "$i" -ge 60 ] && { echo "fts index build timed out"; exit 1; }
        sleep 2
    done
else
    echo "fts-index returned no operation id (already built?): $RESP"
fi

# Acceptance probe: a text query must return results.
PROBE=$(curl -sf -X POST "$FIRN/ns/$NS/query" \
    -H 'content-type: application/json' \
    -d '{"text": "payments", "k": 1, "include_vector": false}')
case "$PROBE" in
    *'"id"'*) echo "seeded namespace $NS (fts probe ok)" ;;
    *) echo "fts probe returned no hits: $PROBE"; exit 1 ;;
esac
