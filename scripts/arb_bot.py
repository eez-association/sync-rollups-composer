#!/usr/bin/env python3
"""
Cross-chain atomic arbitrage bot.

Monitors L1 and L2 AMM reserves. When prices diverge enough for profitable arb,
computes optimal trade size, simulates, and executes the atomic arb via
CrossChainArb.sol on L1.

Continuous loop — polls every 3 seconds.
"""

import json
import time
import sys
import os
from datetime import datetime, timezone
from web3 import Web3
from eth_account import Account
from eth_abi import encode, decode

# ── Config ─────────────────────────────────────────────────────────────
CFG_PATH = sys.argv[1] if len(sys.argv) > 1 else '/tmp/arb_config.json'
with open(CFG_PATH) as f:
    CFG = json.load(f)
BOT_NAME = CFG.get('bot_name', 'bot1')

w3_l1 = Web3(Web3.HTTPProvider(CFG['l1_rpc']))
w3_proxy = Web3(Web3.HTTPProvider(CFG['l1_proxy']))
w3_l2 = Web3(Web3.HTTPProvider(CFG['l2_rpc']))
acct = Account.from_key(CFG['test_key'])

# Normalize all addresses to checksum
for k in ['arb_contract', 'weth_l1', 'usdc_l1', 'amm_l1', 'bridge',
          'l2_executor', 'l2_executor_proxy', 'amm_l2', 'weth_l2', 'usdc_l2']:
    CFG[k] = Web3.to_checksum_address(CFG[k])

# ── Constants ───────────────────────────────────────────────────────────
FEE_NUM = 997           # SimpleAMM fee: 0.3% → keep 99.7%
FEE_DEN = 1000
POLL_INTERVAL = 3       # seconds
MIN_PROFIT_USD = 1.0    # threshold
MAX_ARB_SIZE_WETH = 10**18  # 1 WETH = working capital, never arb more than we own
GAS_PRICE = w3_l1.eth.gas_price
LOG_FILE = CFG.get('log_file', '/tmp/arb_bot.log')
HEARTBEAT_FILE = CFG.get('heartbeat_file', '/tmp/arb_bot_heartbeat.log')
PID_FILE = CFG.get('pid_file', '/tmp/arb_bot.pid')

# ── ABIs (minimal) ──────────────────────────────────────────────────────
AMM_ABI = [
    {"inputs": [], "name": "reserveA", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
    {"inputs": [], "name": "reserveB", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
    {"inputs": [], "name": "tokenA", "outputs": [{"type": "address"}], "stateMutability": "view", "type": "function"},
    {"inputs": [], "name": "tokenB", "outputs": [{"type": "address"}], "stateMutability": "view", "type": "function"},
]
ERC20_ABI = [
    {"inputs": [{"type": "address"}], "name": "balanceOf", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
]
ARB_ABI = [
    {"inputs": [{"type": "uint256"}, {"type": "uint256"}], "name": "arbSellOnL1", "outputs": [{"type": "uint256"}], "stateMutability": "nonpayable", "type": "function"},
    {"inputs": [{"type": "uint256"}, {"type": "uint256"}], "name": "arbSellOnL2", "outputs": [{"type": "uint256"}], "stateMutability": "nonpayable", "type": "function"},
]

amm_l1 = w3_l1.eth.contract(address=CFG['amm_l1'], abi=AMM_ABI)
amm_l2 = w3_l2.eth.contract(address=CFG['amm_l2'], abi=AMM_ABI)
weth_l1 = w3_l1.eth.contract(address=CFG['weth_l1'], abi=ERC20_ABI)
arb = w3_l1.eth.contract(address=CFG['arb_contract'], abi=ARB_ABI)

# ── Helpers ─────────────────────────────────────────────────────────────

def log(msg):
    ts = datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
    line = f'{ts} {msg}'
    print(line, flush=True)
    with open(LOG_FILE, 'a') as f:
        f.write(line + '\n')


def get_reserves():
    """Return (l1_weth, l1_usdc, l2_weth, l2_usdc) — tokenA is WETH, tokenB is USDC by convention."""
    l1a = amm_l1.functions.reserveA().call()
    l1b = amm_l1.functions.reserveB().call()
    l2a = amm_l2.functions.reserveA().call()
    l2b = amm_l2.functions.reserveB().call()
    return l1a, l1b, l2a, l2b


def swap_out(amount_in, reserve_in, reserve_out):
    """Constant-product AMM output for given input with 0.3% fee."""
    if amount_in == 0:
        return 0
    amount_in_with_fee = amount_in * FEE_NUM
    numerator = amount_in_with_fee * reserve_out
    denominator = reserve_in * FEE_DEN + amount_in_with_fee
    return numerator // denominator


def simulate_arb_sell_on_l1(amount_weth, l1_weth, l1_usdc, l2_weth, l2_usdc):
    """
    Path: WETH → USDC (L1 AMM) → USDC (L2 AMM) → WETH. Returns final WETH.
    Profit = final_weth - amount_weth.
    """
    usdc_out = swap_out(amount_weth, l1_weth, l1_usdc)
    if usdc_out == 0:
        return 0
    # L2 AMM: tokenA=WETH, tokenB=USDC. We're swapping USDC (tokenB) for WETH (tokenA).
    weth_out = swap_out(usdc_out, l2_usdc, l2_weth)
    return weth_out


def simulate_arb_sell_on_l2(amount_weth, l1_weth, l1_usdc, l2_weth, l2_usdc):
    """
    Path: WETH (bridged to L2) → USDC (L2 AMM) → (bridged to L1) → WETH (L1 AMM).
    """
    usdc_out = swap_out(amount_weth, l2_weth, l2_usdc)
    if usdc_out == 0:
        return 0
    weth_out = swap_out(usdc_out, l1_usdc, l1_weth)
    return weth_out


def find_optimal_arb(l1_weth, l1_usdc, l2_weth, l2_usdc, max_amount):
    """
    Grid search + refine for optimal arb size and direction.
    Returns (direction, amount_weth, profit_weth) — direction is 'l1' or 'l2'
    (sell side), None if no profitable trade.
    """
    best = (None, 0, 0)

    # Grid search over log-spaced sizes
    sizes = []
    for exp in range(15, int.bit_length(max_amount)):
        v = 1 << exp
        if v > max_amount:
            break
        sizes.append(v)
    # Add mid-range fractions
    for frac in [0.01, 0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9, 1.0]:
        sizes.append(int(max_amount * frac))
    sizes = sorted(set(s for s in sizes if 10**14 <= s <= max_amount))

    for amount in sizes:
        # Try sell on L1 first (L1 expensive, L2 cheap)
        out = simulate_arb_sell_on_l1(amount, l1_weth, l1_usdc, l2_weth, l2_usdc)
        profit = out - amount
        if profit > best[2]:
            best = ('l1', amount, profit)

        # Try sell on L2 first (L2 expensive, L1 cheap)
        out = simulate_arb_sell_on_l2(amount, l1_weth, l1_usdc, l2_weth, l2_usdc)
        profit = out - amount
        if profit > best[2]:
            best = ('l2', amount, profit)

    # Refine around the best size with ternary-like search
    direction, size, profit = best
    if direction is None:
        return None, 0, 0

    sim = simulate_arb_sell_on_l1 if direction == 'l1' else simulate_arb_sell_on_l2
    lo, hi = max(10**14, size // 4), min(max_amount, size * 4)
    for _ in range(30):
        m1 = lo + (hi - lo) // 3
        m2 = hi - (hi - lo) // 3
        p1 = sim(m1, l1_weth, l1_usdc, l2_weth, l2_usdc) - m1
        p2 = sim(m2, l1_weth, l1_usdc, l2_weth, l2_usdc) - m2
        if p1 < p2:
            lo = m1
        else:
            hi = m2
        if hi - lo < 10**12:
            break
    amount = (lo + hi) // 2
    profit = sim(amount, l1_weth, l1_usdc, l2_weth, l2_usdc) - amount
    return direction, amount, profit


def send_tx(fn_name, amount_in, min_profit):
    """Send arb tx through L1 proxy."""
    fn = arb.functions[fn_name](amount_in, min_profit)
    tx = fn.build_transaction({
        'from': acct.address,
        'nonce': w3_proxy.eth.get_transaction_count(acct.address),
        'gas': 5_000_000,
        'gasPrice': GAS_PRICE,
        'chainId': w3_proxy.eth.chain_id,
    })
    signed = acct.sign_transaction(tx)
    tx_hash = w3_proxy.eth.send_raw_transaction(signed.raw_transaction)
    receipt = w3_l1.eth.wait_for_transaction_receipt(tx_hash, timeout=60)
    return tx_hash.hex(), receipt


# ── State ───────────────────────────────────────────────────────────────
state = {
    'iterations': 0,
    'opportunities': 0,
    'attempts': 0,
    'successes': 0,
    'failures': 0,
    'total_profit_weth': 0,
    'total_gas_used': 0,
    'start_time': time.time(),
}


def pct(a, b):
    return f'{(a / b * 100):.3f}%' if b else '—'


def main():
    log(f'=== ARB BOT [{BOT_NAME}] START ===')
    log(f'Config: {CFG_PATH}')
    log(f'Arb contract: {CFG["arb_contract"]}')
    log(f'Min profit: ${MIN_PROFIT_USD}')
    log(f'Max arb size: {MAX_ARB_SIZE_WETH / 1e18} WETH')
    log(f'Poll interval: {POLL_INTERVAL}s')
    log(f'Gas price: {GAS_PRICE} wei')

    # Read starting arb balance
    start_bal = weth_l1.functions.balanceOf(CFG['arb_contract']).call()
    log(f'Starting arb balance: {start_bal / 1e18} WETH')

    run_until = time.time() + 24 * 3600  # 1 day

    while time.time() < run_until:
        state['iterations'] += 1
        try:
            l1a, l1b, l2a, l2b = get_reserves()

            # Current arb balance (working capital)
            bal = weth_l1.functions.balanceOf(CFG['arb_contract']).call()
            max_amount = min(bal, MAX_ARB_SIZE_WETH)
            if max_amount < 10**15:  # < 0.001 WETH — can't trade
                log(f'[skip] arb balance too low: {bal / 1e18:.6f} WETH')
                time.sleep(POLL_INTERVAL)
                continue

            # Compute prices (USDC per WETH, 6 decimals)
            p_l1 = l1b / (l1a / 1e18) if l1a else 0
            p_l2 = l2b / (l2a / 1e18) if l2a else 0
            spread_pct = (p_l1 - p_l2) / p_l1 * 100 if p_l1 else 0

            # Find optimal arb
            direction, amount, profit_wei = find_optimal_arb(l1a, l1b, l2a, l2b, max_amount)

            # Compute profit in USD (profit_wei × USDC/WETH price / 1e18 / 1e6 USDC decimals)
            # profit is in WETH wei; convert to USDC value using the higher price
            p_avg = max(p_l1, p_l2)  # USDC-per-WETH (in USDC ticks / WETH ticks, ticks = 1e6 and 1e18)
            # profit_usd = profit_wei * (price) / 1e18, where price is USDC ticks per WETH wei
            # p_avg = usdc_ticks_per_weth_wei_approx... simpler:
            profit_weth_float = profit_wei / 1e18
            profit_usd = profit_weth_float * p_avg  # p_avg is USDC per 1 WETH (already accounts for 1e18 scale)
            # Convert USDC ticks to USD: p_avg is already USDC-ticks per WETH. USDC has 6 decimals.
            profit_usd = profit_usd / 1e6

            # Heartbeat line every iteration — dashboard parses this for live state.
            # Write to a separate heartbeat file to keep the main log compact.
            heartbeat = (f'[{state["iterations"]}] L1={p_l1/1e6:.2f} L2={p_l2/1e6:.2f} '
                         f'spread={spread_pct:+.2f}% best={profit_usd:+.2f} USD ({direction or "none"})')
            with open(HEARTBEAT_FILE, 'w') as f:
                f.write(f'{datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")} {heartbeat}\n')
            # Only write to the main log every 10th poll to avoid spam
            if state['iterations'] % 10 == 0:
                log(heartbeat)

            if direction is None or profit_usd < MIN_PROFIT_USD:
                time.sleep(POLL_INTERVAL)
                continue

            state['opportunities'] += 1
            log(f'OPPORTUNITY: dir={direction} amount={amount / 1e18:.6f} WETH expected_profit={profit_usd:.2f} USD')

            # Set minProfit = 80% of expected (buffer for price drift)
            min_profit = int(profit_wei * 80 // 100)
            fn_name = 'arbSellOnL1' if direction == 'l1' else 'arbSellOnL2'

            state['attempts'] += 1
            try:
                tx_hash, receipt = send_tx(fn_name, amount, min_profit)
                if receipt['status'] == 1:
                    state['successes'] += 1
                    state['total_gas_used'] += receipt['gasUsed']
                    # Read actual balance change
                    new_bal = weth_l1.functions.balanceOf(CFG['arb_contract']).call()
                    actual_profit_wei = new_bal - bal
                    state['total_profit_weth'] += actual_profit_wei
                    actual_profit_usd = actual_profit_wei / 1e18 * p_avg / 1e6
                    log(f'SUCCESS: tx={tx_hash} gas={receipt["gasUsed"]} profit={actual_profit_wei / 1e18:.6f} WETH (${actual_profit_usd:.2f})')
                else:
                    state['failures'] += 1
                    log(f'REVERTED: tx={tx_hash} gas={receipt["gasUsed"]}')
            except Exception as e:
                state['failures'] += 1
                log(f'SEND ERROR: {e}')

            # Summary every time we attempt
            elapsed = time.time() - state['start_time']
            log(f'STATS: iter={state["iterations"]} opps={state["opportunities"]} '
                f'ok={state["successes"]} fail={state["failures"]} '
                f'profit={state["total_profit_weth"] / 1e18:.6f} WETH '
                f'gas={state["total_gas_used"]} elapsed={elapsed:.0f}s')

        except KeyboardInterrupt:
            log('=== STOPPED ===')
            break
        except Exception as e:
            log(f'ERROR: {e}')

        time.sleep(POLL_INTERVAL)

    log(f'=== DONE ===')
    log(f'Iterations: {state["iterations"]}')
    log(f'Opportunities: {state["opportunities"]}')
    log(f'Attempts: {state["attempts"]}')
    log(f'Successes: {state["successes"]} / Failures: {state["failures"]}')
    log(f'Total profit: {state["total_profit_weth"] / 1e18:.6f} WETH')
    log(f'Total gas used: {state["total_gas_used"]}')


if __name__ == '__main__':
    main()
