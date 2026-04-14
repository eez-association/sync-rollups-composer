# Trace Fixtures

Reentrant `callTracer` JSON traces used by composer_rpc tests and (in
Phase 5) by `proptest`/fuzz harnesses. Each fixture is the smallest valid
trace tree that exercises one canonical cross-chain pattern.

Status: scaffolding for refactor PLAN step 0.6. These files are committed
to the repo so future tests can rely on stable inputs without
regenerating them at test time. Loaded via
[`crate::test_support::trace_fixtures`].

## Fixtures

| File | Pattern |
|---|---|
| `deposit_simple_l1_to_l2.json` | User → L1 proxy → CCM_L1 (single executeCrossChainCall) |
| `withdrawal_simple_l2_to_l1.json` | User → Bridge_L2 → CCM_L2 (single executeCrossChainCall) |
| `flash_loan_3_call_l1_to_l2.json` | User → executor → 3 sibling executeCrossChainCall children |
| `ping_pong_depth_2_l2_to_l1.json` | 2 sibling roots, second root has a child cross-chain call |
| `ping_pong_depth_3_l2_to_l1.json` | Linear chain: root → child → grandchild (3 nested executeCrossChainCall) |
| `top_level_revert.json` | Trace where the root call reverts (`error: "execution reverted"`) |
| `child_continuation.json` | 2 sibling cross-chain calls modelling a continuation pair |
| `multi_call_call_twice.json` | User → CallTwice contract → 2 cross-chain calls with different params |

## Format

Each file is a JSON object representing a single `callTracer` trace
node:

```json
{
  "from": "0x...",
  "to": "0x...",
  "input": "0x...",
  "value": "0x0",
  "type": "CALL",
  "gas": "0x...",
  "gasUsed": "0x...",
  "output": "0x",
  "calls": [ ... ]
}
```

The `calls` array contains zero or more child trace nodes. Optional
fields like `error` mark reverted frames.

The `executeCrossChainCall(address,bytes)` selector is `0x9af53259`.
Calldata in the fixtures is real ABI-encoded calldata for that
function with `sourceAddress = 0x...aa00` and `callData = 0x` (empty
inner data) — sufficient for `walk_trace_tree` selector matching.

## Adding a fixture

1. Drop a new `.json` file in this directory.
2. Register it in `crate::test_support::trace_fixtures::all_fixtures()`.
3. Run `cargo nextest run -p based-rollup composer_rpc::` — every
   round-trip test in `composer_rpc/{l1_to_l2,l2_to_l1}_tests.rs`
   automatically picks it up.

## What these fixtures do NOT verify

These are SCAFFOLDING. They guarantee:
- Each file is valid JSON.
- Each file deserializes to a `serde_json::Value`.
- Each top-level node has the required fields (`from`, `to`, `calls`).

Deeper assertions (correct `walk_trace_tree` output, scope navigation,
selector extraction) live in Phase 5 of the refactor when the typed
`TraceNode`/`TraceCallFrame` structs land. See
`docs/refactor/PLAN.md` step 4.5.
