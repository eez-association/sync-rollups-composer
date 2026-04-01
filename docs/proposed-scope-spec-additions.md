# Proposed Protocol Specification Additions: Scope Determination

Based on Q&A with protocol engineer (2026-03-31). These additions formalize the scope array
computation rules that are currently implicit in the protocol.

---

## 1. SYNC_ROLLUPS_PROTOCOL_SPEC.md — New Section D.0

**Insert before "D.1 Scope Array Semantics":**

### D.0 Scope Determination Algorithm (Builder)

Scope is **deterministic** — defined by contract structure, not by builder choice. All builders
MUST produce identical scope for the same transaction. The builder discovers scope by analyzing
the user transaction's execution trace.

#### Canonical Algorithm

For each proxy call discovered in the trace:

```
scope = accumulated_prefix ++ local_tree_path

Where:
  accumulated_prefix:  uint256[] = scope from the parent hop's execution entry
                       (empty [] for first hop)
  local_tree_path:     uint256[] = path in this hop's call tree from tx entry to proxy call
```

**`local_tree_path` computation:**

1. Obtain the `callTracer` output for this hop's execution
2. Traverse from root to the proxy call:
   - The root node (tx entry) is at depth 0
   - Each child frame (CALL, DELEGATECALL, STATICCALL) increments depth by 1
3. **Subtract 1 from the depth** — the contract that directly calls the proxy is NOT a scope level.
   It only executes; it does not "wrap" the cross-chain operation. Per §D.6: "Each level in the scope
   array corresponds to one level of the L2 call stack that **wraps** the cross-chain operation."
4. At each depth level, assign **ordinal index** (0, 1, 2, ...) among cross-chain calls in detection order
5. `local_tree_path = [ordinal_0, ordinal_1, ..., ordinal_d]` where `d` = trace_depth - 1

**Critical rule — single direct calls use scope=[]:**

When there is exactly one cross-chain call and the calling contract calls the proxy directly
(no intermediate wrappers), the scope is empty `[]`. The `_resolveScopes` function processes
the CALL directly via `_processCallAtScope` without entering any `newScope()` frame.

Scope is only non-empty when the builder needs scope navigation:
- **Multiple siblings**: scope=[0], [1], ... to route between calls
- **Deep nesting**: scope=[0,0], ... to descend through wrapper contracts
- **Return calls**: scope=[0] when a callback executes inside a scope frame

**Rules:**

1. **Direct caller excluded:** `scope_length = trace_depth - 1`. The contract that calls the proxy does NOT contribute a scope level.
2. **DELEGATECALL counts as depth:** Every frame (CALL, DELEGATECALL, STATICCALL) contributes one depth level.
3. **Ordinal is detection order:** Among cross-chain calls at a given depth, index by trace order (0, 1, 2...).
4. **Symmetric L1↔L2:** Identical rules apply on both chains.
5. **Scope accumulates across hops:** Each new hop appends to the previous hop's scope prefix.
6. **Return calls always append [0]:** Return calls detected in L1 trigger traces always use `parent_scope ++ [0]`, not trace_depth from the trigger trace (which includes protocol-internal frames).
7. **No on-chain validation:** Wrong scope causes `ExecutionNotFound`, not a specific error.

**Examples:**

| Pattern | Trace (depth) | scope_len=depth-1 | accumulated | scope |
|---------|---------------|-------------------|-------------|-------|
| Simple direct (depth 1) | User→proxy | 0 | [] | **[]** |
| 1 wrapper (depth 2) | User→A→proxy | 1 | [] | **[0]** |
| 2 wrappers (depth 3) | User→A→B→proxy | 2 | [] | **[0,0]** |
| Two siblings (depth 1) | User→proxyA, proxyB | 0 | [] | **[0], [1]** *(siblings)* |
| Deep siblings (depth 2) | User→A→proxyX, proxyY | 1 | [] | **[0,0], [0,1]** |
| Return call (any hop) | inside trigger trace | - | parent | **parent++[0]** |
| PingPong hop 1 | PingPong→proxy (d=1) | 0 | [] | **[]** |
| PingPong hop 2 (return) | inside trigger | - | [] | **[0]** |
| PingPong hop 3 (return) | inside trigger | - | [0] | **[0,0]** |

---

## 2. EXECUTION_TABLE_SPEC.md — New Section: Scope Accumulation

**Insert after "Cross-chain action hash consistency":**

### Scope Accumulation Across Hops

When a call chain crosses multiple hops (L1→L2→L1, etc.), scope accumulates deterministically.

#### Rule

```
scope_hop_N = scope_from_hop_(N-1)_entry ++ local_tree_path_hop_N
```

- `scope_from_hop_(N-1)_entry`: the `scope` field of the entry that triggered this hop ([] for first hop)
- `local_tree_path_hop_N`: path within hop N's call tree (from §D.0)
- `++`: array concatenation

#### Example: PingPong 3 hops (L2→L1→L2→L1)

```
Hop 1 (L2→L1): PingPong calls proxy at depth 0
  accumulated = [], local = [0]
  L1 entry: nextAction = CALL(PingPongL1, scope=[0])

Hop 2 (L1→L2): PingPongL1 calls proxy at depth 0
  accumulated = [0], local = [0]
  L2 entry: nextAction = CALL(PingPongL2, scope=[0,0])

Hop 3 (L2→L1): PingPongL2 calls proxy at depth 0
  accumulated = [0,0], local = [0]
  L1 entry: nextAction = CALL(PingPongL1, scope=[0,0,0])
```

#### Example: Sequential siblings + hop

```
Hop 1 (L2→L1): SCX makes 2 cross-chain calls
  Call #1: accumulated=[], local=[0] → scope=[0]
  Call #2: accumulated=[], local=[1] → scope=[1]

If call #1 triggers hop 2 (L1→L2):
  accumulated = [0], local = [0]
  → scope = [0,0]

If call #2 triggers hop 2 (L1→L2):
  accumulated = [1], local = [0]
  → scope = [1,0]
```

#### Determinism

All builders MUST produce identical accumulated scopes. The trace is deterministic, local path
computation is deterministic, and accumulated scope is fixed on-chain in the entry data.

---

## 3. docs/DERIVATION.md — New Section 2a

**Insert after section 2 (L1 Contract: Rollups.sol):**

### 2a. Scope Determination (Builder Rule)

The builder determines scope for each cross-chain call deterministically from the execution trace.
Scope is **not configurable** — it is discovered, not chosen.

#### Canonical Rule

```
scope = accumulated_prefix ++ local_tree_path
```

- `accumulated_prefix`: scope from the parent hop's entry. Empty `[]` for the first hop.
- `local_tree_path`: path from the call tree root to the proxy call in this hop's trace.

#### Local Tree Path Computation

Given a `callTracer` trace for a transaction:

1. Root trace node = depth 0. Each child in `calls[]` = depth + 1.
2. DELEGATECALL, STATICCALL count as depth levels (same as CALL).
3. When a proxy call is detected at depth `d`, the `local_tree_path` has `d` elements.
4. Each element = ordinal index of the cross-chain call among siblings at that depth (0-indexed, detection order).

#### Accumulation Across Hops

Multi-hop patterns (PingPong, flash loans) accumulate scope:
- Hop 1: `scope = [] ++ local = local`
- Hop 2: `scope = hop1_scope ++ local`
- Hop N: `scope = hop(N-1)_scope ++ local`

In the builder's iterative `debug_traceCallMany` simulation, maintain `accumulated_scope: Vec<U256>` and extend it at each hop.

#### Impact on Existing Patterns

| Pattern | Old scope (all hops) | New scope (accumulated) |
|---------|---------------------|------------------------|
| Simple L2→L1 | [0] | [0] (unchanged) |
| Siblings | [0], [1] | [0], [1] (unchanged) |
| PingPong hop 1 | [0] | [0] |
| PingPong hop 2 | [0] | [0,0] |
| PingPong hop 3 | [0] | [0,0,0] |
| Flash loan forward | [0] | [0] |
| Flash loan return | [0] | [0,0] |
| Deep nesting | [0] | [0,0] (NEW) |
