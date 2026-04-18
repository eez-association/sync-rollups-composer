#!/usr/bin/env python3
"""
Arb bot — time-bounded variant of PR #33's arb_bot.py.

Polls L1 and L2 AMM reserves every POLL_INTERVAL seconds. When a profitable
cross-chain arb exists, sends a tx via CrossChainArb on the L1 composer proxy.
Exits 0 on clean completion after --duration seconds.

Writes:
- {log_file}: per-attempt lines (OPPORTUNITY / SUCCESS / REVERTED / SEND ERROR)
- {heartbeat_file}: last-seen state (overwritten each iteration)
- stdout: one JSON summary line per 10 iterations (for monitor.py to parse)
"""

import argparse
import json
import os
import sys
import time
from datetime import datetime, timezone
from web3 import Web3
from eth_account import Account


def log(line, log_file):
    ts = datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
    msg = f'{ts} {line}'
    print(msg, flush=True)
    with open(log_file, 'a') as f:
        f.write(msg + '\n')


def swap_out(amount_in, reserve_in, reserve_out, fee_num=997, fee_den=1000):
    if amount_in == 0:
        return 0
    amount_in_with_fee = amount_in * fee_num
    num = amount_in_with_fee * reserve_out
    den = reserve_in * fee_den + amount_in_with_fee
    return num // den


def sim_arb_sell_on_l1(amount, l1w, l1u, l2w, l2u):
    usdc_out = swap_out(amount, l1w, l1u)
    if usdc_out == 0:
        return 0
    return swap_out(usdc_out, l2u, l2w)


def sim_arb_sell_on_l2(amount, l1w, l1u, l2w, l2u):
    usdc_out = swap_out(amount, l2w, l2u)
    if usdc_out == 0:
        return 0
    return swap_out(usdc_out, l1u, l1w)


def find_optimal_arb(l1w, l1u, l2w, l2u, max_amount):
    best = (None, 0, 0)
    sizes = set()
    for exp in range(15, int.bit_length(max_amount)):
        v = 1 << exp
        if v <= max_amount:
            sizes.add(v)
    for frac in (0.01, 0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9, 1.0):
        sizes.add(int(max_amount * frac))
    sizes = sorted(s for s in sizes if 10**14 <= s <= max_amount)

    for amt in sizes:
        out1 = sim_arb_sell_on_l1(amt, l1w, l1u, l2w, l2u)
        if out1 - amt > best[2]:
            best = ('l1', amt, out1 - amt)
        out2 = sim_arb_sell_on_l2(amt, l1w, l1u, l2w, l2u)
        if out2 - amt > best[2]:
            best = ('l2', amt, out2 - amt)

    direction, size, profit = best
    if direction is None:
        return None, 0, 0

    sim = sim_arb_sell_on_l1 if direction == 'l1' else sim_arb_sell_on_l2
    lo = max(10**14, size // 4)
    hi = min(max_amount, size * 4)
    for _ in range(30):
        m1 = lo + (hi - lo) // 3
        m2 = hi - (hi - lo) // 3
        p1 = sim(m1, l1w, l1u, l2w, l2u) - m1
        p2 = sim(m2, l1w, l1u, l2w, l2u) - m2
        if p1 < p2:
            lo = m1
        else:
            hi = m2
        if hi - lo < 10**12:
            break
    amt = (lo + hi) // 2
    return direction, amt, sim(amt, l1w, l1u, l2w, l2u) - amt


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('config', help='path to arb_config.json')
    ap.add_argument('--duration', type=int, default=600, help='seconds to run')
    ap.add_argument('--poll', type=int, default=3, help='poll interval (s)')
    ap.add_argument('--min-profit-usd', type=float, default=1.0)
    ap.add_argument('--max-arb-weth', type=int, default=10**18)  # 1 WETH cap per attempt
    args = ap.parse_args()

    with open(args.config) as f:
        cfg = json.load(f)
    log_file = cfg.get('log_file', f'/tmp/arb_{cfg.get("bot_name", "bot")}.log')
    heartbeat = cfg.get('heartbeat_file', f'/tmp/arb_{cfg.get("bot_name", "bot")}_hb.log')
    pid_file = cfg.get('pid_file', f'/tmp/arb_{cfg.get("bot_name", "bot")}.pid')
    with open(pid_file, 'w') as f:
        f.write(str(os.getpid()))

    w3_l1 = Web3(Web3.HTTPProvider(cfg['l1_rpc']))
    w3_proxy = Web3(Web3.HTTPProvider(cfg['l1_proxy']))
    acct = Account.from_key(cfg['test_key'])

    for k in ('arb_contract', 'weth_l1', 'amm_l1', 'amm_l2'):
        cfg[k] = Web3.to_checksum_address(cfg[k])

    amm_abi = [
        {"inputs": [], "name": "reserveA", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
        {"inputs": [], "name": "reserveB", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
    ]
    erc20_abi = [
        {"inputs": [{"type": "address"}], "name": "balanceOf", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
    ]
    arb_abi = [
        {"inputs": [{"type": "uint256"}, {"type": "uint256"}], "name": "arbSellOnL1", "outputs": [{"type": "uint256"}], "stateMutability": "nonpayable", "type": "function"},
        {"inputs": [{"type": "uint256"}, {"type": "uint256"}], "name": "arbSellOnL2", "outputs": [{"type": "uint256"}], "stateMutability": "nonpayable", "type": "function"},
    ]
    w3_l2 = Web3(Web3.HTTPProvider(cfg['l2_rpc']))
    amm_l1 = w3_l1.eth.contract(address=cfg['amm_l1'], abi=amm_abi)
    amm_l2 = w3_l2.eth.contract(address=cfg['amm_l2'], abi=amm_abi)
    weth_l1 = w3_l1.eth.contract(address=cfg['weth_l1'], abi=erc20_abi)
    arb = w3_l1.eth.contract(address=cfg['arb_contract'], abi=arb_abi)

    gas_price = w3_l1.eth.gas_price
    bot_name = cfg.get('bot_name', 'bot')
    log(f'=== ARB BOT [{bot_name}] START duration={args.duration}s ===', log_file)

    state = {
        'iterations': 0, 'opportunities': 0, 'attempts': 0,
        'successes': 0, 'failures': 0, 'timeouts': 0,
        'total_profit_weth': 0, 'total_gas_used': 0,
        'start_time': time.time(),
    }
    end = time.time() + args.duration

    while time.time() < end:
        state['iterations'] += 1
        try:
            l1w = amm_l1.functions.reserveA().call()
            l1u = amm_l1.functions.reserveB().call()
            l2w = amm_l2.functions.reserveA().call()
            l2u = amm_l2.functions.reserveB().call()
            bal = weth_l1.functions.balanceOf(cfg['arb_contract']).call()
            max_amt = min(bal, args.max_arb_weth)
            if max_amt < 10**15:
                time.sleep(args.poll)
                continue

            p_l1 = l1u / (l1w / 1e18) if l1w else 0
            p_l2 = l2u / (l2w / 1e18) if l2w else 0
            p_avg = max(p_l1, p_l2)
            direction, amount, profit_wei = find_optimal_arb(l1w, l1u, l2w, l2u, max_amt)
            profit_usd = (profit_wei / 1e18) * (p_avg / 1e6)

            with open(heartbeat, 'w') as f:
                ts = datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
                f.write(f'{ts} iter={state["iterations"]} L1={p_l1/1e6:.2f} L2={p_l2/1e6:.2f} '
                        f'best=${profit_usd:+.2f} ({direction or "none"})\n')

            if direction is None or profit_usd < args.min_profit_usd:
                time.sleep(args.poll)
                continue

            state['opportunities'] += 1
            log(f'OPPORTUNITY dir={direction} amount={amount/1e18:.6f}WETH '
                f'expected=${profit_usd:.2f}', log_file)

            min_profit = int(profit_wei * 80 // 100)
            fn = 'arbSellOnL1' if direction == 'l1' else 'arbSellOnL2'
            state['attempts'] += 1
            try:
                tx = arb.functions[fn](amount, min_profit).build_transaction({
                    'from': acct.address,
                    'nonce': w3_proxy.eth.get_transaction_count(acct.address),
                    'gas': 5_000_000,
                    'gasPrice': gas_price,
                    'chainId': w3_proxy.eth.chain_id,
                })
                signed = acct.sign_transaction(tx)
                tx_hash = w3_proxy.eth.send_raw_transaction(signed.raw_transaction)
                try:
                    rcpt = w3_l1.eth.wait_for_transaction_receipt(tx_hash, timeout=60)
                    if rcpt['status'] == 1:
                        new_bal = weth_l1.functions.balanceOf(cfg['arb_contract']).call()
                        actual = new_bal - bal
                        state['successes'] += 1
                        state['total_gas_used'] += rcpt['gasUsed']
                        state['total_profit_weth'] += actual
                        log(f'SUCCESS tx={tx_hash.hex()} gas={rcpt["gasUsed"]} '
                            f'profit={actual/1e18:.6f}WETH', log_file)
                    else:
                        state['failures'] += 1
                        log(f'REVERTED tx={tx_hash.hex()} gas={rcpt["gasUsed"]}', log_file)
                except Exception as e:
                    state['timeouts'] += 1
                    log(f'TIMEOUT tx={tx_hash.hex()} err={type(e).__name__}: {e}', log_file)
            except Exception as e:
                state['failures'] += 1
                log(f'SEND_ERROR {type(e).__name__}: {e}', log_file)

            if state['iterations'] % 10 == 0:
                print(json.dumps({'bot': bot_name, **state}), flush=True)
        except KeyboardInterrupt:
            break
        except Exception as e:
            log(f'LOOP_ERROR {type(e).__name__}: {e}', log_file)
        time.sleep(args.poll)

    elapsed = time.time() - state['start_time']
    state['elapsed'] = elapsed
    log(f'=== DONE elapsed={elapsed:.0f}s iter={state["iterations"]} '
        f'opps={state["opportunities"]} ok={state["successes"]} '
        f'fail={state["failures"]} timeout={state["timeouts"]} ===', log_file)
    print(json.dumps({'bot': bot_name, 'final': True, **state}), flush=True)


if __name__ == '__main__':
    main()
