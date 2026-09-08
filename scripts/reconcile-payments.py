#!/usr/bin/env python3
"""Recover uncertain exact EIP-3009 outcomes from confirmed chain evidence.
No signing keys or payment signatures are read. Unknown outcomes stay blocked.
Run: uv run --with web3 python scripts/reconcile-payments.py JOURNAL
"""
import argparse,datetime,json,sqlite3
from web3 import Web3
p=argparse.ArgumentParser()
p.add_argument('journal')
p.add_argument('--write',action='store_true',help='Persist chain-proven successful outcomes; default is read-only')
a=p.parse_args()
db=sqlite3.connect(f'file:{a.journal}?mode={"rw" if a.write else "ro"}',uri=True,timeout=5)
networks={
 'eip155:84532':('https://sepolia.base.org','0x036CbD53842c5426634e7929541eC2318f3dCF7e'),
 'eip155:8453':('https://mainnet.base.org','0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913'),
}
def topic_address(address):return '0x'+address.lower().removeprefix('0x').rjust(64,'0')
def first_block(w3,timestamp,high):
 distance=1024
 low=max(0,high-distance)
 while low>0 and w3.eth.get_block(low)['timestamp']>timestamp:
  distance*=2
  if distance>1048576:raise RuntimeError('Recovery range exceeds bounded recent history')
  low=max(0,high-distance)
 while low<high:
  mid=(low+high)//2
  if w3.eth.get_block(mid)['timestamp']<timestamp:low=mid+1
  else:high=mid
 return low
for attempt,raw,created in db.execute("SELECT id,authorization_details,created_at FROM attempts WHERE phase IN ('settling','uncertain') ORDER BY created_at LIMIT 20").fetchall():
 details=json.loads(raw);network=details['network']
 if network not in networks:print(attempt,'unsupported network; unchanged');continue
 rpc,asset=networks[network]
 if details['asset'].lower()!=asset.lower():print(attempt,'unsupported asset; unchanged');continue
 w3=Web3(Web3.HTTPProvider(rpc,request_kwargs={'timeout':20}))
 assert w3.eth.chain_id==int(network.split(':')[1])
 now=int(w3.eth.get_block('latest')['timestamp'])
 created_at=int(datetime.datetime.fromisoformat(created).replace(tzinfo=datetime.timezone.utc).timestamp())
 if now-created_at>7*86400:print(attempt,'older than automatic search window; unchanged');continue
 # Stay behind the chain head; this is confirmation evidence, not a claim of finality.
 end=max(0,w3.eth.block_number-12)
 start=first_block(w3,max(0,created_at-300),end)
 used='0x'+Web3.keccak(text='AuthorizationUsed(address,bytes32)').hex()
 transfer='0x'+Web3.keccak(text='Transfer(address,address,uint256)').hex()
 txs=[]
 for lo in range(start,end+1,500):
  txs.extend(w3.eth.get_logs({'address':Web3.to_checksum_address(asset),'fromBlock':lo,'toBlock':min(lo+499,end),'topics':[used,topic_address(details['payer']),details['nonce']]}))
 recovered=None
 for event in txs:
  receipt=w3.eth.get_transaction_receipt(event['transactionHash'])
  if receipt.status!=1:continue
  for log in receipt.logs:
   topics=['0x'+bytes(t).hex() for t in log.topics]
   if log.address.lower()==asset.lower() and topics==[transfer,topic_address(details['payer']),topic_address(details['receiver'])] and int.from_bytes(log.data,'big')==int(details['amount']):
    recovered={'success':True,'network':network,'payer':details['payer'],'transaction':'0x'+bytes(receipt.transactionHash).hex()}
 if recovered:
  if a.write:
   with db:db.execute("UPDATE attempts SET phase='settled',settlement=? WHERE id=? AND phase IN ('settling','uncertain')",(json.dumps(recovered),attempt))
  print(attempt,'chain-proven transfer',recovered['transaction'],'persisted' if a.write else 'read-only')
 else:print(attempt,'no matching confirmed transfer; outcome remains unknown')
