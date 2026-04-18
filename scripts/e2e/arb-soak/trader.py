#!/usr/bin/env python3
"""
Random trader — time-bounded variant of PR #33's trader.py.

Creates price movements on the L1 or L2 AMM so arb opportunities exist.
Uses dev#9 key (funder). Exits after --duration seconds.
"""

import argparse
import json
import math
import os
import random
import time
from datetime import datetime, timezone
from web3 import Web3
from eth_account import Account

TRADER_KEY = '0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6'  # dev#9

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


def log(line, log_file):
    ts = datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
    msg = f'{ts} {line}'
    print(msg, flush=True)
    with open(log_file, 'a') as f:
        f.write(msg + '\n')


def trade_size_for_move(reserve_in, pct):
    k = 1 - pct
    return int(reserve_in * (1 / math.sqrt(k) - 1))


def ensure_balance(w3, erc_abi, token_addr, trader, needed, mintable):
    erc = w3.eth.contract(address=token_addr, abi=erc_abi)
    bal = erc.functions.balanceOf(trader.address).call()
    if bal >= needed:
        return True
    if not mintable:
        return False
    to_mint = (needed - bal) * 10
    tx = erc.functions.mint(trader.address, to_mint).build_transaction({
        'from': trader.address,
        'nonce': w3.eth.get_transaction_count(trader.address),
        'gas': 200000,
        'gasPrice': 2 * 10**9,
        'chainId': w3.eth.chain_id,
    })
    signed = trader.sign_transaction(tx)
    try:
        rcpt = w3.eth.wait_for_transaction_receipt(w3.eth.send_raw_transaction(signed.raw_transaction), timeout=60)
        return rcpt['status'] == 1
    except Exception:
        return False


def do_trade(cfg, trader, chain, sell_weth, pct):
    if chain == 'l1':
        w3 = Web3(Web3.HTTPProvider(cfg['l1_rpc']))
        w3_send = Web3(Web3.HTTPProvider(cfg['l1_proxy']))
        amm_addr = cfg['amm_l1']
        weth_addr = cfg['weth_l1']
        usdc_addr = cfg['usdc_l1']
        mintable = True
    else:
        w3 = Web3(Web3.HTTPProvider(cfg['l2_rpc']))
        w3_send = w3
        amm_addr = cfg['amm_l2']
        weth_addr = cfg['weth_l2']
        usdc_addr = cfg['usdc_l2']
        mintable = False

    amm = w3.eth.contract(address=amm_addr, abi=AMM_ABI)
    ra = amm.functions.reserveA().call()
    rb = amm.functions.reserveB().call()

    if sell_weth:
        token_in = weth_addr
        amount_in = min(trade_size_for_move(ra, pct), ra // 3)
    else:
        token_in = usdc_addr
        amount_in = min(trade_size_for_move(rb, pct), rb // 3)

    if amount_in == 0:
        return False, 'zero_amount'
    if not ensure_balance(w3, ERC20_ABI, token_in, trader, amount_in, mintable):
        return False, 'insufficient_balance'

    erc = w3.eth.contract(address=token_in, abi=ERC20_ABI)
    tx = erc.functions.approve(amm_addr, amount_in).build_transaction({
        'from': trader.address,
        'nonce': w3_send.eth.get_transaction_count(trader.address),
        'gas': 100000, 'gasPrice': 2*10**9, 'chainId': w3_send.eth.chain_id,
    })
    try:
        w3.eth.wait_for_transaction_receipt(
            w3_send.eth.send_raw_transaction(trader.sign_transaction(tx).raw_transaction),
            timeout=60)
    except Exception as e:
        return False, f'approve_failed:{type(e).__name__}'

    tx = amm.functions.swap(token_in, amount_in).build_transaction({
        'from': trader.address,
        'nonce': w3_send.eth.get_transaction_count(trader.address),
        'gas': 500000, 'gasPrice': 2*10**9, 'chainId': w3_send.eth.chain_id,
    })
    try:
        rcpt = w3.eth.wait_for_transaction_receipt(
            w3_send.eth.send_raw_transaction(trader.sign_transaction(tx).raw_transaction),
            timeout=60)
        if rcpt['status'] != 1:
            return False, 'swap_reverted'
    except Exception as e:
        return False, f'swap_failed:{type(e).__name__}'

    ra2 = amm.functions.reserveA().call()
    rb2 = amm.functions.reserveB().call()
    p1 = rb / ra if ra else 0
    p2 = rb2 / ra2 if ra2 else 0
    move = (p2 - p1) / p1 * 100 if p1 else 0
    return True, f'{chain} {"sellWETH" if sell_weth else "buyWETH"} move={move:+.2f}%'


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--config', default='/tmp/arb_config.json')
    ap.add_argument('--duration', type=int, default=600)
    ap.add_argument('--log', default='/tmp/trader.log')
    args = ap.parse_args()

    with open(args.config) as f:
        cfg = json.load(f)
    for k in ('weth_l1', 'usdc_l1', 'amm_l1', 'amm_l2', 'weth_l2', 'usdc_l2'):
        cfg[k] = Web3.to_checksum_address(cfg[k])
    trader = Account.from_key(TRADER_KEY)
    with open('/tmp/trader.pid', 'w') as f:
        f.write(str(os.getpid()))

    log(f'=== TRADER START duration={args.duration}s addr={trader.address} ===', args.log)
    # Bootstrap mintable balances
    w3_l1 = Web3(Web3.HTTPProvider(cfg['l1_rpc']))
    ensure_balance(w3_l1, ERC20_ABI, cfg['weth_l1'], trader, 100 * 10**18, True)
    ensure_balance(w3_l1, ERC20_ABI, cfg['usdc_l1'], trader, 200_000 * 10**6, True)

    trades_ok = 0
    trades_fail = 0
    end = time.time() + args.duration
    while time.time() < end:
        try:
            chain = random.choice(['l1', 'l2'])
            sell_weth = random.choice([True, False])
            pct = random.uniform(0.005, 0.05)
            ok, msg = do_trade(cfg, trader, chain, sell_weth, pct)
            if ok:
                trades_ok += 1
                log(f'TRADE {msg}', args.log)
            else:
                trades_fail += 1
                log(f'skip {msg}', args.log)
        except Exception as e:
            trades_fail += 1
            log(f'ERROR {type(e).__name__}: {e}', args.log)
        time.sleep(random.uniform(5, 20))

    log(f'=== TRADER DONE ok={trades_ok} fail={trades_fail} ===', args.log)
    print(json.dumps({'trader_ok': trades_ok, 'trader_fail': trades_fail, 'final': True}), flush=True)


if __name__ == '__main__':
    main()
