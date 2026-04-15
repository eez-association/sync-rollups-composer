#!/usr/bin/env python3
"""
Random trader that creates price movements on L1 or L2 AMM.

Each iteration:
- Picks random pool (L1 or L2)
- Picks random direction (buy WETH with USDC, or sell WETH for USDC)
- Picks random target price move (0.5% – 5%)
- Computes trade size to achieve that move
- Executes the swap
- Sleeps random 5–20s

Uses dev#9 key. Has funds on both L1 and L2.
"""

import json
import time
import random
import math
from datetime import datetime, timezone
from web3 import Web3
from eth_account import Account

# Reuse addresses from bot config
with open('/tmp/arb_config.json') as f:
    CFG = json.load(f)

for k in ['weth_l1', 'usdc_l1', 'amm_l1', 'amm_l2', 'weth_l2', 'usdc_l2']:
    CFG[k] = Web3.to_checksum_address(CFG[k])

TRADER_KEY = '0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6'  # dev#9
TRADER = Account.from_key(TRADER_KEY)

w3_l1 = Web3(Web3.HTTPProvider(CFG['l1_rpc']))
w3_l2 = Web3(Web3.HTTPProvider(CFG['l2_rpc']))

FEE_NUM = 997
FEE_DEN = 1000
LOG = '/tmp/trader.log'
PID_FILE = '/tmp/trader.pid'

AMM_ABI = [
    {"inputs": [], "name": "reserveA", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
    {"inputs": [], "name": "reserveB", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
    {"inputs": [{"type": "address"}, {"type": "uint256"}], "name": "swap", "outputs": [{"type": "uint256"}], "stateMutability": "nonpayable", "type": "function"},
]
ERC20_ABI = [
    {"inputs": [{"type": "address"}], "name": "balanceOf", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
    {"inputs": [{"type": "address"}, {"type": "uint256"}], "name": "approve", "outputs": [{"type": "bool"}], "stateMutability": "nonpayable", "type": "function"},
    {"inputs": [{"type": "address"}, {"type": "uint256"}], "name": "mint", "outputs": [], "stateMutability": "nonpayable", "type": "function"},
]


def log(msg):
    ts = datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
    line = f'{ts} {msg}'
    print(line, flush=True)
    with open(LOG, 'a') as f:
        f.write(line + '\n')


def trade_size_for_price_move(reserve_in, pct):
    """
    Given target price move fraction `pct` (e.g. 0.03 for 3%) on a CP AMM,
    compute input amount (of tokenIn) needed.

    Selling tokenIn for tokenOut moves tokenOut/tokenIn price DOWN.
    For target price ratio k = new/old, a = reserve_in * (1/sqrt(k) - 1).

    For selling tokenA with target price-down move of `pct`:
      k = 1 - pct
      a = reserve_in * (1/sqrt(1-pct) - 1)

    (Ignores 0.3% fee — close enough for 0.5-5% targets.)
    """
    k = 1 - pct
    return int(reserve_in * (1 / math.sqrt(k) - 1))


def _mint_token(w3, token_addr, amount_needed, label):
    """Mint amount_needed (+headroom) of a MockERC20 token to the trader."""
    erc = w3.eth.contract(address=token_addr, abi=ERC20_ABI)
    bal = erc.functions.balanceOf(TRADER.address).call()
    if bal >= amount_needed:
        return True
    deficit = amount_needed - bal
    # Mint 10x the needed amount so we don't have to keep topping up
    to_mint = deficit * 10
    tx = erc.functions.mint(TRADER.address, to_mint).build_transaction({
        'from': TRADER.address,
        'nonce': w3.eth.get_transaction_count(TRADER.address),
        'gas': 200000,
        'gasPrice': 2 * 10**9,
        'chainId': w3.eth.chain_id,
    })
    signed = TRADER.sign_transaction(tx)
    try:
        receipt = w3.eth.wait_for_transaction_receipt(
            w3.eth.send_raw_transaction(signed.raw_transaction), timeout=60)
        if receipt['status'] == 1:
            log(f'minted {to_mint / (10**6 if "USDC" in label else 10**18):.2f} {label}')
            return True
        else:
            log(f'mint {label} reverted')
            return False
    except Exception as e:
        log(f'mint {label} error: {e}')
        return False


def ensure_weth_on_l1(amt):
    return _mint_token(w3_l1, CFG['weth_l1'], amt, 'L1 WETH')


def ensure_usdc_on_l1(amt):
    return _mint_token(w3_l1, CFG['usdc_l1'], amt, 'L1 USDC')


def ensure_weth_on_l2(amt):
    """L2 WETH is a bridge-wrapped token (WrappedToken.sol) — no public mint."""
    weth = w3_l2.eth.contract(address=CFG['weth_l2'], abi=ERC20_ABI)
    return weth.functions.balanceOf(TRADER.address).call() >= amt


def ensure_usdc_on_l2(amt):
    """Same — L2 USDC is bridge-wrapped, no public mint."""
    usdc = w3_l2.eth.contract(address=CFG['usdc_l2'], abi=ERC20_ABI)
    return usdc.functions.balanceOf(TRADER.address).call() >= amt


def do_trade(chain, sell_weth, pct_target):
    """
    chain: 'l1' or 'l2'
    sell_weth: True = sell WETH for USDC (price down). False = sell USDC (price up).
    pct_target: fraction in [0.005, 0.05]
    """
    if chain == 'l1':
        w3 = w3_l1
        amm_addr = CFG['amm_l1']
        weth_addr = CFG['weth_l1']
        usdc_addr = CFG['usdc_l1']
        rpc_url = CFG['l1_proxy']  # route through proxy so composer picks up if needed
    else:
        w3 = w3_l2
        amm_addr = CFG['amm_l2']
        weth_addr = CFG['weth_l2']
        usdc_addr = CFG['usdc_l2']
        rpc_url = CFG['l2_rpc']

    amm = w3.eth.contract(address=amm_addr, abi=AMM_ABI)
    ra = amm.functions.reserveA().call()  # WETH reserve
    rb = amm.functions.reserveB().call()  # USDC reserve

    if sell_weth:
        token_in = weth_addr
        amount_in = trade_size_for_price_move(ra, pct_target)
        # Cap at fraction of reserve for sanity
        amount_in = min(amount_in, ra // 3)
    else:
        token_in = usdc_addr
        amount_in = trade_size_for_price_move(rb, pct_target)
        amount_in = min(amount_in, rb // 3)

    if amount_in == 0:
        return False, 'zero amount'

    # Ensure sufficient balance (mint on L1, skip on L2 where we can't mint)
    erc = w3.eth.contract(address=token_in, abi=ERC20_ABI)
    if chain == 'l1':
        if sell_weth:
            if not ensure_weth_on_l1(amount_in):
                return False, 'mint L1 WETH failed'
        else:
            if not ensure_usdc_on_l1(amount_in):
                return False, 'mint L1 USDC failed'
    else:
        # L2 — can't mint, must have received tokens from bridge
        bal = erc.functions.balanceOf(TRADER.address).call()
        if bal < amount_in:
            return False, f'insufficient on L2 ({bal} < {amount_in})'

    # Approve + swap (via proxy if L1 — composer is tolerant of plain swaps)
    w3_send = Web3(Web3.HTTPProvider(rpc_url))

    tx1 = erc.functions.approve(amm_addr, amount_in).build_transaction({
        'from': TRADER.address,
        'nonce': w3_send.eth.get_transaction_count(TRADER.address),
        'gas': 100000,
        'gasPrice': 2 * 10**9,
        'chainId': w3_send.eth.chain_id,
    })
    signed = TRADER.sign_transaction(tx1)
    w3.eth.wait_for_transaction_receipt(w3_send.eth.send_raw_transaction(signed.raw_transaction), timeout=60)

    tx2 = amm.functions.swap(token_in, amount_in).build_transaction({
        'from': TRADER.address,
        'nonce': w3_send.eth.get_transaction_count(TRADER.address),
        'gas': 500000,
        'gasPrice': 2 * 10**9,
        'chainId': w3_send.eth.chain_id,
    })
    signed = TRADER.sign_transaction(tx2)
    tx_hash = w3_send.eth.send_raw_transaction(signed.raw_transaction)
    receipt = w3.eth.wait_for_transaction_receipt(tx_hash, timeout=60)
    if receipt['status'] != 1:
        return False, f'tx reverted: {tx_hash.hex()}'

    # Report actual price move
    ra2 = amm.functions.reserveA().call()
    rb2 = amm.functions.reserveB().call()
    p1 = rb / ra if ra else 0
    p2 = rb2 / ra2 if ra2 else 0
    actual_pct = (p2 - p1) / p1 * 100 if p1 else 0
    return True, f'{chain} {"sellWETH" if sell_weth else "buyWETH"} in={amount_in} priceMove={actual_pct:+.2f}%'


def main():
    import os
    with open(PID_FILE, 'w') as f:
        f.write(str(os.getpid()))

    log('=== TRADER START ===')
    log(f'Address: {TRADER.address}')
    log(f'L1 balance: {w3_l1.eth.get_balance(TRADER.address) / 1e18:.2f} ETH')
    log(f'L2 balance: {w3_l2.eth.get_balance(TRADER.address) / 1e18:.2f} ETH')

    # Bootstrap: mint initial WETH + USDC on L1 so we can trade either direction
    ensure_weth_on_l1(100 * 10**18)
    ensure_usdc_on_l1(200_000 * 10**6)
    weth = w3_l1.eth.contract(address=CFG['weth_l1'], abi=ERC20_ABI)
    usdc = w3_l1.eth.contract(address=CFG['usdc_l1'], abi=ERC20_ABI)
    log(f'L1 WETH: {weth.functions.balanceOf(TRADER.address).call() / 1e18:.4f}')
    log(f'L1 USDC: {usdc.functions.balanceOf(TRADER.address).call() / 1e6:.2f}')

    while True:
        try:
            # Pick random parameters
            chain = random.choice(['l1', 'l2'])
            sell_weth = random.choice([True, False])
            pct = random.uniform(0.005, 0.05)

            ok, msg = do_trade(chain, sell_weth, pct)
            if ok:
                log(f'TRADE: {msg}')
            else:
                log(f'skip: {msg}')
        except Exception as e:
            log(f'ERROR: {e}')

        time.sleep(random.uniform(5, 20))


if __name__ == '__main__':
    main()
