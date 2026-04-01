#!/usr/bin/env bash
# debug-trace-iterative.sh — Full lifecycle debugger for cross-chain transactions
#
# Reconstructs the ENTIRE flow of a cross-chain tx from builder logs:
#   Phase A: L2 Detection (initial trace, proxy call detection)
#   Phase B: Iterative Discovery (L1 simulation → L2 re-trace → repeat)
#   Phase C: Entry Construction (L1 deferred entries, L2 table entries)
#   Phase D: L1 Submission (postBatch + trigger execution)
#
# All data comes from builder logs — no external computation needed.
#
# Usage:
#   # Show last intercepted tx:
#   HEALTH_URL=http://localhost:11560/health bash scripts/e2e/debug-trace-iterative.sh
#
#   # Show specific tx by target address:
#   HEALTH_URL=http://localhost:11560/health bash scripts/e2e/debug-trace-iterative.sh 0xF801fc...
#
#   # Specify time window:
#   SINCE=300s HEALTH_URL=http://localhost:11560/health bash scripts/e2e/debug-trace-iterative.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

HEALTH_URL="${HEALTH_URL:-http://localhost:11560/health}"
SINCE="${SINCE:-600s}"
TARGET_FILTER="${1:-}"

# Auto-detect compose
if echo "$HEALTH_URL" | grep -q "11560"; then
  COMPOSE="sudo docker compose -f deployments/devnet-eez/docker-compose.yml -f deployments/devnet-eez/docker-compose.dev.yml"
else
  COMPOSE="sudo docker compose -f deployments/testnet-eez/docker-compose.yml -f deployments/testnet-eez/docker-compose.dev.yml"
fi

# ── Collect raw logs ──
LOGS=$($COMPOSE logs builder --no-log-prefix --since "$SINCE" 2>&1 | sed 's/\x1b\[[0-9;]*m//g')

# ── Resolve target ──
# Accepts a tx hash (0x...) or contract address as filter
if [ -z "$TARGET_FILTER" ]; then
  # Find the most recent user_tx from the logs
  TARGET_FILTER=$(echo "$LOGS" | grep "intercepted eth_sendRawTransaction" | tail -1 | grep -oP 'user_tx_hash=\K0x[0-9a-fA-F]+' || echo "")
  if [ -z "$TARGET_FILTER" ]; then
    TARGET_FILTER=$(echo "$LOGS" | grep "tracing L2 tx with debug_traceCall" | tail -1 | grep -oP 'to_addr=\K0x[0-9a-fA-F]+' || echo "")
  fi
  if [ -z "$TARGET_FILTER" ]; then
    echo "No intercepted tx found in last $SINCE of logs."
    echo "Usage: $0 [tx_hash_or_address]"
    exit 1
  fi
fi
TARGET_SHORT="${TARGET_FILTER:0:14}..."

echo "Filter: $TARGET_FILTER"
echo ""

# ── Load addresses for labeling ──
eval "$($COMPOSE exec -T builder cat /shared/rollup.env 2>/dev/null)" 2>/dev/null || true
CCM="${CROSS_CHAIN_MANAGER_ADDRESS:-unknown}"

# ── Feed everything to Python for rendering ──
# Write Python script to temp file to avoid bash escaping issues
RENDER_PY=$(mktemp /tmp/debug_render_XXXXX.py)
trap "rm -f $RENDER_PY" EXIT

cat > "$RENDER_PY" << 'PYTHON_EOF'
import sys, json, re, os

target = os.environ.get('TARGET_FILTER', '').lower()
ccm = os.environ.get('CCM', '').lower()
target_short = os.environ.get('TARGET_SHORT', '?')

labels = {}
labels[ccm] = 'CCM'

def lbl(addr):
    a = (addr or '?').lower()
    return labels.get(a, a[:6] + '..' + a[-4:] if len(a) > 12 else a)

def rx(pattern, line):
    m = re.search(pattern, line)
    return m.group(1) if m else ''

# ── Collect ──
all_lines = sys.stdin.read().split('\n')
relevant = []
ctx = False
for line in all_lines:
    if target in line.lower():
        ctx = True
    if ctx:
        relevant.append(line)
        if 'hold-then-forward' in line or 'forwarding tx as-is' in line:
            ctx = False
if not relevant:
    relevant = [l for l in all_lines if any(k in l for k in ['proxy', 'trace', 'discovery', 'detect'])]

# ── Colors ──
C='\033[36m'; G='\033[32m'; R='\033[31m'; Y='\033[33m'; B='\033[1m'; D='\033[2m'; N='\033[0m'
L2='\033[34m'; L1='\033[35m'

W = 72  # box width
def box_top(title):    print(f'{C}{"━"*W}{N}'); print(f'{C}{B}  {title}{N}'); print(f'{C}{"─"*W}{N}')
def box_section(t):    print(f'\n{B}{t}{N}'); print(f'{"─"*W}')
def box_end():         print(f'{C}{"━"*W}{N}')

# Chain indicators
L2_TAG = f'{L2}[L2]{N}'
L1_TAG = f'{L1}[L1]{N}'
ARROW_L2_TO_L1 = f'{L2}L2{N} ──────▶ {L1}L1{N}'
ARROW_L1_TO_L2 = f'{L1}L1{N} ──────▶ {L2}L2{N}'

box_top(f'CROSS-CHAIN TX LIFECYCLE  {target_short}')
iteration = 0
file_counter = 0

for line in relevant:
    ts = rx(r'(\d{2}:\d{2}:\d{2}\.\d{3})', line)

    # ═══ Phase A ═══
    if 'tracing L2 tx with debug_traceCall' in line and target in line.lower():
        to_val = rx(r'to_addr=(\S+)', line)
        sender = rx(r'sender=(\S+)', line)
        user_tx = rx(r'user_tx=(\S+)', line)
        box_section(f'{L2_TAG} PHASE A: Initial L2 Trace')
        print(f'  {D}{ts}{N}  sender: {sender}')
        print(f'  {D}{ts}{N}  target: {to_val}')
        if user_tx:
            print(f'  {D}{ts}{N}  tx:     {user_tx}')

    elif 'detected cross-chain proxy call' in line:
        proxy = rx(r'proxy=(\S+)', line)
        dest = rx(r'destination=(\S+)', line)
        dp = rx(r'depth=(\d+)', line)
        src = rx(r'source=(\S+)', line)
        labels[proxy.lower()] = f'Proxy({dest[-6:]})'
        labels[dest.lower()] = dest[-8:]
        scope_str = '[' + ','.join(['0'] * int(dp)) + ']' if dp else '[]'
        print(f'  {D}{ts}{N}  {G}▸{N} detected: {lbl(src)} → {Y}{lbl(proxy)}{N} → {ARROW_L2_TO_L1} {lbl(dest)}')
        print(f'  {D}{ts}{N}           depth={dp}  scope={scope_str}')

    elif 'detected internal L2' in line and 'cross-chain calls' in line:
        c = rx(r'count=(\d+)', line)
        print(f'  {D}{ts}{N}  {B}Total: {c} cross-chain call(s) detected{N}')

    # ═══ Phase B ═══
    elif 'iterative L2 discovery: traceCallMany' in line:
        it = rx(r'iteration=(\d+)', line)
        kn = rx(r'known_calls=(\d+)', line)
        iteration = int(it) if it else 0
        box_section(f'PHASE B: Iterative Discovery — Iteration {it}  ({kn} known)')

    elif 'L1 delivery return data via debug_traceCallMany' in line:
        dest = rx(r'destination=(\S+)', line)
        rlen = rx(r'return_data_len=(\d+)', line)
        rhex = rx(r'return_data_hex=(\S+)', line)
        failed = rx(r'delivery_failed=(\w+)', line)
        st = f'{R}FAILED{N}' if failed == 'true' else f'{G}OK{N}'
        print(f'  {D}{ts}{N}  {L1_TAG} Simulate delivery → {lbl(dest)}')
        print(f'           {st}  return_data({rlen}B): {D}{rhex}{N}')

    elif 'L1 delivery reverted' in line:
        dest = rx(r'destination=(\S+)', line)
        print(f'  {D}{ts}{N}  {L1_TAG} Simulate delivery → {lbl(dest)}')
        print(f'           {R}REVERTED (no output){N}')

    elif 'L2 table entry for loadExecutionTable' in line:
        idx = rx(r'idx=(\d+)', line)
        ah = rx(r'action_hash=(\S+)', line)
        nat = rx(r'next_action_type=(\S+)', line)
        sl = rx(r'next_action_scope_len=(\d+)', line)
        ah_short = ah[:14] + '..' if ah else '?'
        scope_info = f'scope[{sl}]' if sl != '0' else ''
        arrow = '→' if nat == 'Result' else '⇒'
        print(f'  {D}{ts}{N}  {L2_TAG} Entry[{idx}]: {D}hash={ah_short}{N} {arrow} {nat} {Y}{scope_info}{N}')

    elif 'loadExecutionTable calldata built' in line:
        ec = rx(r'entry_count=(\d+)', line)
        cl = rx(r'calldata_len=(\d+)', line)
        print(f'  {D}{ts}{N}  {L2_TAG} loadExecutionTable: {B}{ec} entries{N} ({cl} bytes)')

    elif 'user tx trace tree' in line:
        tree_str = rx(r'trace_tree=(.+?)$', line)
        err = rx(r'user_error="([^"]*)"', line)
        nodes = rx(r'trace_nodes=(\d+)', line)
        st = f'{R}REVERT{N}' if err and err != 'none' else f'{G}OK{N}'
        print(f'\n  {D}{ts}{N}  {L2_TAG} Re-trace result: {st} ({nodes} nodes)')
        if tree_str:
            print(f'  {"─"*50}')
            for part in tree_str.split(' | '):
                pp = part.split(':')
                if len(pp) >= 5:
                    depth_s, addr, sel, ch, status = pp[0], pp[1], pp[2], pp[3], pp[4]
                    depth_n = int(depth_s.replace('d=','')) if 'd=' in depth_s else 0
                    indent = '    ' * depth_n
                    icon = f'{G}✓{N}' if status == 'ok' else f'{R}✗{N}'
                    a = lbl('0x' + addr) if not addr.startswith('0x') else lbl(addr)
                    print(f'  {indent}{icon} {a} {D}{sel}{N}  {D}{ch}{N}')
            print(f'  {"─"*50}')

    elif 'walker results from re-trace' in line:
        det = rx(r'detected_in_retrace=(\d+)', line)
        kn = rx(r'known_calls=(\d+)', line)
        print(f'  {D}{ts}{N}  Walker: {det} in trace, {kn} known')

    elif 'detected call in re-trace' in line:
        dest = rx(r'destination=(\S+)', line)
        dp = rx(r'trace_depth=(\d+)', line)
        print(f'           ▸ {lbl(dest)} depth={dp}')

    elif 'discovered new L2' in line and 'calls' in line:
        n = rx(r'new=(\d+)', line)
        print(f'  {D}{ts}{N}  {G}★ {n} NEW call(s) discovered!{N}')

    elif 'iterative L2 discovery converged' in line:
        t = rx(r'total=(\d+)', line)
        print(f'  {D}{ts}{N}  {G}✓ Converged: {t} total calls{N}')

    elif 'iterative L2 discovery complete' in line:
        c = rx(r'count=(\d+)', line)
        box_section(f'Discovery Result: {c} call(s)')

    elif 'iterative discovery final call' in line:
        dest = rx(r'destination=(\S+)', line)
        src = rx(r'source_address=(\S+)', line)
        sel = rx(r'calldata_prefix=(\S+)', line)
        idx = rx(r'idx=(\d+)', line)
        print(f'  Call[{idx}]: {lbl(src)} ──{ARROW_L2_TO_L1}──▶ {lbl(dest)}  {D}{sel}{N}')

    # ═══ Phase C ═══
    elif 'L1 target has code' in line:
        dest = rx(r'destination=(\S+)', line)
        box_section(f'{L1_TAG} PHASE C: L1 Delivery Simulation')
        print(f'  {D}{ts}{N}  Target: {lbl(dest)} (has code)')

    elif 'L1 delivery simulation iteration' in line:
        it = rx(r'iteration=(\d+)', line)
        rc = rx(r'known_return_calls=(\d+)', line)
        print(f'  {D}{ts}{N}  {L1_TAG} sim iteration {it} (return_calls={rc})')

    elif 'trigger tx reverted in L1 simulation' in line:
        print(f'  {D}{ts}{N}  {Y}⚠ L1 trigger reverted in simulation{N}')

    elif 'combined L1 delivery simulation converged' in line:
        rc = rx(r'total_return_calls=(\d+)', line)
        print(f'  {D}{ts}{N}  {G}✓ L1 simulation converged{N} (return_calls={rc})')

    elif 'L1 delivery simulation complete' in line:
        rlen = rx(r'return_data_len=(\d+)', line)
        failed = rx(r'delivery_failed=(\w+)', line)
        st = f'{R}FAILED{N}' if failed == 'true' else f'{G}OK{N}'
        print(f'  {D}{ts}{N}  {G}✓ L1 delivery complete{N}: {st} return={rlen}B')

    # ═══ Phase D ═══
    elif 'queued L2' in line and 'cross-chain call' in line:
        dest = rx(r'destination=(\S+)', line)
        cid = rx(r'call_id=(\S+)', line)
        cid_short = cid[:14] + '..' if cid else '?'
        box_section(f'PHASE D: Queue & Submit')
        print(f'  {D}{ts}{N}  {G}✓ Queued{N}: → {lbl(dest)}  id={D}{cid_short}{N}')

    elif 'hold-then-forward' in line:
        txh = rx(r'tx_hash=(\S+)', line)
        print(f'  {D}{ts}{N}  {G}✓ TX held for driver injection{N}')
        print(f'           tx: {txh}')

    elif 'built execution table for multi-call' in line:
        l2e = rx(r'l2_entries=(\d+)', line)
        l1e = rx(r'l1_entries=(\d+)', line)
        print(f'  {D}{ts}{N}  {G}✓ Multi-call table built{N}: {L2_TAG} {l2e} entries  {L1_TAG} {l1e} entries')

    elif 'sent executeL2TX trigger' in line:
        nonce = rx(r'nonce=(\d+)', line)
        print(f'  {D}{ts}{N}  {L1_TAG} {Y}▸ executeL2TX trigger sent{N} (nonce={nonce})')

    elif 'postBatch confirmed' in line:
        block = rx(r'l1_block_number=(\d+)', line)
        print(f'  {D}{ts}{N}  {L1_TAG} {G}✓ postBatch confirmed{N} block={block}')

    elif 'merging RPC cross-chain entries' in line:
        c = rx(r'count=(\d+)', line)
        print(f'  {D}{ts}{N}  {L2_TAG} Merging {c} entry group(s) into next L2 block')

    # ═══ Errors ═══
    elif 'skipping deferred entry' in line:
        ah = rx(r'action_hash=(\S+)', line)
        ah_short = ah[:14] + '..' if ah else '?'
        print(f'  {D}{ts}{N}  {R}⚠ Unconsumed entry skipped{N}: {D}{ah_short}{N}')

    elif 'pre_state_root mismatch' in line:
        print(f'  {D}{ts}{N}  {R}✗ State root mismatch → rewind{N}')

    elif 'rewinding L2 chain' in line:
        tgt = rx(r'rewind_target=(\d+)', line) or rx(r'target=(\d+)', line)
        print(f'  {D}{ts}{N}  {R}↺ Rewind to L2 block {tgt}{N}')

    # ═══ Raw request/response ═══
    elif 'debug_traceCallMany REQUEST' in line:
        it = rx(r'iteration=(\d+)', line)
        utx = rx(r'user_tx=(\S+)', line)
        fname = f'/tmp/debug_trace_req_iter{it}_{utx[:10] if utx else "unknown"}.json'
        req_match = re.search(r'request=({.*})\s*$', line)
        if req_match:
            try:
                with open(fname, 'w') as f:
                    f.write(req_match.group(1))
                print(f'  {D}{ts}{N}  {D}📄 Request saved: {fname}{N}')
            except: pass

    elif 'debug_traceCallMany RESPONSE' in line:
        it = rx(r'iteration=(\d+)', line)
        utx = rx(r'user_tx=(\S+)', line)
        fname = f'/tmp/debug_trace_resp_iter{it}_{utx[:10] if utx else "unknown"}.json'
        resp_match = re.search(r'response=({.*})\s*$', line)
        if resp_match:
            try:
                resp = json.loads(resp_match.group(1))
                with open(fname, 'w') as f:
                    json.dump(resp, f, indent=2)
                result = resp.get('result', [[]])
                block = result[0] if isinstance(result, list) and result else []
                txs = block if isinstance(block, list) else [block]
                for i, tx in enumerate(txs):
                    err = tx.get('error', '')
                    ch = len(tx.get('calls', []))
                    to = tx.get('to', '?')
                    st = f'{R}REVERT{N}' if err else f'{G}OK{N}'
                    chain = L1_TAG if i == 0 else L2_TAG
                    print(f'  {D}{ts}{N}  {chain} TX{i}: {lbl(to)} {st} children={ch}')
                print(f'  {D}{ts}{N}  {D}📄 Response saved: {fname}{N}')
            except:
                pass

print()
box_end()
print()
print(f'{D}Saved trace files:{N}')
import glob
for f in sorted(glob.glob('/tmp/debug_trace_*.json')):
    print(f'  {f}')
print()
print(f'{D}Replay any request:{N}')
print(f'  curl -s -X POST $L2_RPC -H "Content-Type: application/json" -d @<file> | jq .')
PYTHON_EOF

export TARGET_FILTER TARGET_SHORT CCM
echo "$LOGS" | python3 "$RENDER_PY"
