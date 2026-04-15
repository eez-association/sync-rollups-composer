#!/usr/bin/env python3
"""
Dashboard for the cross-chain arb bot.

Serves a simple HTML page that shows:
- Live L1 and L2 AMM prices + reserves
- Spread between them
- Bot stats (iterations, opportunities, successes, total profit)
- Recent bot log lines

Run: python3 scripts/arb_dashboard.py
Then open: http://localhost:8765/
"""

import json
import os
import re
from http.server import BaseHTTPRequestHandler, HTTPServer
from web3 import Web3

CONFIG_PATHS = ['/tmp/arb_config.json', '/tmp/arb_config_2.json']
TRADER_LOG = '/tmp/trader.log'
TRADER_PID = '/tmp/trader.pid'
PORT = 8765

BOTS = []
for p in CONFIG_PATHS:
    if not os.path.exists(p):
        continue
    with open(p) as f:
        c = json.load(f)
    for k in ['arb_contract', 'weth_l1', 'usdc_l1', 'amm_l1', 'amm_l2']:
        c[k] = Web3.to_checksum_address(c[k])
    BOTS.append(c)

CFG = BOTS[0]  # first bot holds shared addresses for the AMMs

w3_l1 = Web3(Web3.HTTPProvider(CFG['l1_rpc']))
w3_l2 = Web3(Web3.HTTPProvider(CFG['l2_rpc']))

AMM_ABI = [
    {"inputs": [], "name": "reserveA", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
    {"inputs": [], "name": "reserveB", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
]
ERC20_ABI = [
    {"inputs": [{"type": "address"}], "name": "balanceOf", "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
]

amm_l1 = w3_l1.eth.contract(address=CFG['amm_l1'], abi=AMM_ABI)
amm_l2 = w3_l2.eth.contract(address=CFG['amm_l2'], abi=AMM_ABI)
weth_l1 = w3_l1.eth.contract(address=CFG['weth_l1'], abi=ERC20_ABI)
usdc_l1 = w3_l1.eth.contract(address=CFG['usdc_l1'], abi=ERC20_ABI)


def get_state():
    l1a = amm_l1.functions.reserveA().call()
    l1b = amm_l1.functions.reserveB().call()
    l2a = amm_l2.functions.reserveA().call()
    l2b = amm_l2.functions.reserveB().call()

    p_l1 = (l1b / 1e6) / (l1a / 1e18) if l1a else 0
    p_l2 = (l2b / 1e6) / (l2a / 1e18) if l2a else 0
    spread = (p_l1 - p_l2) / ((p_l1 + p_l2) / 2) * 100 if (p_l1 + p_l2) else 0

    bots = []
    for cfg in BOTS:
        weth_bal = weth_l1.functions.balanceOf(cfg['arb_contract']).call()
        usdc_bal = usdc_l1.functions.balanceOf(cfg['arb_contract']).call()
        stats = parse_log(cfg)
        bots.append({
            'name': cfg.get('bot_name', 'bot'),
            'address': cfg['arb_contract'],
            'weth_balance': weth_bal / 1e18,
            'usdc_balance': usdc_bal / 1e6,
            **stats,
        })

    trader = parse_trader_log()

    return {
        'l1': {'weth': l1a / 1e18, 'usdc': l1b / 1e6, 'price': p_l1},
        'l2': {'weth': l2a / 1e18, 'usdc': l2b / 1e6, 'price': p_l2},
        'spread_pct': spread,
        'bots': bots,
        'trader': trader,
    }


def parse_trader_log():
    out = {'running': False, 'lines': [], 'trades': 0}
    if not os.path.exists(TRADER_PID):
        return out
    try:
        with open(TRADER_PID) as f:
            pid = int(f.read().strip())
        os.kill(pid, 0)
        out['running'] = True
    except (ProcessLookupError, ValueError):
        return out
    if os.path.exists(TRADER_LOG):
        with open(TRADER_LOG) as f:
            lines = f.readlines()
        out['lines'] = [l.rstrip() for l in lines[-20:]]
        out['trades'] = sum(1 for l in lines if ' TRADE: ' in l)
    return out


def parse_log(cfg):
    log_path = cfg.get('log_file', '/tmp/arb_bot.log')
    if not os.path.exists(log_path):
        return {'running': False, 'lines': []}

    with open(log_path) as f:
        lines = f.readlines()

    # Keep last 30 lines for display
    recent = [l.rstrip() for l in lines[-30:]]

    # Parse stats from the latest STATS line
    stats = {
        'iterations': 0,
        'opportunities': 0,
        'successes': 0,
        'failures': 0,
        'profit_weth': 0,
        'gas_used': 0,
        'elapsed_s': 0,
        'last_opportunity': None,
        'last_success': None,
        'last_success_profit': 0,
        'last_success_tx': None,
    }

    stats_re = re.compile(
        r'STATS: iter=(\d+) opps=(\d+) ok=(\d+) fail=(\d+) profit=([\d.]+) WETH gas=(\d+) elapsed=(\d+)'
    )
    success_re = re.compile(
        r'(\S+Z) SUCCESS: tx=(\S+) gas=(\d+) profit=([\d.]+) WETH \(\$([\d.]+)\)'
    )
    opp_re = re.compile(r'(\S+Z) OPPORTUNITY: dir=(\S+) amount=([\d.]+) WETH expected_profit=([\d.]+) USD')
    heartbeat_re = re.compile(
        r'(\S+Z) \[(\d+)\] L1=([\d.]+) L2=([\d.]+) spread=([+-][\d.]+)% best=([+-][\d.]+) USD \((\S+)\)'
    )

    for line in lines:
        m = stats_re.search(line)
        if m:
            stats['iterations'] = int(m.group(1))
            stats['opportunities'] = int(m.group(2))
            stats['successes'] = int(m.group(3))
            stats['failures'] = int(m.group(4))
            stats['profit_weth'] = float(m.group(5))
            stats['gas_used'] = int(m.group(6))
            stats['elapsed_s'] = int(m.group(7))

        m = success_re.search(line)
        if m:
            stats['last_success'] = m.group(1)
            stats['last_success_tx'] = m.group(2)
            stats['last_success_profit'] = float(m.group(5))

        m = opp_re.search(line)
        if m:
            stats['last_opportunity'] = m.group(1)

        m = heartbeat_re.search(line)
        if m:
            stats['last_heartbeat'] = m.group(1)
            stats['last_heartbeat_iter'] = int(m.group(2))
            stats['current_best_usd'] = float(m.group(6))
            stats['current_best_dir'] = m.group(7)

    heartbeat_file = cfg.get('heartbeat_file', '/tmp/arb_bot_heartbeat.log')
    if os.path.exists(heartbeat_file):
        with open(heartbeat_file) as f:
            hb_line = f.read().strip()
        m = heartbeat_re.search(hb_line)
        if m:
            stats['last_heartbeat'] = m.group(1)
            stats['last_heartbeat_iter'] = int(m.group(2))
            stats['current_best_usd'] = float(m.group(6))
            stats['current_best_dir'] = m.group(7)

    stats['lines'] = recent
    pid_file = cfg.get('pid_file', '/tmp/arb_bot.pid')
    if os.path.exists(pid_file):
        try:
            with open(pid_file) as f:
                pid = int(f.read().strip())
            os.kill(pid, 0)
            stats['running'] = True
        except (ProcessLookupError, ValueError):
            stats['running'] = False
    else:
        stats['running'] = False
    return stats


HTML = r"""<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Cross-Chain Arb Dashboard</title>
<style>
  body { font-family: -apple-system, system-ui, sans-serif; background: #0a0a0a; color: #e0e0e0; margin: 0; padding: 20px; }
  h1 { margin: 0 0 20px; font-size: 20px; color: #6cf; }
  .grid { display: grid; grid-template-columns: 1fr 1fr 1fr; gap: 16px; margin-bottom: 16px; }
  .card { background: #151515; border: 1px solid #2a2a2a; border-radius: 8px; padding: 16px; }
  .card h2 { margin: 0 0 12px; font-size: 13px; color: #888; text-transform: uppercase; letter-spacing: 0.05em; }
  .big { font-size: 28px; font-weight: 600; color: #fff; }
  .sub { color: #888; font-size: 12px; margin-top: 4px; }
  .pos { color: #4ade80; }
  .neg { color: #f87171; }
  .muted { color: #666; }
  table { width: 100%; border-collapse: collapse; font-size: 12px; }
  table td { padding: 4px 8px; border-bottom: 1px solid #1f1f1f; }
  table td:first-child { color: #888; }
  table td:last-child { text-align: right; font-family: ui-monospace, monospace; color: #fff; }
  .log { background: #0a0a0a; border: 1px solid #2a2a2a; border-radius: 8px; padding: 12px; font-family: ui-monospace, monospace; font-size: 11px; line-height: 1.5; max-height: 420px; overflow-y: auto; }
  .log-opp { color: #fbbf24; }
  .log-ok { color: #4ade80; }
  .log-err { color: #f87171; }
  .log-stat { color: #6cf; }
  .log-ts { color: #555; }
  .status { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 6px; vertical-align: middle; }
  .status-ok { background: #4ade80; box-shadow: 0 0 6px #4ade80; }
  .status-bad { background: #f87171; }
  .spread-bar { height: 6px; background: #1f1f1f; border-radius: 3px; overflow: hidden; margin-top: 8px; }
  .spread-fill { height: 100%; background: linear-gradient(90deg, #4ade80, #fbbf24, #f87171); }
  .muted-addr { font-family: ui-monospace, monospace; font-size: 10px; color: #555; }
</style>
</head>
<body>

<h1>◆ Cross-Chain Arb Dashboard</h1>

<div class="grid">
  <div class="card">
    <h2>L1 AMM</h2>
    <div class="big" id="l1-price">—</div>
    <div class="sub">USDC / WETH</div>
    <table style="margin-top: 12px">
      <tr><td>WETH</td><td id="l1-weth">—</td></tr>
      <tr><td>USDC</td><td id="l1-usdc">—</td></tr>
    </table>
  </div>

  <div class="card">
    <h2>L2 AMM</h2>
    <div class="big" id="l2-price">—</div>
    <div class="sub">USDC / WETH</div>
    <table style="margin-top: 12px">
      <tr><td>WETH</td><td id="l2-weth">—</td></tr>
      <tr><td>USDC</td><td id="l2-usdc">—</td></tr>
    </table>
  </div>

  <div class="card">
    <h2>Spread</h2>
    <div class="big" id="spread">—</div>
    <div class="sub" id="spread-dir">—</div>
    <div class="spread-bar" style="margin-top: 16px"><div class="spread-fill" id="spread-bar" style="width: 0%"></div></div>
  </div>
</div>

<div class="grid" id="bots-grid"></div>

<div class="grid" style="grid-template-columns: 1fr 1fr">
  <div class="card">
    <h2>Bot 1 Log</h2>
    <div class="log" id="log-bot1">loading…</div>
  </div>
  <div class="card">
    <h2>Bot 2 Log</h2>
    <div class="log" id="log-bot2">loading…</div>
  </div>
</div>

<div class="card">
  <h2>Trader <span id="trader-state" style="font-size: 11px; margin-left: 10px"></span></h2>
  <div class="log" id="log-trader" style="max-height: 200px">loading…</div>
</div>

<script>
function fmt(n, dp) {
  if (n === null || n === undefined) return '—';
  return n.toLocaleString('en-US', {minimumFractionDigits: dp, maximumFractionDigits: dp});
}

function fmtElapsed(s) {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h) return `${h}h ${m}m`;
  if (m) return `${m}m ${sec}s`;
  return `${sec}s`;
}

function colorizeLog(line) {
  const m = line.match(/^(\S+Z) (.*)$/);
  if (!m) return line;
  const ts = m[1], rest = m[2];
  let cls = '';
  if (rest.startsWith('OPPORTUNITY')) cls = 'log-opp';
  else if (rest.startsWith('SUCCESS')) cls = 'log-ok';
  else if (rest.startsWith('TRADE:')) cls = 'log-opp';
  else if (rest.startsWith('ERROR') || rest.startsWith('REVERTED') || rest.startsWith('SEND ERROR') || rest.startsWith('skip')) cls = 'log-err';
  else if (rest.startsWith('STATS')) cls = 'log-stat';
  return `<span class="log-ts">${ts}</span> <span class="${cls}">${rest.replace(/</g, '&lt;')}</span>`;
}

async function refresh() {
  try {
    const r = await fetch('/api/state');
    const s = await r.json();

    document.getElementById('l1-price').textContent = fmt(s.l1.price, 2);
    document.getElementById('l1-weth').textContent = fmt(s.l1.weth, 4);
    document.getElementById('l1-usdc').textContent = fmt(s.l1.usdc, 2);

    document.getElementById('l2-price').textContent = fmt(s.l2.price, 2);
    document.getElementById('l2-weth').textContent = fmt(s.l2.weth, 4);
    document.getElementById('l2-usdc').textContent = fmt(s.l2.usdc, 2);

    const spread = s.spread_pct;
    const spreadEl = document.getElementById('spread');
    spreadEl.textContent = (spread >= 0 ? '+' : '') + spread.toFixed(3) + '%';
    spreadEl.className = 'big ' + (Math.abs(spread) > 0.5 ? 'pos' : 'muted');
    document.getElementById('spread-dir').textContent =
      Math.abs(spread) < 0.05 ? 'balanced' :
      spread > 0 ? 'L1 expensive, L2 cheap' : 'L2 expensive, L1 cheap';
    document.getElementById('spread-bar').style.width = Math.min(100, Math.abs(spread) * 20) + '%';

    // Render bots
    const bots = s.bots || [];
    const midPrice = (s.l1.price + s.l2.price) / 2;
    const grid = document.getElementById('bots-grid');
    grid.innerHTML = '';
    grid.style.gridTemplateColumns = `repeat(${bots.length}, 1fr)`;

    bots.forEach((b, i) => {
      const running = b.running;
      const currentBest = b.current_best_usd || 0;
      const stateText = !running ? 'STOPPED'
        : currentBest >= 1 ? 'FIRING' : 'MONITORING';
      const verdict = !running ? 'bot process not found'
        : currentBest >= 1 ? `will fire: ${b.current_best_dir || '?'} direction`
        : currentBest > 0 ? `best: +$${currentBest.toFixed(2)} (< $1 threshold)`
        : `no arb (fees ~0.6%, spread ${Math.abs(s.spread_pct).toFixed(2)}%)`;
      const profitWeth = b.profit_weth || 0;
      const profitUsd = profitWeth * midPrice;

      const card = document.createElement('div');
      card.className = 'card';
      card.innerHTML = `
        <h2>${b.name.toUpperCase()} <span style="float:right"><span class="status ${running ? 'status-ok' : 'status-bad'}"></span>${stateText}</span></h2>
        <div class="big ${profitWeth > 0 ? 'pos' : ''}">${profitWeth >= 0 ? '+' : ''}${fmt(profitWeth, 6)} WETH</div>
        <div class="sub">≈ $${fmt(profitUsd, 2)} · ${verdict}</div>
        <table style="margin-top: 12px">
          <tr><td>Capital</td><td>${fmt(b.weth_balance, 6)} WETH</td></tr>
          <tr><td>Current best</td><td class="${currentBest >= 1 ? 'pos' : ''}">${currentBest > 0 ? '+$' + currentBest.toFixed(2) + ' (' + (b.current_best_dir || '?') + ')' : '$0.00'}</td></tr>
          <tr><td>Successes</td><td>${fmt(b.successes || 0, 0)}</td></tr>
          <tr><td>Failures</td><td class="${(b.failures||0)>0?'neg':''}">${fmt(b.failures || 0, 0)}</td></tr>
          <tr><td>Iterations</td><td>${fmt(b.iterations || 0, 0)}</td></tr>
          <tr><td>Gas spent</td><td>${fmt(b.gas_used || 0, 0)}</td></tr>
          <tr><td>Elapsed</td><td>${fmtElapsed(b.elapsed_s || 0)}</td></tr>
          <tr><td>Contract</td><td><span class="muted-addr">${b.address}</span></td></tr>
        </table>
      `;
      grid.appendChild(card);
    });

    // Render bot logs
    bots.forEach((b, i) => {
      const el = document.getElementById(`log-${b.name}`);
      if (el) el.innerHTML = (b.lines || []).map(colorizeLog).reverse().join('<br>');
    });

    // Render trader
    const trader = s.trader || {};
    const tEl = document.getElementById('trader-state');
    if (tEl) {
      tEl.innerHTML = `<span class="status ${trader.running ? 'status-ok' : 'status-bad'}"></span>${trader.running ? 'RUNNING' : 'STOPPED'} · ${trader.trades || 0} trades`;
    }
    const tLog = document.getElementById('log-trader');
    if (tLog) tLog.innerHTML = (trader.lines || []).map(colorizeLog).reverse().join('<br>');
  } catch (e) {
    console.error(e);
  }
}

refresh();
setInterval(refresh, 3000);
</script>

</body>
</html>
"""


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass  # silence default access log

    def do_GET(self):
        if self.path == '/' or self.path.startswith('/?'):
            self.send_response(200)
            self.send_header('Content-Type', 'text/html; charset=utf-8')
            self.end_headers()
            self.wfile.write(HTML.encode('utf-8'))
        elif self.path == '/api/state':
            try:
                data = get_state()
                body = json.dumps(data).encode('utf-8')
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Access-Control-Allow-Origin', '*')
                self.end_headers()
                self.wfile.write(body)
            except Exception as e:
                self.send_response(500)
                self.send_header('Content-Type', 'application/json')
                self.end_headers()
                self.wfile.write(json.dumps({'error': str(e)}).encode('utf-8'))
        else:
            self.send_response(404)
            self.end_headers()


def main():
    print(f'Dashboard: http://localhost:{PORT}/')
    HTTPServer(('0.0.0.0', PORT), Handler).serve_forever()


if __name__ == '__main__':
    main()
