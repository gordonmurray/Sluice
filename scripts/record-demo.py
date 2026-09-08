#!/usr/bin/env python3
"""Present live requests to the offline demo for an asciinema recording."""
import base64
import json
import os
from pathlib import Path
import re
import subprocess
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
os.chdir(ROOT)
TOKEN = '0x057ef64E23666F000b34aE31332854aCBd1c8544'
RECEIVER = '0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC'
URL = 'http://localhost:8080'
PATH = '/firn/ns/demo/query'


def command(*args):
    return subprocess.check_output(args, text=True, timeout=60).strip()


def balance():
    return int(command('docker', 'exec', 'sluice-anvil', 'cast', 'call', TOKEN,
                       'balanceOf(address)(uint256)', RECEIVER,
                       '--rpc-url', 'http://localhost:8545').split()[0])


def page(title):
    print('\033[2J\033[H\033[1;36mSLUICE  |  HTTP requests with x402 payments\033[0m')
    print('Local Anvil chain / mock USDC / fake funds\n')
    print(f'\033[1;33m{title}\033[0m\n', flush=True)
    time.sleep(2)


def fetch(path, body=None):
    request = urllib.request.Request(URL + path, data=body,
                                     headers={'Content-Type': 'application/json'})
    try:
        response = urllib.request.urlopen(request, timeout=10)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.status, response.headers, response.read()


def show(value):
    print(json.dumps(value, indent=2), flush=True)


def main():
    page('1 / 4   A free health check')
    print('GET /healthz', flush=True)
    status, _, body = fetch('/healthz')
    assert status == 200, status
    print(f'HTTP {status}\n{body.decode()}\n', flush=True)
    print('The gateway is running. No payment is required.', flush=True)
    time.sleep(7)

    page('2 / 4   Ask for a search without paying')
    print(f'POST {PATH}', flush=True)
    body = json.dumps({'text': 'gasless payments without ETH', 'k': 3}).encode()
    status, headers, _ = fetch(PATH, body)
    assert status == 402, status
    requirements = json.loads(base64.b64decode(headers['payment-required']))
    accepted = requirements['accepts'][0]
    assert accepted['asset'].lower() == TOKEN.lower(), 'Use the offline mock-token stack'
    assert accepted['amount'] == '50000', 'Restore the demo pricing configuration'
    print(f'HTTP {status} Payment Required\n\nDecoded payment-required header:', flush=True)
    show({'scheme': accepted['scheme'], 'network': accepted['network'],
          'amount': accepted['amount'], 'asset': accepted['asset'], 'payTo': accepted['payTo']})
    print('\n50,000 micro-USDC = 0.05 USDC in token units. Local fake value.', flush=True)
    time.sleep(10)

    page('3 / 4   Sign, settle, and retry')
    print('$ ./scripts/pay-with-curl.sh\n', flush=True)
    print('Requesting a quote and signing with the public local demo key.\nWaiting for local settlement...', flush=True)
    before = balance()
    env = os.environ.copy()
    env.pop('CLIENT_PRIVATE_KEY', None)
    env.update(GATEWAY=URL, PATH_PAID=PATH)
    output = subprocess.check_output(['scripts/pay-with-curl.sh'], text=True,
                                     env=env, timeout=90)
    match = re.search(r'HTTP/\S+ 200[^\n]*\n(.*)', output, re.S)
    assert match, 'Paid request did not return HTTP 200'
    receipt_header = re.search(r'^payment-response:\s*(\S+)', match[1], re.M | re.I)
    assert receipt_header, 'Paid response omitted the settlement receipt'
    receipt = json.loads(base64.b64decode(receipt_header[1]))
    assert receipt['success'] is True, receipt
    tx = receipt['transaction']
    assert re.fullmatch(r'0x[0-9a-fA-F]{64}', tx), tx
    response_body = json.loads(match[1].split('\n\n', 1)[1])
    assert response_body['results'], 'Search returned no results'
    print('\nHTTP 200 OK\n\nFirst search result:', flush=True)
    show(response_body['results'][0])
    print('\nPayment settled before the origin handled the request.', flush=True)
    time.sleep(9)

    page('4 / 4   Check the payment and indexed receipt')
    chain = json.loads(command('docker', 'exec', 'sluice-anvil', 'cast', 'receipt',
                              tx, '--json', '--rpc-url', 'http://localhost:8545'))
    assert chain['status'] in ('0x1', '0x01', 1), chain
    delta = balance() - before
    assert delta == 50000, f'Unexpected balance change: {delta}'
    query = ("SELECT amount_micro_usdc, origin_status FROM payments "
             f"WHERE network='eip155:8453' AND tx_hash='{tx}' AND success")
    for _ in range(30):
        row = command('docker', 'exec', 'sluice-postgres', 'psql', '-U', 'sluice',
                      '-d', 'sluice', '-At', '-F', '|', '-c', query)
        if row == '50000|200':
            break
        time.sleep(1)
    else:
        raise AssertionError('Settlement did not reach the receipt index')
    print('Decoded payment-response header:', flush=True)
    show(receipt)
    print(f'\nChain transaction: successful\nReceiver balance: +{delta:,} micro-USDC', flush=True)
    print('Postgres receipt: amount=50000, origin_status=200\n', flush=True)
    print('402 -> signed payment -> settlement -> search result -> indexed receipt', flush=True)
    print('\nRun the full check: ./scripts/smoke.sh', flush=True)
    time.sleep(12)


if __name__ == '__main__':
    main()
