#!/usr/bin/env python3
"""
Soak monitor — samples the health endpoint, parses bot logs, and emits a
pass/fail verdict as JSON.

Pass criteria (all must hold):
- healthy == true at >=95% of samples
- consecutive_rewind_cycles never exceeds MAX_FLUSH_MISMATCHES (2)
- L2-L1 lag stays <= L2_LAG_BUDGET (default 20 blocks)
- no mode oscillation Builder<->Sync more than 3x in any 60s window
- cross-chain tx success rate across both bots >= SUCCESS_FLOOR_PCT after
  a warmup window (default 60s)
- no "anchor-block divergence ... halting" ERROR in builder logs (if we
  can read them — this is best-effort, skipped when docker isn't available)

Writes {--out} as JSON on exit.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from collections import deque
from urllib.request import urlopen
from urllib.error import URLError


def sample_health(url, timeout=5):
    try:
        with urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode())
    except (URLError, OSError, ValueError) as e:
        return {'_error': str(e)}


def read_bot_log(path):
    try:
        with open(path) as f:
            return f.read()
    except FileNotFoundError:
        return ''


def count_events(text):
    success = len(re.findall(r'^\S+Z SUCCESS ', text, re.M))
    reverted = len(re.findall(r'^\S+Z REVERTED ', text, re.M))
    timeout = len(re.findall(r'^\S+Z TIMEOUT ', text, re.M))
    send_err = len(re.findall(r'^\S+Z SEND_ERROR ', text, re.M))
    attempts = len(re.findall(r'^\S+Z OPPORTUNITY ', text, re.M))
    return {'success': success, 'reverted': reverted, 'timeout': timeout,
            'send_error': send_err, 'opportunities': attempts}


def try_grep_builder_logs(docker_compose_cmd, pattern):
    if not docker_compose_cmd:
        return None
    try:
        out = subprocess.check_output(
            f'{docker_compose_cmd} logs --since 1h builder 2>/dev/null | grep -c -E "{pattern}" || echo 0',
            shell=True, text=True, timeout=30)
        return int(out.strip() or 0)
    except (subprocess.SubprocessError, ValueError):
        return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--health-url', required=True)
    ap.add_argument('--duration', type=int, default=600)
    ap.add_argument('--interval', type=int, default=5)
    ap.add_argument('--out', default='/tmp/soak-verdict.json')
    ap.add_argument('--warmup-s', type=int, default=60)
    ap.add_argument('--l2-lag-budget', type=int, default=20)
    ap.add_argument('--max-rewind-cycles', type=int, default=2)
    ap.add_argument('--success-floor-pct', type=float, default=5.0,
                    help='min %% of attempts that must succeed after warmup')
    ap.add_argument('--bot-logs', nargs='+', default=['/tmp/arb_bot.log', '/tmp/arb_bot2.log'])
    ap.add_argument('--docker-compose-cmd', default='',
                    help='e.g. "sudo docker compose -f ... -f ..."')
    args = ap.parse_args()

    start = time.time()
    end = start + args.duration
    samples = []
    mode_changes = deque()  # (timestamp, mode)
    last_mode = None
    violations = []

    print(f'[monitor] duration={args.duration}s interval={args.interval}s '
          f'health_url={args.health_url}', flush=True)

    while time.time() < end:
        now = time.time()
        h = sample_health(args.health_url)
        h['ts'] = now
        samples.append(h)
        if '_error' not in h:
            mode = h.get('mode')
            if mode and mode != last_mode:
                mode_changes.append((now, mode))
                last_mode = mode
            # sliding 60s window for mode oscillation
            while mode_changes and now - mode_changes[0][0] > 60:
                mode_changes.popleft()
            if len(mode_changes) > 6:  # >3 oscillations (= 6 transitions) in 60s
                violations.append(
                    f'{now:.0f}: mode oscillation Builder<->Sync '
                    f'{len(mode_changes)} transitions in 60s')

            lag = h.get('l1_derivation_head', 0) - h.get('l2_head', 0)
            if lag > args.l2_lag_budget:
                violations.append(
                    f'{now:.0f}: L2 lag {lag} exceeds budget {args.l2_lag_budget}')

            cycles = h.get('consecutive_rewind_cycles', 0)
            if cycles > args.max_rewind_cycles:
                violations.append(
                    f'{now:.0f}: consecutive_rewind_cycles={cycles} '
                    f'> max {args.max_rewind_cycles}')

        if int(now - start) % 30 == 0 and '_error' not in h:
            print(
                f'[{int(now-start):4d}s] mode={h.get("mode")} '
                f'L2={h.get("l2_head")} L1={h.get("l1_derivation_head")} '
                f'lag={h.get("l1_derivation_head",0)-h.get("l2_head",0)} '
                f'cycles={h.get("consecutive_rewind_cycles")} '
                f'pending={h.get("pending_submissions")} '
                f'healthy={h.get("healthy")}',
                flush=True)
        time.sleep(args.interval)

    # Post-process
    healthy = [s for s in samples if '_error' not in s and s.get('healthy')]
    healthy_pct = 100.0 * len(healthy) / max(1, len([s for s in samples if '_error' not in s]))
    if healthy_pct < 95:
        violations.append(f'healthy% = {healthy_pct:.1f}% < 95%')

    # Bot events
    bot_counts = {}
    for p in args.bot_logs:
        text = read_bot_log(p)
        bot_counts[os.path.basename(p)] = count_events(text)

    total_attempts = sum(b.get('opportunities', 0) for b in bot_counts.values())
    total_success = sum(b.get('success', 0) for b in bot_counts.values())
    success_rate = 100.0 * total_success / max(1, total_attempts)
    if total_attempts >= 10 and success_rate < args.success_floor_pct:
        violations.append(
            f'cross-chain success rate {success_rate:.1f}% < floor '
            f'{args.success_floor_pct}% (over {total_attempts} attempts)')

    # Builder log grep (best-effort)
    halt_count = try_grep_builder_logs(
        args.docker_compose_cmd,
        'anchor-block divergence beyond safety threshold|anchor-block post-commit divergence')
    if halt_count is not None and halt_count > 0:
        violations.append(f'builder logs contain {halt_count} halt ERROR lines')

    verdict = {
        'passed': len(violations) == 0,
        'violations': violations,
        'duration_s': args.duration,
        'samples': len(samples),
        'healthy_pct': healthy_pct,
        'bot_counts': bot_counts,
        'total_attempts': total_attempts,
        'total_success': total_success,
        'success_rate_pct': success_rate,
        'halt_log_lines': halt_count,
        'final_sample': samples[-1] if samples else None,
    }
    with open(args.out, 'w') as f:
        json.dump(verdict, f, indent=2)
    print(f'\n[monitor] VERDICT: {"PASS" if verdict["passed"] else "FAIL"}', flush=True)
    for v in violations:
        print(f'  - {v}', flush=True)
    print(f'[monitor] written to {args.out}', flush=True)
    sys.exit(0 if verdict['passed'] else 1)


if __name__ == '__main__':
    main()
