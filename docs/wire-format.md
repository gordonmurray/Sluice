# Payment wire format

The demo client hides the wire format, so here it is in the raw. A request
without payment gets the quote:

```
$ curl -i -X POST localhost:8080/firn/ns/demo/query \
    -H 'content-type: application/json' \
    -d '{"text": "gasless payments without ETH", "k": 3}'

HTTP/1.1 402 Payment Required
payment-required: eyJ4NDAyVmVyc2lvbiI6MiwiZXJyb3IiOiJQYXltZW50LVNpZ25hdHVyZSBoZWFkZXIgaXMg…
content-length: 0
```

The `payment-required` header is base64. Decoded, it says exactly what to
sign: 50000 micro-USDC (six decimals, so $0.05) to the pay-to address, USDC
on Base, EIP-3009:

```json
{
  "x402Version": 2,
  "accepts": [{
    "scheme": "exact",
    "network": "eip155:8453",
    "amount": "50000",
    "payTo": "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
    "asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    "extra": { "assetTransferMethod": "eip3009", "name": "USD Coin", "version": "2" }
  }]
}
```

Paying is signing an EIP-712 `transferWithAuthorization` for that exact
amount and retrying with the result in a `Payment-Signature` header.
`scripts/pay-with-curl.sh` does the whole exchange with curl, `cast` (run
inside the anvil container) and python3, no x402 client library:

```
$ ./scripts/pay-with-curl.sh

HTTP/1.1 200 OK
content-type: application/json
payment-response: eyJuZXR3b3JrIjoiZWlwMTU1Ojg0NTMiLCJwYXllciI6IjB4YTc0OTBGRkQ2ZkZBRjlDNjI5…

{"results":[{"id":1,"score":0.25,"text":"x402 is an open protocol for HTTP-native payments: …
```

The `payment-response` header decodes to the settlement receipt, on-chain
before the origin did any work:

```json
{
  "network": "eip155:8453",
  "payer": "0xa7490FFD6fFAF9C629a1E1Be4875E6b7700943DA",
  "success": true,
  "transaction": "0xb796b89437aec331092839cdc967c7229d5f27d1cf9539dc7c6f8d8ce9a8aa7e"
}
```
