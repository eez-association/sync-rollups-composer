# arb-soak — cross-chain contention soak test

Adapted from [PR #33](https://github.com/eez-association/sync-rollups-composer/pull/33)
(koeppelmann's `arb-bot-reproducer`). Deterministically reproduces the
L2-rewind-loop / monotonic-lag symptoms tracked in issue #35.

## Stack

- `contracts/test-multi-call/src/CrossChainArb.sol` — atomic cross-chain
  arbitrage contract; holds WETH working capital, sends `arbSellOnL1` /
  `arbSellOnL2` that each fan out into bridge + cross-chain call.
- Two `CrossChainArb` instances race for the same opportunity — this is
  the contention pattern that drove issue #35.
- One random trader (`trader.py`) creates price moves on L1 and L2 AMMs
  so arb opportunities exist.
- `monitor.py` samples the health endpoint + parses bot logs and produces
  a pass/fail verdict.

## Pass criteria (all must hold for the run's duration)

- `healthy == true` at ≥95% of samples
- `consecutive_rewind_cycles` never exceeds `MAX_FLUSH_MISMATCHES = 2`
- L2 ↔ L1-derivation-head lag stays ≤ 20 blocks
- Builder/Sync mode does not oscillate more than 3 times in any 60s window
- Cross-chain tx success rate ≥ 5% after a 60s warmup (once 10+ attempts
  have happened)
- No "anchor-block divergence … halting" ERROR lines in builder logs

Any single violation → FAIL.

## Running

### Devnet (default ports 11555/11556/11545/11560)

```bash
HEALTH_URL=http://localhost:11560/health \
L1_RPC=http://localhost:11555 \
L1_PROXY=http://localhost:11556 \
L2_RPC=http://localhost:11545 \
SOAK_DURATION=1800 \
./scripts/e2e/arb-soak/run.sh
```

### Testnet (default ports 9555/9556/9545/9560)

```bash
SOAK_DURATION=1800 ./scripts/e2e/arb-soak/run.sh
```

### Skipping setup on re-runs

```bash
SOAK_SKIP_SETUP=1 SOAK_DURATION=3600 ./scripts/e2e/arb-soak/run.sh
```

Reuses the previously deployed `CrossChainArb` + AMMs from
`/tmp/arb_config{,_2}.json`.

## Files produced

| path | purpose |
|---|---|
| `/tmp/arb_config.json`, `/tmp/arb_config_2.json` | per-bot deploy config |
| `/tmp/arb_bot.log`, `/tmp/arb_bot2.log` | per-bot attempt log |
| `/tmp/arb_bot_heartbeat.log` (etc.) | latest-state heartbeat per bot |
| `/tmp/trader.log` | trader activity |
| `/tmp/soak-verdict.json` | final JSON verdict (pass/fail + metrics) |

## Expected outcomes by image version

| image commit | expected verdict at 30 min |
|---|---|
| pre-PR#38 (sibling-reorg) | FAIL — monotonic lag + rewind cycles |
| PR#38 only | FAIL at the L1→L2 zero-consumption path (~90 min) |
| PR#39 | PASS if Fix 1 + Fix 2 cover all observed divergences |
| PR#39 + Gap-1/2/3 extension | target PASS for 3h+ |

## Non-goals

- This is **not** a pure-Bash test. The arb-simulation math and the random
  trader are Python. Same split PR #33 uses — proven to reproduce.
- No dashboard. Use `tail -f /tmp/arb_bot*.log` for live view.
