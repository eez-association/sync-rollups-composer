# Consensus & Derivation Specification

> **This document is normative.** Any code change that violates these rules is a bug.
> The implementation must conform to this spec, not the other way around.

## Foundational Principle

**L1 is the canonical source of truth.** This is a based rollup — L1 is the sequencer. Any node can re-derive the full L2 chain from L1 events alone, without trusting any other party. There is no privileged sequencer. The builder is a convenience, not an authority.

---

## 1. Block Timing

L2 blocks follow a deterministic timestamp schedule:

```
l2_timestamp = deployment_timestamp + ((l2_block_number + 1) × block_time)
```

- `block_time` = 12 seconds
- `deployment_timestamp` is set at rollup creation (recorded in genesis)
- L2 block numbers are **explicit** (submitted to L1), not derived from L1 block numbers
- Block 0 is genesis (never submitted)
- First submitted block is block 1

**Consensus rule**: A block is invalid if its timestamp ≠ `deployment_timestamp + ((block_number + 1) × 12)`.

**Builder target rule**: The builder derives the target L2 block deterministically from the L1 head: `target = l1_head - deployment_l1_block`. No wall-clock dependency. The last block built has a timestamp 12 s ahead of the builder's wall clock — this is correct because it matches the *next* L1 block, where `postBatch` lands.

**Rationale for `+1`**: L2 block K has the timestamp of L1 block `deployment_l1_block + K + 1`. The builder sees L1 head = N and builds up to L2 block `N - deployment_l1_block`, whose timestamp equals L1 block N + 1 — exactly the block that will include the `postBatch` transaction.

---

## 2. L1 Contract: Rollups.sol

A single L1 contract (`Rollups.sol`) is the sole interface between L1 and L2:

- **`postBatch(entries, blobCount, callData, proof)`** — the only submission function
- **`BatchPosted(entries, publicInputsHash)`** — the only event used for derivation
- **`rollups(rollupId)`** — view returning `(owner, verificationKey, stateRoot, etherBalance)`

There is no Inbox contract, no BlockSubmitted event, no Deposited event. Everything goes through `postBatch`.

**Proof verification**: The `proof` parameter carries a 65-byte ECDSA signature of `publicInputsHash`. The builder signs `publicInputsHash` (computed identically to Rollups.sol's on-chain computation) using its private key. On L1, `tmpECDSAVerifier` recovers the signer from the signature and verifies it matches the rollup's registered verification key. The builder targets the **next** L1 block (`latest + 1`) when computing `publicInputsHash`; if the tx lands in a later block, the hash changes, the signature fails, and the existing retry logic re-attempts.

**`publicInputsHash` computation**: `keccak256(abi.encodePacked(blockhash(block.number - 1), block.timestamp, abi.encode(entryHashes), abi.encode(blobHashes), keccak256(callData)))`. Note the second field is `block.timestamp` (not `block.number`). The builder predicts this as `latest_timestamp + block_time`. `blobHashes` is always empty (blobCount = 0). `entryHashes` is the array of per-entry hashes from the batch.

**`computeCrossChainProxyAddress(address, uint256)`**: Takes two arguments — `originalAddress` and `originalRollupId`. No `domain` parameter. Used by the builder to predict proxy addresses for trigger transactions without deploying them first.

---

## 3. Data Encoding

### 3a. Block data in `callData`

L2 block data is encoded in the `postBatch` callData parameter:

```
abi.encode(uint256[] l2BlockNumbers, bytes[] transactions)
```

- `l2BlockNumbers[i]` is the explicit L2 block number for block i
- `transactions[i]` is the RLP-encoded transaction list for block i (empty bytes for protocol-only blocks with no user transactions)
- ALL blocks are submitted to L1, including protocol-only blocks — this avoids gap-fill complexity and ensures deterministic L1 context across builder/fullnodes. Empty blocks add only callData cost (block number + empty tx bytes).
- Empty callData (length 0) means no block data (cross-chain-only batch)

### 3b. Execution entries for blocks

Each L2 block in a batch maps to one **immediate** execution entry:

```
ExecutionEntry {
    stateDeltas: [StateDelta {
        rollupId:     ROLLUP_ID,
        currentState: pre_state_root,   // state root BEFORE this block
        newState:     post_state_root,  // state root AFTER this block
        etherDelta:   0
    }],
    actionHash: bytes32(0),   // immediate — applied during postBatch()
    nextAction: Action { ... } // unused but required by struct
}
```

**Important**: The immediate entry's `newState` is the **clean** state root — the block executed WITHOUT cross-chain entry effects. Cross-chain effects are encoded as chained state deltas on deferred entries (see §3e). The on-chain `stateRoot` after `postBatch` equals the clean block state root; it evolves further only when deferred entries are consumed.

### 3c. State root chaining

Rollups.sol enforces: `entry.stateDeltas[0].currentState == rollups[rollupId].stateRoot`

This means:
- Block 1's `currentState` must match the on-chain state root
- Block 1's `newState` becomes the new on-chain state root
- Block 2's `currentState` must match block 1's `newState`
- And so on — a strict chain

This provides **implicit ordering** without a `nextL2Block` counter.

### 3d. Combined batches

A single `postBatch()` call contains:
- Entries `[0]`: single aggregate immediate entry for all L2 blocks — `StateDelta(first_pre_state → clean_state_root)`, where `clean_state_root` is the final block's state root WITHOUT cross-chain entry effects
- Entries `[1..M)`: deferred entries for cross-chain execution (`actionHash ≠ 0`), each carrying a chained `StateDelta` (see §3e). For simple deposits, these are non-nested L1-format entries (`actionHash=hash(CALL), nextAction=RESULT`). For withdrawals, entries use nested format (§13c). For multi-call continuations, entries may use extended structures with `nextAction=CALL{scope=[0]}` for scope navigation (§14).
- `callData`: encoded block data
- `proof`: ECDSA signature of `publicInputsHash` (see §2)

**Constraint**: `lastStateUpdateBlock` allows only ONE `postBatch()` per L1 block per rollup.

### 3e. Cross-chain entry state deltas (unified intermediate roots)

Deferred cross-chain entries carry **chained state deltas** that encode each entry's effect on the L2 state root. When a block has D deposit entries and W withdrawal entries, the builder computes D+W+1 intermediate state roots in a single unified chain:

```
R(0,0) = block state WITHOUT any deposit or withdrawal entry txs (clean — used in immediate entry)
R(1,0) = block state with 1st deposit applied
...
R(D,0) = block state with all D deposits, no withdrawals
R(D,1) = block state with all D deposits + 1st withdrawal
...
R(D,W) = block state with all deposits + all withdrawals (speculative — builder's local state)
```

Deposits precede withdrawals in the chain because `executeRemoteCall` txs appear before L2-to-L1 cross-chain txs in block execution order.

**Note**: The clean root R(0,0) may include effects of L1-fetched entries (incoming cross-chain calls from other rollups that are already consumed on L1). These canonical entries are always present in every prefix and do not need chained deltas. Only speculative (RPC-originated) deposit entries and withdrawal entries get chained deltas for L1 submission.

The builder internally maintains **L2-format pairs** (CALL trigger + RESULT table entry) for local block execution. Before L1 submission, deposit pairs are converted to **L1-format entries** (non-nested): `actionHash=hash(CALL), nextAction=RESULT`. The CALL entry's state delta is carried into the L1 entry. This prevents Rollups.sol from entering `newScope()` for simple calls. Withdrawal entries use **nested** format (see §13c). Multi-call continuations (flash loans) use extended entry structures with scope navigation (see §14).

**Deposit entries** carry the state delta from the corresponding CALL:

```
D1: actionHash=hash(CALL₁), nextAction=RESULT₁, StateDelta(rollupId, R(0,0), R(1,0), etherDelta=+call_value₁)
D2: actionHash=hash(CALL₂), nextAction=RESULT₂, StateDelta(rollupId, R(1,0), R(2,0), etherDelta=+call_value₂)
...
DD: actionHash=hash(CALLd), nextAction=RESULTd, StateDelta(rollupId, R(D-1,0), R(D,0), etherDelta=+call_valueD)
```

Each deposit entry's `etherDelta` equals the `call_value` (i.e., `msg.value`) of the corresponding CALL action. For ETH deposit entries (e.g., `Bridge.bridgeEther` on L1), `etherDelta` is positive and equals the deposited ETH amount. For zero-value cross-chain calls, `etherDelta` is 0. This must match the `_etherDelta` accumulated by `Rollups.sol` during `executeCrossChainCall`; a mismatch causes `postBatch` to revert with `EtherDeltaMismatch`.

**Withdrawal entries** continue the chain from R(D,0):

```
W1_CALL:   StateDelta(rollupId, R(D,0),   R(D,1), etherDelta=0)
W1_RESULT: StateDelta(rollupId, R(D,1),   R(D,1), etherDelta=-amount₁)
W2_CALL:   StateDelta(rollupId, R(D,1),   R(D,2), etherDelta=0)
W2_RESULT: StateDelta(rollupId, R(D,2),   R(D,2), etherDelta=-amount₂)
...
WW_CALL:   StateDelta(rollupId, R(D,W-1), R(D,W), etherDelta=0)
WW_RESULT: StateDelta(rollupId, R(D,W),   R(D,W), etherDelta=-amountW)
```

Each withdrawal CALL entry chains roots (advancing state), while each RESULT entry is identity for state but carries `etherDelta = -amount` for the ETH leaving the rollup. See §13d for full withdrawal delta semantics.

**Gas price ordering**: The builder sorts deposit entries by the L1 gas price of their corresponding user tx (descending) before computing chained deltas. This matches the L1 miner's tx ordering — miners order transactions by `max_fee_per_gas` descending, so higher-gas-price user txs are executed first on L1. If entries were chained in arrival order instead of gas price order, `_findAndApplyExecution` would revert with `ExecutionNotFound` because `currentState` wouldn't match the on-chain root at consumption time.

**Consumption on L1**: When `_findAndApplyExecution` processes an entry, it checks `delta.currentState == rollups[rollupId].stateRoot`. If it matches, the on-chain `stateRoot` advances to `delta.newState`. For deposit entries the RESULT is returned directly (no nested `newScope()`); for withdrawal entries `_processCallAtScope` enters scope navigation. This creates a strict ordering across both types:

- D1 can only be consumed when `stateRoot == R(0,0)` (clean state after postBatch)
- D2 can only be consumed when `stateRoot == R(1,0)` (after D1 consumed)
- W1 can only be consumed when `stateRoot == R(D,0)` (after all deposits consumed)
- W2 can only be consumed when `stateRoot == R(D,1)` (after W1 consumed)

**Partial consumption**: If entry E_k is not consumed, all subsequent entries are automatically blocked (their `currentState` won't match the on-chain root). The final on-chain state root equals the state after the last successfully consumed entry:

| Consumed entries       | Final on-chain stateRoot |
|------------------------|--------------------------|
| None                   | R(0,0) (clean)           |
| D1 only                | R(1,0)                   |
| All deposits           | R(D,0)                   |
| All deposits + W1      | R(D,1)                   |
| All deposits + all W   | R(D,W) (speculative)     |

**Consequence**: Derivation determines the correct L2 state by applying consumed entry deltas to the batch's clean state root (see §4e).

---

## 4. Derivation Rules

### 4a. Event source

Derivation reads **only** `BatchPosted` events from `rollups_address`. There is no other event source.

### 4b. Deriving blocks from a BatchPosted event

For each `BatchPosted` event:

1. Fetch the L1 transaction by hash (`eth_getTransactionByHash`)
2. Decode the `postBatch` calldata → get `(entries, blobCount, callData, proof)`
3. Decode `callData` → get `(uint256[] l2BlockNumbers, bytes[] transactions)`
4. Separate entries into immediate (`actionHash == 0`) and deferred (`actionHash ≠ 0`)
4a. For consumed deferred entries, reconstruct L2-format entry pairs using the CALL actions from `ExecutionConsumed` events (see §4e)
5. The **last** immediate entry's `StateDelta.newState` is the batch final state root
6. The final state root is assigned to the **last** block in the batch; intermediate blocks get `B256::ZERO` (recomputed locally by executing the block)
7. Deferred entries (filtered by §4e) are assigned to the **first** block in the batch — even if that block is a gap-fill empty block

### 4c. L1 context

For L2 blocks derived from a `BatchPosted` event at L1 block N:

```
l1_context_block_number = N - 1
l1_context_block_hash   = parent_hash of L1 block N
```

This is deterministic: the builder uses `latest_l1_block` when building, the tx lands in `latest_l1_block + 1`, so `containing_block - 1 = latest_l1_block`.

The L1 context is carried in the L2 block header:
- `parent_beacon_block_root` field → L1 block hash
- `prev_randao` field → L1 block number (encoded as U256 in B256)

### 4d. Gap-fill empty blocks

When submitted L2 block numbers are non-sequential (e.g., blocks 5, 8 submitted — blocks 6, 7 missing), the derivation pipeline generates **gap-fill empty blocks** for the missing numbers.

- Gap-fill blocks have no transactions (empty RLP list `[0xc0]`)
- Gap-fill blocks use the L1 context from the **previous** submission. If no previous submission exists (e.g., first batch after genesis), they use the deployment L1 block as context. After a reorg rollback, L1 context is restored from the cursor at the fork point.
- Gap-fill blocks have `state_root = B256::ZERO` (recomputed locally)
- Maximum gap: `MAX_BLOCK_GAP = 1000` blocks. Larger gaps are rejected as a DoS protection.

### 4e. Cross-chain entry filtering (L1 is the truth)

Deferred entries (`actionHash ≠ 0`) represent cross-chain execution. **They are only executed on L2 if they were consumed on L1.** This is the most important cross-chain rule:

- The builder may speculatively execute cross-chain calls on L2 before L1 confirmation
- But derivation (which all nodes use to establish the canonical L2 chain) must only include entries that were **actually consumed on L1** — proven by an `ExecutionConsumed` event
- If the builder's speculative state includes entries that were never consumed on L1, derivation will produce a different state root, and the builder must reorg to match L1 truth
- **Receipt checking, background polling, or any off-chain signal is NOT a substitute.** The only proof of consumption is the `ExecutionConsumed` event on L1. No event = it didn't happen.

Algorithm:

1. Fetch `ExecutionConsumed` events from `rollups_address` in range `[batch_l1_block, to_block]`
2. Collect the set of consumed `actionHash` values from event topics
3. Extract `batch_final_state_root` from the immediate entry (= clean state root Y)
4. For each deferred entry in the batch:
   - If `actionHash` is in the consumed set → include in L2 block's execution entries
   - If `actionHash` is NOT in the consumed set → **skip**
5. Compute `effective_state_root` by applying consumed entries' state deltas to `batch_final_state_root`:
   ```
   effective_root = batch_final_state_root  // Y
   for entry in consumed_deferred_entries:
       for delta in entry.state_deltas:
           if delta.currentState == effective_root:
               effective_root = delta.newState
   ```
6. Use `effective_root` as the block's expected state root (for verification in §8)
7. **Protocol tx filtering**: If any deferred entries were skipped in step 4, filter the corresponding builder protocol transactions from the block's callData transaction list (see §4f). This ensures all nodes execute the same filtered transaction list and produce the same state root.

**The fetch range extends to `to_block`** (the full derivation window, not just the batch's L1 block) because the user's proxy call may land in a later L1 block than the `postBatch`.

**Consequence for the builder**: The builder optimistically builds blocks WITH cross-chain entries (state root X). The immediate entry submitted to L1 uses the clean state root (Y). When all entries are consumed on L1, the on-chain root evolves to X — matching the builder's local state. When entries are NOT consumed, the on-chain root stays at Y (or an intermediate X_k for partial consumption). All nodes — builder, fullnodes, and sync — apply protocol tx filtering (§4f) during derivation, discarding `executeRemoteCall` txs (deposits) and L2-to-L1 cross-chain txs for unconsumed entries from the callData before execution. This produces root Y (zero consumption) or X_k (partial consumption) on every node, matching on-chain. The builder detects the mismatch in `flush_to_l1`, rewinds to Sync mode, and re-derives the block with filtered txs — producing the correct root. The builder can then submit new blocks without a pre_state_root mismatch.

**Builder batch constraint**: After posting a `postBatch` that includes cross-chain entries, the builder sets an **entry verification hold** that prevents both new `postBatch` submissions and new block production until derivation has verified the entry-bearing block. While the hold is active, the builder **halts block production** — `step_builder` returns early without building. This is necessary because building during hold would accumulate blocks with advancing L1 context that mismatch after a rewind, causing double rewind cycles (the first rewind produces blocks that themselves need rewinding). The hold is cleared when `verify_local_block_matches_l1` confirms the entry-bearing block matches the L1-derived state (entries consumed correctly), or when `clear_pending_state` runs during a rewind (which re-derives with §4f filtering). Once cleared, the builder resumes block production and submission in the next `step_builder` call.

### 4f. Protocol tx filtering (callData transaction discarding)

When §4e determines that deferred entries were NOT consumed on L1, the corresponding builder protocol transactions must also be discarded from the block's callData transaction list before execution. This is critical: the callData contains pre-signed speculative protocol txs (`loadExecutionTable`, `executeRemoteCall`, and L2-to-L1 cross-chain txs) that would produce phantom state if executed. All nodes perform the same filtering (based on the same `ExecutionConsumed` events), ensuring consensus.

Filtering uses a **unified single-pass** algorithm (`filter_block_entries`) that applies independent prefix counting to both deposit and L2-to-L1 cross-chain txs simultaneously. A block may contain both types.

**Two-phase identification**: L2-to-L1 cross-chain txs are identified **generically** via receipt-based event scanning, not by matching Bridge-specific selectors. This means any contract calling through a `CrossChainProxy` on L2 (whether `Bridge.bridgeEther`, `Bridge.bridgeTokens`, or any future cross-chain contract) is correctly classified without code changes.

Phase 1 (derivation, from L1 data only): Derivation computes `consumed_deposit_count` and `unconsumed_withdrawal_pair_count` from the batch entries and `ExecutionConsumed` events. These counts are attached to the derived block as `DeferredFiltering` metadata.

Phase 2 (driver, trial execution): The driver trial-executes the unfiltered block to obtain execution receipts. It scans receipts for `CrossChainCallExecuted` events (emitted by `CrossChainManagerL2.executeCrossChainCall` whenever a proxy forwards a call through the CCM). The tx indices that produced such events are the L2-to-L1 cross-chain txs. The driver then computes `consumed_l2_to_l1_count = total_l2_to_l1_txs - unconsumed_withdrawal_pair_count` and calls `filter_block_entries` with these counts.

**Filtering algorithm (unified prefix counting)**:

For each block in the batch that has unconsumed deferred entries:

1. Decode the block's RLP transaction list from callData
2. Count **consumed deposit entries** (N_d): the number of `executeRemoteCall` txs to keep (from Phase 1).
3. Count **consumed L2-to-L1 entries** (N_w): the number of L2-to-L1 cross-chain txs to keep (from Phase 2). Computed as `total_l2_to_l1_txs - unconsumed_withdrawal_pairs`. Withdrawal entries come in CALL+RESULT pairs in the batch; each pair maps to one L2-to-L1 tx.
4. Walk the transaction list once. For each transaction at index `i`:
   a. If the tx targets `cross_chain_manager_address` with the `executeRemoteCall` selector: this is the K-th such tx (counting from 1). If K <= N_d, **keep**. If K > N_d, **discard**.
   b. If index `i` is in the L2-to-L1 tx index set (from receipt scanning): this is the J-th such tx (counting from 1). If J <= N_w, **keep**. If J > N_w, **discard**.
   c. If the tx targets `cross_chain_manager_address` with the `loadExecutionTable` selector, **always keep**.
   d. All other txs (setContext, user txs), **keep**.
5. Execute the filtered transaction list. The builder account nonce advances only for executed txs — discarded txs do not consume nonces.

**Why receipt-based identification (not selector matching)**: The previous approach matched `bridgeEther(0)` selectors to identify L2-to-L1 txs. This was Bridge-specific and would not detect other cross-chain contracts (e.g., `bridgeTokens`, direct proxy calls, wrapper contracts). The `CrossChainCallExecuted` event is emitted by the CCM itself for **every** outgoing L2-to-L1 call, regardless of which contract initiated it. Receipt scanning is therefore fully generic and future-proof.

**Why prefix counting (not actionHash matching)**: The chained delta ordering (§3e) guarantees that consumption is always a **prefix** — within each type, entry 1 must be consumed before entry 2, etc. Therefore, the first N_d `executeRemoteCall` txs correspond to consumed deposit entries, and the first N_w L2-to-L1 txs correspond to consumed withdrawal entries. Prefix counting is simpler than actionHash reconstruction and correctly handles the **duplicate actionHash edge case**: when two entries call the same contract with identical parameters, they produce the same `keccak256(abi.encode(action))`. ActionHash matching cannot distinguish them, but prefix counting correctly keeps only the first N regardless of hash collisions.

**Why loadExecutionTable is always kept**: `loadExecutionTable` writes RESULT entries to the CCM's internal execution table — outgoing data for other rollups to consume. `executeRemoteCall` takes its parameters directly (destination, data, etc.) and does NOT read from the execution table. The table data has zero effect on user-facing contract state (balances, storage). Keeping loadTable with all entries is safe, enables per-tx filtering for partial consumption, and preserves the builder's nonce sequence on all nodes.

**Intermediate root alignment**: The builder's `compute_unified_intermediate_roots` computes intermediate roots with loadTable always loading ALL entries, applies the same two-phase identification (trial execution + receipt scanning for `CrossChainCallExecuted` events), and uses the same `filter_block_entries` function that derivation uses. This ensures the chained state deltas on L1 entries use roots that match what derivation produces. See §3e.

**Nonce consistency**: All nodes discard the same transactions (deterministic filtering from L1 events). The builder account nonce after the block is identical on every node. Subsequent blocks' protocol txs (signed by the builder with sequential nonces) are valid because the nonce sequence is consistent across all nodes.

**Example — zero deposit consumption** (1 deposit entry E1, not consumed):

```
callData:  [setContext(nonce=10), executeRemoteCall(nonce=11), userTx_A, userTx_B]
filtered:  [setContext(nonce=10), userTx_A, userTx_B]
```

Builder nonce after: 11 on all nodes. State root: R(0,0) (clean).

**Example — partial deposit consumption** (D1 consumed, D2 not):

```
callData:  [setContext(nonce=10), loadTable(nonce=11), executeRemoteCall_D1(nonce=12), executeRemoteCall_D2(nonce=13), userTxs...]
filtered:  [setContext(nonce=10), loadTable(nonce=11), executeRemoteCall_D1(nonce=12), userTxs...]
```

Builder nonce after: 13 on all nodes. State root: R(1,0) (D1 applied). The intermediate root R(1,0) matches the on-chain root after D1 consumption because `compute_unified_intermediate_roots` uses the same loadTable and filtering.

**Example — mixed block, partial consumption** (2 deposits D1,D2 consumed; 1 L2-to-L1 call W1 not consumed):

```
callData:  [setContext, loadTable, executeRemoteCall_D1, executeRemoteCall_D2, l2_to_l1_W1, userTxs...]
filtered:  [setContext, loadTable, executeRemoteCall_D1, executeRemoteCall_D2, userTxs...]
```

State root: R(2,0) (both deposits applied, no L2-to-L1 calls). On-chain root after D1+D2 consumption = R(2,0).

**Example — L2-to-L1 partial consumption** (W1 consumed, W2 not, no deposits):

```
callData:  [setContext(nonce=10), loadTable(nonce=11), l2_to_l1_W1(user_tx), l2_to_l1_W2(user_tx), ...]
filtered:  [setContext(nonce=10), loadTable(nonce=11), l2_to_l1_W1(user_tx), ...]
```

State root: R(0,1) (W1 applied). On-chain root after W1 consumption = R(0,1). Matches derivation.

In these examples, `l2_to_l1_W*` represents any L2-to-L1 cross-chain tx (e.g., `bridgeEther`, `bridgeTokens`, or any contract calling through a proxy). The filtering logic does not inspect the tx calldata -- it uses the L2-to-L1 tx index set from receipt scanning.

**Multi-block batch constraint**: Protocol tx filtering can cause nonce breaks within a multi-block batch. If block K's protocol txs are discarded but block K+1 exists in the same batch, K+1's pre-signed txs have nonces that assume block K's txs executed — the nonces are too high and execution fails. To prevent this, the builder flushes immediately after building a block with speculative cross-chain entries. Previously-queued blocks (without entries) are included in the same batch ahead of the entries block, making the entries block the **last** in the batch. Since no subsequent blocks follow it, nonce consistency is preserved. This is a builder-side constraint enforced in `step_builder`.

### 4g. Rollup ID filtering

All derived entries are filtered by `rollupId` matching the node's configured `ROLLUP_ID`. Entries for other rollups are ignored.

---

## 5. Builder Protocol Transactions

All protocol operations are builder-signed transactions placed at the start of every block, before user transactions. They are normal EVM transactions (with signature, gas cost, and nonce) signed by the builder's key. One implicit pre-execution operation runs before any transactions:

1. **Ethereum's beacon root contract (EIP-4788)** — standard Ethereum system call

**Block structure:**
```
Block N:
  beneficiary = builder_address
  tx[0]: builder → L2Context.setContext(l1ParentBlockNumber, l1ParentBlockHash)
  tx[1]: builder → CCM.loadExecutionTable(entries)           [if cross-chain entries exist]
  tx[2]: builder → CCM.executeIncomingCrossChainCall(...)     [per CALL trigger entry]
  tx[N..]: user transactions from mempool
```

**Block 1 (special — contract deployment):**
```
  tx[0]: builder → CREATE L2Context(authorizedCaller=builder)           nonce=0
  tx[1]: builder → CREATE CrossChainManagerL2(rollupId, systemAddress)  nonce=1
  tx[2]: builder → CREATE Bridge()                                      nonce=2
  tx[3]: builder → Bridge.initialize(ccmAddress, rollupId, admin)       nonce=3
  tx[4..]: bootstrap account funding (if configured)
  tx[N]:  builder → L2Context.setContext(...)
```
CCM and Bridge deployment (tx[1]..tx[3]) are conditional on cross-chain being configured (`rollups_address != 0`). Without cross-chain, only L2Context (tx[0]) is deployed and nonce advances to 1.

### 5a. L2Context

```
L2Context.setContext(uint256 l1ParentBlockNumber, bytes32 l1ParentBlockHash)
```

- Target: `L2_CONTEXT_ADDRESS` (deterministic: `CREATE(builder_address, nonce=0)`)
- Takes two parameters: the L1 **parent** block number and hash (i.e., the L1 block the builder saw as `latest` when building). L2 block number and timestamp are available via native EVM opcodes (`block.number`, `block.timestamp`) and are not passed as arguments.
- Sets per-block L1 context that L2 contracts can read
- Builder-signed transaction with `gas_price = baseFee` (1 wei at genesis)

### 5b. loadExecutionTable (cross-chain)

```
CrossChainManagerL2.loadExecutionTable(ExecutionEntry[] entries)
```

- Target: `CROSS_CHAIN_MANAGER_ADDRESS` (deterministic: `CREATE(builder_address, nonce=1)`)
- Only included when the block has deferred execution entries (from derivation or builder)
- **Filtering**: CALL entries targeting the current rollup are excluded from the table — they are executed separately in §5c
- Skipped if `cross_chain_manager_address` is zero or no entries exist
- **Self-cleaning semantics**: The contract deletes the entire execution table (`delete executions`) and re-populates it with the new entries. This guarantees that stale entries from prior blocks are overwritten rather than duplicated. No driver-side pre-check is needed.

### 5c. executeIncomingCrossChainCall (cross-chain)

```
CrossChainManagerL2.executeIncomingCrossChainCall(destination, value, data, sourceAddress, sourceRollup, scope)
```

- One transaction per CALL entry targeting the current rollup
- These are the entries filtered out in §5b — they trigger actual contract execution on L2
- If the call reverts, the revert is logged but does not halt block execution

**Terminology note**: The on-chain function name is `executeIncomingCrossChainCall`. The codebase uses `executeRemoteCall` as a shorthand in variable names, comments, and the Rust constant `EXECUTE_REMOTE_CALL_SELECTOR` (which maps to the `executeIncomingCrossChainCall` selector). This spec uses `executeRemoteCall` in filtering examples (§4f) and protocol tx identification for brevity; both names refer to the same function.

### 5d. Protocol transaction rules

- All protocol transactions use legacy (type 0) format with `gas_price = max(1, parent.base_fee_per_gas)`
- Builder signs with explicit nonces, tracked across blocks and recovered from L2 state on mode transitions
- Protocol transactions are deterministic: given the same inputs, all nodes produce identical blocks
- `beneficiary` (coinbase) is set to the builder address on all nodes

### 5e. CCM ETH balance (genesis pre-mint)

The CrossChainManagerL2 (CCM) needs ETH balance to forward to recipients when processing deposit entries via `executeIncomingCrossChainCall`. Rather than minting ETH at runtime (which breaks trace replay tools like `debug_traceTransaction`), the CCM receives a large pre-mint balance in the genesis allocation.

**Mechanism**: `deploy.sh` computes the deterministic CCM address (`CREATE(builder_address, nonce=1)`) and injects an alloc entry into `genesis.json` before computing the genesis state root for L1 registration:

```
"<ccm_address>": { "balance": "0xD3C21BCECCEDA1000000" }   // 1,000,000 ETH
```

The genesis state root submitted to `Rollups.createRollup()` includes this balance. All nodes start from the same genesis, so the CCM balance is consistent across builder, fullnodes, and sync.

**Why no runtime minting**: `apply_pre_execution_changes()` is pure standard Ethereum (beacon root contract only, per EIP-4788). There is no custom state modification before block transactions execute. This means `debug_traceTransaction` and other trace replay tools produce correct results — they see the same execution as live processing, with no hidden pre-execution state changes.

**Sufficiency**: Since `ccm_balance` (1,000,000 ETH) far exceeds any realistic deposit amount, the historical smart minting formula `delta = max(0, needed - ccm_balance)` always evaluates to 0. No runtime minting is ever triggered.

**Edge cases**:
- The CCM balance decreases when ETH is forwarded to deposit recipients, but 1,000,000 ETH provides ample buffer for normal operation
- If the CCM balance were ever exhausted (requires > 1M ETH in cumulative deposits without corresponding L2 activity returning ETH to the CCM), `executeIncomingCrossChainCall` would revert for insufficient balance — a detectable failure, not silent corruption
- Withdrawals burn ETH by sending to `SYSTEM_ADDRESS`, not from the CCM — they do not reduce the CCM balance

### 5f. Block 2 `setCanonicalBridgeAddress` (one-time protocol tx)

On **block 2**, if `bridge_l1_address` is configured (non-zero), the builder inserts a `setCanonicalBridgeAddress` protocol transaction before `setContext`. This is a one-time call that tells the L2 Bridge contract the address of its L1 counterpart, enabling flash loan continuation entries (§14) that require the bridge to know the canonical L1 bridge for cross-chain token returns.

```
Block 2 (special — canonical bridge setup):
  tx[0]: builder → Bridge.setCanonicalBridgeAddress(bridge_l1_address)   [one-time, conditional]
  tx[1]: builder → L2Context.setContext(l1ParentBlockNumber, l1ParentBlockHash)
  tx[2..]: normal protocol txs + user txs
```

**Why block 2**: The Bridge contract is deployed in block 1 (nonce=2) and initialized (nonce=3). Block 2 is the earliest block where the Bridge exists and can accept configuration. The `setCanonicalBridgeAddress` call is conditional on both `bridge_l1_address` and `bridge_l2_address` being non-zero in the node configuration.

**Determinism**: All nodes that derive block 2 from L1 will see this transaction in the `postBatch` callData, so it executes identically on builder, fullnodes, and sync. There is no consensus risk.

---

## 6. Consensus Validation

The `RollupConsensus` module validates:

1. **Timestamp**: `header.timestamp == deployment_timestamp + ((block_number + 1) × block_time)`
2. **Parent hash**: `header.parent_hash == parent.hash`
3. **Block number**: `header.number == parent.number + 1`
4. **Difficulty**: must be 0 (post-merge)
5. **Nonce**: must be 0 (post-merge)
6. **Extra data**: must be empty (deterministic block building — all nodes produce identical blocks)
7. **Gas**: `gas_used <= gas_limit`

No PoW/PoS validation — L1 is the consensus layer.

---

## 7. L1 Reorg Handling

The derivation pipeline stores `(l2_block_number, l1_block_number, l1_block_hash)` per derived block.

**Detection**: On each L1 poll, compare the most recent stored L1 block hashes against the canonical chain (up to `REORG_CHECK_DEPTH = 64` entries). If a mismatch is found, walk backward to find the fork point.

**Recovery**:
1. Roll back derivation cursor to the fork point
2. Discard derived blocks after the fork point
3. Re-derive from the new canonical chain
4. The execution cursor (which L1 block range to scan for `BatchPosted` events) is rolled back with `min(fork_point, current_cursor)` — it can only move backward, never forward, during a rollback. This ensures re-derivation scans the full range needed for any events that moved to different L1 blocks after the reorg.

**Finalization**: Blocks derived from finalized L1 blocks are pruned from the reorg-detection cursor (they can never reorg).

---

## 8. State Root Authority

**The L1-derived state root is authoritative.** Any node that re-executes the derivation pipeline from L1 events will produce the correct state root. There is no other source of truth.

- The builder submits a **clean** state root in `postBatch` (block state without cross-chain entry effects) — this is a verifiable claim
- Cross-chain entry effects are encoded as chained state deltas on deferred entries
- When entries are consumed on L1, the on-chain stateRoot evolves from clean to speculative
- If entries are NOT consumed, the on-chain root stays clean — matching derivation naturally
- **No corrective batches**: the builder never submits a postBatch to "fix" the on-chain state root. The clean/speculative separation ensures state convergence automatically.
- Fullnodes derive blocks from L1, execute the filtered transaction list (§4f), and compute their own state root
- If the builder's locally-built state root differs from the L1-derived state root:
  - **Builder mode**: state root mismatch is a critical error — the builder transitions back to **Sync mode**, rolls back derivation to the mismatched L1 block, clears all pending submissions and cross-chain entries, and re-derives from L1. Protocol tx filtering (§4f) ensures re-derivation produces the correct root (matching on-chain), so the rewind is productive. Repeated Builder→Sync→Builder cycles are damped with exponential backoff (up to 60s) to prevent tight rewind loops.
  - **Fullnode/Sync mode**: protocol tx filtering (§4f) ensures blocks are derived with the correct filtered transactions from the start — no mismatch occurs during normal derivation. If a mismatch is detected (e.g., after restart when re-verifying previously-committed blocks), L1-derived state is authoritative.
- **No off-chain signal (receipts, background polling, WebSocket notifications) can override L1 derivation.** The only mechanism for correcting L2 state is re-derivation from L1 events.

---

## 9. Operating Modes

### Sync mode
- Reads L1 `BatchPosted` events
- Derives blocks, executes transactions, computes state roots
- Used when catching up to L1 head

### Builder mode
- Builds blocks from mempool up to current wall clock time
- Posts blocks + cross-chain entries to L1 via `postBatch()`
- **Skip logic**: Before submitting, the builder checks the on-chain `stateRoot` against pending blocks. A block is considered already-submitted if `state_root == on_chain_root` (all entries consumed), `clean_state_root == on_chain_root` (no entries consumed), or `intermediate_roots` contains `on_chain_root` (partial consumption of deposits and/or withdrawals). All blocks up to and including the latest match are drained from the pending queue.

### Fullnode mode
- Derives from L1, optionally receives preconfirmation blocks from builder via WebSocket
- Preconfirmed blocks are verified against L1-derived blocks
- If preconfirmed block matches L1 derivation → accepted (fast path)
- If mismatch → L1 derivation wins (reorg to L1 version)

### Mode transitions

- **Sync → Builder**: when `last_processed_l1_block >= latest_l1_block` and `builder_mode=true`. All pending submissions, preconfirmed hashes, and cross-chain entries are cleared before entering Builder mode. The builder's L2 nonce is recovered from chain state.
- **Sync → Fullnode**: same condition but `builder_mode=false`. Same state clearing.
- **Builder → Sync**: on state root mismatch (see §8). Derivation rolls back, pending state is cleared. Re-entry to Builder is damped with exponential backoff to prevent tight rewind loops.
- **Fullnode → Sync**: not currently triggered (fullnodes follow L1 derivation only).

---

## 10. Determinism Requirements

For all nodes to converge on the same state, the following must be identical across builder, fullnodes, and sync:

1. **Timestamp formula**: exactly `deployment_timestamp + ((block_number + 1) × block_time)`
2. **Builder protocol transactions**: same setContext parameters, same execution, same builder address as coinbase
3. **Gas limit formula**: `calc_gas_limit` must use `(parent/1024).saturating_sub(1)` to match reth's `calculate_block_gas_limit`, with `DESIRED_GAS_LIMIT = 60_000_000` (60M) as the target
4. **Extra data**: must be empty (`""`) on all nodes
5. **Transaction ordering**: same transaction list from L1 callData
6. **Gap-fill blocks**: same empty block generation for missing block numbers
7. **L1 context**: same `l1_block_number` and `l1_block_hash` derived from L1 block containing the `BatchPosted` event
8. **Cross-chain protocol transactions**: same execution entries loaded via `loadExecutionTable`, same CALL entries executed via `executeRemoteCall`, in the same order (see §5b, §5c)

If ANY of these differ between nodes, state roots will diverge.

---

## 11. Checkpoint Persistence

The derivation pipeline persists its state to survive restarts:

- **Derivation cursor** (`last_processed_l1_block`) and **execution cursor** (`last_execution_l1_block`) are stored in reth's database as stage checkpoints
- **L1 block mapping** (`l2_block_number → l1_block_number, l1_block_hash`) is stored for reorg detection
- On restart, the cursor is rebuilt by scanning L2 block headers: `prev_randao` carries L1 block number, `parent_beacon_block_root` carries L1 block hash
- Finalized L1 blocks are pruned from the cursor (they can never reorg)
- Derivation resumes from the last checkpoint — it does NOT re-derive the entire chain

---

## 12. Invariants

These must hold at all times:

1. `l2_block_number` is strictly increasing (no duplicate blocks on the canonical chain)
2. `l2_timestamp` is strictly increasing and matches the deterministic formula
3. State root chaining is enforced on L1: each block's `currentState` matches the previous `newState`
4. A block's L1 context always refers to the L1 block BEFORE the containing block
5. Gap-fill blocks produce deterministic state (same as executing an empty block)
6. Deferred cross-chain entries are only executed on L2 if `ExecutionConsumed` exists on L1. Unconsumed entries are skipped, and their corresponding protocol txs (`executeRemoteCall` for deposits, L2-to-L1 cross-chain txs for withdrawals) are discarded from the callData before block execution (§4f). `loadExecutionTable` is always kept as it contains only internal CCM bookkeeping. Each CALL entry carries a chained `StateDelta` that evolves the on-chain `stateRoot` upon consumption. Partial consumption produces the correct intermediate state (see §3e).
7. Only ONE `postBatch()` per L1 block (enforced by `lastStateUpdateBlock`)
8. The derivation pipeline cursor never advances past successfully-processed blocks
9. The execution cursor only moves backward during reorg rollbacks, never forward
10. Builder protocol transactions (§5b, §5c) are deterministic and in the same order on all nodes
11. The immediate entry's `newState` is the **clean** state root — the block executed WITHOUT cross-chain entry effects. Entry effects are encoded in deferred entries' chained state deltas. The on-chain `stateRoot` equals the clean root after `postBatch`, and evolves toward the speculative root as entries are consumed.
12. Deposits and withdrawals may coexist in the same block and `postBatch`. The unified intermediate root chain (§3e) handles both types: D+W+1 roots from R(0,0) through R(D,0) to R(D,W). §4f filtering applies independent prefix counting to both `executeRemoteCall` (deposit) and L2-to-L1 cross-chain txs (identified generically via `CrossChainCallExecuted` receipt events) in a single pass.
13. Both deposit and withdrawal deferred entries carry chained intermediate state roots (§3e, §13d), not identity deltas. Each CALL entry (deposit or withdrawal) has a unique `currentState` enabling L1 disambiguation via `_findAndApplyExecution`. Partial consumption produces deterministic intermediate state roots, and derivation filters unconsumed txs of both types to produce matching roots (§4f).
14. Multi-call continuation L2 entries MUST use scope navigation (`callReturn` with `scope=[0]`) whenever a call has children whose effects must be delivered within the same L2 transaction. This applies to flash loan token returns (§14a, §14b) and recursive ping-pong patterns (§14g). Without scope navigation, `_processCallAtScope` never executes the return call. For depth > 1 patterns, scope navigation entries are generated recursively at every level of the call tree (§14f).
15. Return calls for multi-call L2-to-L1 continuations MUST be discovered via combined L1 simulation (`simulate_l1_combined_delivery`, which bundles all triggers in one `debug_traceCallMany` so later calls see earlier state effects) OR constructed analytically from the forward trip's `receiveTokens` parameters (swap `destinationAddress` and `sourceRollupId`). Combined simulation is the primary path; analytical construction is the fallback when simulation fails or returns no return calls. Per-call L1 delivery simulation cannot be used because it runs each call in isolation, so the second call cannot see tokens released by the first (§14c).
16. L1 and L2 multi-call entry structures are mirrors of each other for equivalent flows. L1-to-L2 flash loan (§14a): 3 L1 entries + 3 L2 entries. L2-to-L1 flash loan (§14b): 3 L2 entries (2 direct calls + 1 scope exit) + 5 L1 entries. The asymmetry in L1 entry count arises because the builder must send separate trigger txs, each producing trigger + resolution entries (§14d).
17. Recursive cross-chain discovery (§14f) uses Phase A/B alternation with `MAX_RECURSIVE_DEPTH = 5`. Phase A simulates L2-to-L1 calls on L1 to discover return calls; Phase B simulates return calls on L2 to discover nested L2-to-L1 calls. Both multi-call and single-call paths use the same loop. The single-call path promotes to multi-call entry construction when depth > 1 calls are discovered.
18. Return call children in `build_l2_to_l1_continuation_entries` are identified by `child.call_action.rollup_id == our_rollup_id` (§14f). For return call children, `callReturn` and trigger entries use NON-swapped addresses (destination stays destination, source stays source). For L2-to-L1 children, addresses ARE swapped. Incorrect classification produces wrong proxy addresses and `ExecutionNotFound` on L1.
19. `walk_trigger_trace_for_return_calls` MUST skip forward delivery proxy calls (`from == rollups_address`) when scanning L1 traces for return calls (§14c). Without this filter, the forward delivery is misclassified as a return call, producing spurious entries.

---

## 13. L2-to-L1 Withdrawals

L2→L1 ETH withdrawals are symmetric with L1→L2 deposits, using the same two-entry CALL+RESULT pattern but in the reverse direction.

### 13a. Withdrawal Flow

The canonical example uses `Bridge.bridgeEther`, but the detection and entry construction pipeline is fully generic -- any contract calling through a `CrossChainProxy` on L2 triggers the same flow. The L2-to-L1 composer RPC (`composer_rpc/l2_to_l1.rs`) detects all cross-chain calls via the protocol-level `executeCrossChainCall` child pattern in traces, not contract-specific selectors.

1. User calls a cross-chain contract on L2 (e.g., `Bridge.bridgeEther{value:X}(0)` where rollupId=0 means L1)
2. Contract creates proxy(user, 0) on L2, calls `proxy{value:X}(data)`
3. Proxy → `CCM.executeCrossChainCall(user, data)` with msg.value=X
4. CCM burns X ETH (sends to `SYSTEM_ADDRESS`), computes actionHash, emits `CrossChainCallExecuted`, consumes pre-loaded execution table entry
5. Builder posts `postBatch` to L1 with deferred L2-to-L1 entries
6. Builder sends trigger tx: calls proxy(user, rollup_id) on L1 with value=0
7. `Rollups.executeCrossChainCall` matches trigger CALL against stored deferred entry
8. `_processCallAtScope` sends X ETH to user via `proxy.executeOnBehalf{value:X}(user, data)`
9. RESULT entry consumed, `_applyStateDeltas` verifies `_etherDelta == totalEtherDelta`

### 13b. Nonce-Linked Atomicity

Withdrawals achieve practical atomicity through sequential nonces from the same sender in the same L1 block:
- Nonce K: `postBatch` (stores deferred entries)
- Nonce K+1: `createCrossChainProxy` (ensures L1 proxy exists)
- Nonce K+2: trigger tx (calls proxy to consume entries)

Since all transactions share the same sender, L1 miners include all or none. The gas overbid on `postBatch` ensures correct ordering within the block.

**Trigger failure recovery**: If the trigger tx fails to send (RPC error, nonce corruption), the builder immediately rewinds to Sync mode: clears pending state, rolls back derivation to the last confirmed anchor, and re-derives. The trigger tx never lands on L1, so entries remain unconsumed. Re-derivation produces the clean root (R0), matching the on-chain state.

If `postBatch` succeeds but the trigger tx **reverts** on L1 (e.g., `EtherDeltaMismatch` on one of several triggers, or gas estimation failure), the builder waits for the `postBatch` receipt, then checks all trigger receipts. If any trigger reverted, the builder rewinds for re-derivation with §4f filtering. The on-chain root is at some intermediate R_k; derivation filters unconsumed L2-to-L1 txs and produces the matching root.

### 13c. Entry Format

**L2 table entries** (loaded via `loadExecutionTable`, consumed by user's Bridge tx):
- CALL: actionHash matches CCM.executeCrossChainCall output. nextAction = RESULT (terminal).
- RESULT: actionHash matches RESULT action. nextAction = RESULT (terminal).

**L1 deferred entries** (posted via `postBatch`, consumed by trigger tx):
- CALL: actionHash matches Rollups.executeCrossChainCall output from trigger. nextAction = delivery CALL (enters `newScope` for ETH delivery).
- RESULT: actionHash matches delivery RESULT (empty returnData for EOA). nextAction = terminal RESULT.

L1 entries use **nested** format (unlike deposits which use non-nested) because `_processCallAtScope` enters scope navigation to execute ETH delivery. Multi-call continuation entries (§14) also use nested format with `scope=[0]` for both delivery and execution calls.

### 13d. State Delta Accounting (Unified Intermediate State Roots)

Withdrawal entries participate in the **unified intermediate root chain** (§3e) alongside deposit entries. For a block with D deposits and W withdrawals, the builder computes D+W+1 intermediate state roots via `compute_unified_intermediate_roots`. The function re-executes the block with prefix-filtered transaction lists using the same `filter_block_entries` function that derivation uses (§4f), guaranteeing byte-identical tx lists and thus identical state roots.

**Root chain construction**:

```
roots[0]     = block with 0 deposits, 0 withdrawals (clean)
roots[d]     = block with d deposits, 0 withdrawals    (for d in 1..D)
roots[D]     = block with all D deposits, 0 withdrawals
roots[D+w]   = block with all D deposits, w withdrawals (for w in 1..W)
roots[D+W]   = block with all txs = speculative_root
```

The last root (speculative) is already known from block execution — no rebuild needed.

**Entry state delta assignment** (via `attach_unified_chained_state_deltas`):

For deposit entry i (0-indexed, i in 0..D):
```
Di_CALL: StateDelta(rollupId, roots[i], roots[i+1], etherDelta=+call_value_i)
```
RESULT entries for deposits carry no state delta (same as §3e).

For withdrawal entry j (0-indexed, j in 0..W):
```
Wj_CALL:   StateDelta(rollupId, roots[D+j],   roots[D+j+1], etherDelta=0)
Wj_RESULT: StateDelta(rollupId, roots[D+j+1], roots[D+j+1], etherDelta=-amount_j)
```

Each CALL entry (deposit or withdrawal) chains roots, giving it a unique `currentState` for `_findAndApplyExecution` to match. Each withdrawal RESULT entry is identity for state but carries `etherDelta = -amount` for the ETH leaving the rollup.

**Immediate entry**: The aggregate immediate entry's `newState` is `roots[0]` (the clean root, state WITHOUT any deposit or withdrawal entry txs). The on-chain `stateRoot` advances to the clean root after `postBatch`, then evolves as entries are consumed.

**`_etherDelta` accounting for withdrawals**: trigger msg.value=0 -> `+0`, delivery sends X ETH -> `-X`. Total = `-X`. Entry state deltas: CALL etherDelta=0, RESULT etherDelta=-X. Total = `-X`. Match verified by `_applyStateDeltas`.

**Consumption ordering on L1**: Follows the unified chain (§3e). Deposits are consumed first (D1..DD), then withdrawals (W1..WW). Partial consumption produces deterministic intermediate state roots — see §3e table.

**Derivation**: The same `effective_state_root` algorithm (§4e step 5) applies. After `postBatch`, `batch_final_state_root = roots[0]`. Derivation walks consumed CALL entry deltas forward through the unified chain. The effective root equals the state after the last consumed entry of either type.

**`loadExecutionTable` during intermediate root computation**: The builder always keeps `loadExecutionTable` when computing intermediate roots. The table must be present for all prefix computations to maintain consistent nonce accounting (see §4f "Why loadExecutionTable is always kept").

**Flush-to-L1 skip logic**: The `rposition` check in `flush_to_l1` matches the on-chain root against `state_root`, `clean_state_root`, and every element of `intermediate_roots`. This handles partial consumption (on-chain root is an intermediate root that matches neither clean nor speculative).

### 13e. Coexistence of Deposits and Withdrawals

Deposits and withdrawals may coexist in the same block and `postBatch`. The unified intermediate root chain (§3e) disambiguates both types: D+W+1 roots provide unique `currentState` values for every CALL entry regardless of type.

**Why mutual exclusion was previously required**: Before unified intermediate roots, withdrawal entries used identity state deltas (`currentState = newState`), making them invisible to state root comparison. In a mixed block, derivation could not determine which type of entry was consumed or unconsumed. The unified chain eliminates this problem because every entry — deposit or withdrawal — advances the state root.

**Builder block construction**: The builder drains both the deposit queue and the withdrawal queue into the same block. In the unified intermediate root chain, deposits come first (matching `executeRemoteCall` execution order before L2-to-L1 cross-chain txs in the block).

**Filtering**: §4f applies independent prefix counting to `executeRemoteCall` (deposits) and L2-to-L1 cross-chain txs (identified generically via `CrossChainCallExecuted` receipt events) in a single pass. No type disambiguation is needed within a batch.

**Entry ordering in `postBatch`**: Deposit entries (non-nested format) appear first, followed by withdrawal entries (nested format). This matches the consumption order on L1: deposits are consumed before withdrawals.

### 13f. Derivation

During derivation, fullnodes reconstruct withdrawal entries from `BatchPosted` events:
- Identify withdrawal entries by `nextAction.actionType == CALL` (nested format)
- From the L1 trigger CALL action, reconstruct L2 table entries for `loadExecutionTable`
- If entries are consumed (`ExecutionConsumed` events): include L2 table entries in block, user's Bridge tx succeeds
- If entries are NOT consumed: skip L2 table entries, user's cross-chain tx reverts (no matching `_executions[actionHash]`), ETH not burned — safe fallback

### 13g. Deferral Exhaustion and Builder Crash Recovery

**Entry verification deferrals**: After posting a `postBatch` with cross-chain entries, the builder sets an entry verification hold (§4e). When the entry-bearing block's state root does not yet match the L1-derived root (because `ExecutionConsumed` events have not appeared), the builder defers verification up to `MAX_ENTRY_VERIFY_DEFERRALS = 3` times. Each deferral returns an error that triggers the main loop's exponential backoff (2+4+8 = 14 seconds total), giving L1 time to mine the user's consumption tx.

**Deferral exhaustion -> rewind**: If deferrals are exhausted (entries still not consumed), the builder **rewinds** to Sync mode. The rewind target is `entry_block - 1`, so the entry block itself gets re-derived with §4f-filtered txs. This produces correct nonces for subsequent blocks. Without rewind, fullnodes would diverge permanently because they derive with filtered txs while the builder's local state has unfiltered nonces.

**Builder crash between `postBatch` and trigger**: If the builder crashes after `postBatch` confirmation but before sending the trigger tx, entries remain unconsumed on L1. On restart, the builder re-derives from L1. Derivation applies §4f filtering: all unconsumed `executeRemoteCall` and L2-to-L1 cross-chain txs are filtered out (consumed count = 0), producing R(0,0) as the effective state root. For L2-to-L1 calls, the user's cross-chain tx is not executed, so ETH is not burned on L2 — the user retains their funds and can retry.

**Permanent builder failure**: If the builder never restarts (permanent hardware failure with no backup):
- Entries remain unconsumed on L1 indefinitely
- The user's withdrawal is blocked but funds are safe (not burned) because re-derivation filters the withdrawal tx
- A replacement builder (or manual intervention) can submit the trigger tx to complete the withdrawal

This is a significant improvement over the previous design where identity state deltas meant unconsumed entries were invisible to derivation — the old design would burn ETH on L2 without L1 delivery. With unified intermediate state roots, derivation can always detect and filter unconsumed entries of both types.

### 13h. EOA Restriction

The implementation now supports both EOA and contract recipients. For contract recipients, `simulate_l1_delivery` captures the delivery return data via `eth_call`, and the RESULT action hash includes the actual return bytes (not `result_void`). See §14f for the continuation entry structure.

---

## 14. Multi-Call Continuations (Flash Loans)

Multi-call continuations extend the basic CALL-RESULT entry pattern to support **atomic multi-step cross-chain interactions** where intermediate results must be delivered within the same transaction. The canonical use case is flash loans: tokens are borrowed, bridged cross-chain, used, bridged back, and repaid -- all atomically.

The key mechanism is **scope navigation**. When an entry's `nextAction` is a CALL with `scope=[0]` (rather than a terminal RESULT), `_resolveScopes` enters `newScope` and `_processCallAtScope` executes the delivery/execution call inline. This allows tokens to return within the same transaction via `executeOnBehalf`.

### 14a. L1-to-L2 Flash Loan (Forward Direction)

**Scenario**: User calls `executor.execute()` on L1. FlashLoan lends 10k tokens. The executor makes two L1-to-L2 cross-chain calls: (1) `bridgeTokens` to bridge tokens to L2, and (2) `claimAndBridgeBack` which claims an NFT on L2 and bridges tokens back. Scope navigation on L2 calls `Bridge_L2.receiveTokens` to mint wrapped tokens, and `Bridge_L1.receiveTokens` releases native tokens to the executor for flash loan repayment.

**L1 execution table (3 deferred entries)**:

```
L1 Entry 0 (forward bridge):
  actionHash = hash(CALL{L2, bridgeL2, fwdReceiveTokens, bridgeL1, MAINNET, []})
  stateDeltas: [{rollupId: L2, current: S0, new: S1, etherDelta: 0}]
  nextAction:  result_L2_void = RESULT{rollupId: L2, data: "", failed: false, scope: []}

L1 Entry 1 (claim and bridge back):
  actionHash = hash(CALL{L2, executorL2, claimAndBridgeBack, executorL1, MAINNET, []})
  stateDeltas: [{rollupId: L2, current: S1, new: S2, etherDelta: 0}]
  nextAction:  callReturnScoped = CALL{
    rollupId: MAINNET, destination: bridgeL1, data: retReceiveTokens,
    sourceAddress: bridgeL2, sourceRollup: L2, scope: [0]
  }

L1 Entry 2 (scope exit):
  actionHash = hash(RESULT{rollupId: MAINNET, data: "", failed: false, scope: []})
  stateDeltas: [{rollupId: L2, current: S2, new: S3, etherDelta: 0}]
  nextAction:  result_L2_void
```

L1 entries use the **direct call perspective** for action hashes because the user calls proxies on L1 directly (not through builder trigger transactions).

**L2 execution table (3 entries, loaded via `loadExecutionTable`)**:

```
L2 Entry 0:
  actionHash = hash(result_L2_void)
  stateDeltas: []
  nextAction:  callB = CALL{L2, executorL2, claimAndBridgeBack, executorL1, MAINNET, []}

L2 Entry 1:
  actionHash = hash(CALL{MAINNET, bridgeL1, retReceiveTokens, bridgeL2, L2, []})
  stateDeltas: []
  nextAction:  result_MAINNET_void

L2 Entry 2:
  actionHash = hash(result_L2_void)    // same hash as Entry 0 -- second match (swap-and-pop)
  stateDeltas: []
  nextAction:  result_L2_void          // terminal
```

**L2 execution flow**: A single `executeIncomingCrossChainCall` system call triggers the full chain:
1. `receiveTokens` on L2 mints 10k wrapped tokens to executorL2
2. RESULT{L2,void} consumed -> Entry 0 -> `nextAction=callB` -> `_processCallAtScope` calls `executorL2.claimAndBridgeBack`
3. Inside `claimAndBridgeBack`: `Bridge.bridgeTokens` burns wrapped tokens, proxy -> reentrant `executeCrossChainCall` -> consumes Entry 1 -> returns `result_MAINNET_void`
4. `claimAndBridgeBack` returns -> RESULT{L2,void} -> consumes Entry 2 -> terminal

**L1 scope navigation**: Entry 1's `nextAction` has `scope=[0]`, causing `_resolveScopes` to call `_processCallAtScope`. This executes `proxy(bridgeL2, L2).executeOnBehalf(bridgeL1, retReceiveTokens)`, which calls `Bridge_L1.receiveTokens` to release 10k native tokens to executorL1. The executor repays the flash loan.

### 14b. L2-to-L1 Flash Loan (Reverse Direction)

**Scenario**: User calls `reverseExecutorL2.execute()` on L2. FlashLoanL2Reverse lends 10k wrapped tokens. The executor makes two L2-to-L1 cross-chain calls: (1) `bridgeTokens` to bridge wrapped tokens to L1 (burns on L2), and (2) `claimAndBridgeBack` which claims an NFT on L1 and bridges tokens back to L2. Scope navigation on L2 calls `Bridge_L2.receiveTokens` to mint wrapped tokens for flash loan repayment.

This flow is fundamentally different from §14a:
- In §14a (L1-to-L2): the user triggers on L1; cross-chain execution happens on L2 via a single `executeIncomingCrossChainCall` system call.
- In §14b (L2-to-L1): the user triggers on L2 via proxy calls consuming L2 entries; the builder sends trigger transactions on L1 to consume L1 entries with nested delivery/execution.

**L2 execution table (3 entries with scope navigation)**:

```
L2 Entry 0 (forward bridge):
  actionHash = hash(CALL{MAINNET, bridgeL1, receiveTokens, bridgeL2, L2, []})
  stateDeltas: []
  nextAction:  result_L1_void = RESULT{rollupId: MAINNET, data: "", failed: false, scope: []}

L2 Entry 1 (claim and bridge back):
  actionHash = hash(CALL{MAINNET, reverseExecutorL1, claimAndBridgeBack, reverseExecutorL2, L2, []})
  stateDeltas: []
  nextAction:  callReturn = CALL{
    rollupId: L2, destination: bridgeL2, data: retReceiveTokens,
    sourceAddress: bridgeL1, sourceRollup: MAINNET, scope: [0]
  }

L2 Entry 2 (scope exit):
  actionHash = hash(RESULT{rollupId: L2, data: "", failed: false, scope: []})
  stateDeltas: []
  nextAction:  result_L1_void
```

**L2 scope navigation (flash loan repayment)**: Entry 1's `nextAction` is a CALL with `scope=[0]`. When `executeCrossChainCall` returns this from `_consumeExecution`, `_resolveScopes` enters scope navigation: `newScope([]) -> newScope([0]) -> _processCallAtScope`. This calls `proxy(bridgeL1, MAINNET).executeOnBehalf(bridgeL2, retReceiveTokens)`, which invokes `Bridge_L2.receiveTokens` and **mints** wrapped tokens back to `reverseExecutorL2`. The flash loan can then be repaid within the same L2 transaction.

Without scope navigation (if Entry 1 returned `RESULT(L1, void)` directly), the burned tokens would never return on L2, and `FlashLoanL2Reverse.flashLoan` would revert with `ERC20InsufficientBalance` when checking `balanceAfter >= balanceBefore`.

**`callReturn` field construction**: The `callReturn` action targets L2 (where `receiveTokens` mints wrapped tokens):
- `rollupId = L2` (our rollup, where the call executes)
- `destination = bridgeL2` (= `call_a.source_address`, the bridge that called the proxy)
- `data = retReceiveTokens` (from the child call's detected data)
- `sourceAddress = bridgeL1` (= `call_a.destination`, the L1 bridge)
- `sourceRollup = MAINNET` (L1)
- `scope = [0]` (forces `_processCallAtScope`)

**L1 execution table (5 deferred entries)**:

```
L1 Entry 0 (trigger -> nested delivery):
  actionHash = hash(CALL{L2, bridgeL2, receiveTokens, builder_address, MAINNET, []})
  stateDeltas: [{rollupId: L2, current: S0, new: S1, etherDelta: 0}]
  nextAction:  delivery_A = CALL{
    rollupId: MAINNET, destination: bridgeL1, data: receiveTokens,
    sourceAddress: bridgeL2, sourceRollup: L2, scope: [0]
  }

L1 Entry 0b (delivery resolution):
  actionHash = hash(RESULT{rollupId: MAINNET, data: delivery_return_data, failed: false, scope: []})
  stateDeltas: []
  nextAction:  result_L1_void
  // NOTE: data = "" for void functions (result_void). For functions that return
  // data, use the actual delivery_return_data captured from simulate_l1_delivery.
  // See §14f "Delivery return data in RESULT hashes" for details.

L1 Entry 1 (trigger -> nested execution):
  actionHash = hash(CALL{L2, reverseExecutorL2, claimAndBridgeBack, builder_address, MAINNET, []})
  stateDeltas: [{rollupId: L2, current: S1, new: S2, etherDelta: 0}]
  nextAction:  execution_B = CALL{
    rollupId: MAINNET, destination: reverseExecutorL1, data: claimAndBridgeBack,
    sourceAddress: reverseExecutorL2, sourceRollup: L2, scope: [0]
  }

L1 Entry 1b (bridge return -- reentrant):
  actionHash = hash(CALL{L2, bridgeL2, retReceiveTokens, bridgeL1, MAINNET, []})
  stateDeltas: [{rollupId: L2, current: S2, new: S3, etherDelta: 0}]
  nextAction:  result_L1_void

L1 Entry 2 (scope exit):
  actionHash = hash(RESULT{rollupId: MAINNET, data: "", failed: false, scope: []})
  stateDeltas: []
  nextAction:  result_L1_void
```

**L1 action hash perspective**: L1 entries use the **trigger perspective** for action hashes. The builder sends trigger transactions to proxies on L1, so `Rollups.executeCrossChainCall` computes action hashes with `sourceAddress = builder_address` (the msg.sender to the proxy), not the L2 contract that initiated the call. This differs from §14a where the user calls proxies directly. The `nextAction` fields use the **execution perspective** (the actual target, source, and rollup where the call executes).

**L1 trigger execution**:

*Trigger 1* -- Builder sends to `proxy(bridgeL2, L2)` on L1 with `receiveTokens` calldata:
1. `Rollups.executeCrossChainCall` matches Entry 0, state S0->S1, returns `delivery_A{scope:[0]}`
2. `_resolveScopes` -> `newScope([0])` -> `_processCallAtScope`: calls `proxy(bridgeL2, L2).executeOnBehalf(bridgeL1, receiveTokens)`
3. `Bridge_L1.receiveTokens` releases 10k native tokens to `reverseExecutorL1`
4. Returns void -> `result_void(MAINNET)` -> consumes Entry 0b -> returns `result_L1_void` (terminal)

*Trigger 2* -- Builder sends to `proxy(reverseExecutorL2, L2)` on L1 with `claimAndBridgeBack` calldata:
1. `Rollups.executeCrossChainCall` matches Entry 1, state S1->S2, returns `execution_B{scope:[0]}`
2. `_resolveScopes` -> `newScope([0])` -> `_processCallAtScope`: calls `proxy(reverseExecutorL2, L2).executeOnBehalf(reverseExecutorL1, claimAndBridgeBack)`
3. `reverseExecutorL1.claimAndBridgeBack`: claims NFT, calls `Bridge_L1.bridgeTokens`
4. Bridge_L1 locks tokens, calls `proxy(bridgeL1, L2)` on L1 (reentrant) -> `executeCrossChainCall` -> matches Entry 1b, state S2->S3, returns `result_L1_void`
5. `claimAndBridgeBack` returns -> `result_void(MAINNET)` -> consumes Entry 2 -> returns `result_L1_void` (terminal)

**Hash collision (Entries 0b and 2)**: Both have `hash(RESULT{rollupId: MAINNET, data: "", failed: false, scope: []})`. This is safe because Entry 0b is consumed first (during Trigger 1's scope resolution) via swap-and-pop. By the time Trigger 2 needs Entry 2, Entry 0b is gone.

### 14c. Return Call Discovery

Return calls for multi-call L2-to-L1 continuations are discovered via one of two paths: **combined L1 simulation** (primary) or **analytical construction** (fallback).

**Primary path -- combined L1 simulation**: `simulate_l1_combined_delivery` bundles all trigger transactions into a single `debug_traceCallMany` call with sequential state:

```
bundle: [postBatch(combined_entries), createProxy(user0), trigger0, createProxy(user1), trigger1, ...]
```

Because all triggers execute within one bundle, later calls see state effects from earlier ones (e.g., tokens released by `receiveTokens` in trigger 0 are available to `claimAndBridgeBack` in trigger 1). The function uses iterative discovery: if trigger traces reveal new return calls, entries are rebuilt with continuation structure and re-simulated until convergence or `MAX_SIMULATION_ITERATIONS`. Each iteration returns per-call results with delivery return data and detected return calls.

This solves the per-call isolation problem that prevents independent `simulate_l1_delivery` from working for multi-call patterns. Per-call simulation runs each delivery in isolation with placeholder entries, so the second call cannot see tokens released by the first.

**Fallback path -- analytical construction**: If combined simulation fails (returns `None`) or discovers no return calls, the analytical path in `table_builder.rs` (`analyze_continuation_calls`) constructs return calls from the forward trip's parameters. For the bridge return trip:

1. Decode `receiveTokens` parameters from call_a (the forward bridge call)
2. Construct `retReceiveTokens` with swapped `destinationAddress` and `sourceRollupId`:
   - Forward: `receiveTokens(token, originRollupId, executorL1, amount, ..., sourceRollupId=L2)`
   - Return:  `receiveTokens(token, originRollupId, executorL2, amount, ..., sourceRollupId=MAINNET)`
3. The `destinationAddress` changes from the L1 recipient to the L2 recipient (swap direction)
4. The `sourceRollupId` changes from L2 to MAINNET (the bridge sending is now on L1)

The analytical path is bridge-specific (it knows the `receiveTokens` ABI) while the combined simulation path is generic (it discovers return calls from traces regardless of contract type).

**Forward delivery filtering in trace walking**: `walk_trigger_trace_for_return_calls` walks the `callTracer` output of each L1 trigger transaction to find return calls (proxy calls targeting our rollup). At depth >= 2 in the trace tree, it checks each call target against L1 `authorizedProxies`. However, the first proxy call encountered is typically the **forward delivery** -- `Rollups.executeCrossChainCall` calling `proxy.executeOnBehalf(destination, data)` to deliver the original cross-chain call. This is NOT a return call. The function distinguishes forward delivery from return calls by checking the caller: if `from == rollups_address`, it is a forward delivery (Rollups delivering the original call) and is skipped. Only proxy calls where `from` is a user contract (not Rollups) are classified as return calls. The function continues recursing past forward delivery calls to find the real return calls deeper in the trace tree.

**Return call address semantics**: Return calls detected in L1 traces have `(destination, source_address)` from the L1 execution perspective. A `DetectedReturnCall` stores `destination` as the L2 contract being called (the proxy's `originalAddress`) and `source_address` as the L1 contract that called the proxy. These addresses are passed through to entry construction without swapping -- see §14f for how `is_return_call` routing uses them.

### 14d. Structural Symmetry

L1-to-L2 and L2-to-L1 flash loans are structural mirrors:

| Property | L1-to-L2 (§14a) | L2-to-L1 (§14b) |
|----------|-----------------|-----------------|
| User triggers on | L1 (direct proxy calls) | L2 (proxy calls via executeCrossChainCall) |
| Scope navigation on | L2 (single executeIncomingCrossChainCall) | L2 (callReturn in entry nextAction) + L1 (nested delivery/execution) |
| Token return mechanism | Bridge_L1.receiveTokens releases native tokens | Bridge_L2.receiveTokens mints wrapped tokens |
| L1 entries | 3 (user perspective for actionHash) | 5 (trigger perspective: sourceAddress=builder) |
| L2 entries | 3 (loaded by system call) | 3 (loaded by loadExecutionTable, consumed by user proxy calls) |
| Trigger transactions | None (user calls proxies directly on L1) | 2 builder trigger txs on L1 (nonce-linked with postBatch) |

**Why L2-to-L1 requires 5 L1 entries (vs 3)**: In L1-to-L2, the user's proxy calls on L1 directly trigger `executeCrossChainCall`, which consumes entries and enters scope navigation inline. In L2-to-L1, the builder must send separate trigger txs on L1 for each detected L2 proxy call. Each trigger produces a trigger entry (actionHash from trigger perspective) plus a scope resolution entry. The bridge return trip from `claimAndBridgeBack` also generates a reentrant entry (1b), producing 5 total: 0, 0b, 1, 1b, 2.

**Why L2-to-L1 requires scope navigation on L2**: In the original protocol spec, L2 entries for L2-to-L1 returned `RESULT(L1, void)` directly -- no scope navigation. This works for simple bridge transfers but fails for flash loans: `bridgeTokens` burns wrapped tokens on L2, and without scope navigation, `receiveTokens` (which mints them back) never executes within the same L2 tx. The flash loan pool's `balanceAfter >= balanceBefore` check fails. Adding `callReturn{scope=[0]}` as Entry 1's `nextAction` causes `_processCallAtScope` to call `Bridge_L2.receiveTokens` inline, minting wrapped tokens before the flash loan repayment check.

### 14e. Builder Detection and Entry Construction

The builder detects multi-call continuations via iterative `debug_traceCallMany` simulation on the L1 proxy. The detection and entry construction pipeline is implemented in `table_builder.rs`:

1. **L1 detection** (`analyze_l2_to_l1_continuation_calls`): Identifies L2 proxy calls by scanning for `executeCrossChainCall` traces. Each detected call produces a `DetectedCall` with the CALL action, return data, and parent-child relationships.
2. **L2 detection** (`analyze_continuation_calls`): Identifies multi-call patterns from L1 delivery simulation. Decodes `receiveTokens` from the forward bridge call and constructs the return call analytically.
3. **Entry building** (`build_l2_to_l1_continuation_entries`): Constructs both L2 table entries and L1 deferred entries from the detected calls. The function is recursive: for depth > 1 call trees, it generates scope navigation entries at every level where a call has children, regardless of nesting depth. L2 entries are loaded via `loadExecutionTable`; L1 entries are included in `postBatch`.

**Continuation entry classification**: Continuation entries have `nextAction` targeting our rollup (e.g., `callReturn{rollupId=L2}`), but their `actionHash` is `hash(RESULT)` -- NOT `hash(callReturn)`. The `partition_entries` function in the driver must use a `hash(next_action) == action_hash` guard to distinguish trigger entries from continuation entries. Only entries where the action hash matches the hash of the next action are trigger entries (sent to `executeIncomingCrossChainCall`). Continuation entries are loaded into the execution table via `loadExecutionTable`.

**RESULT entry suppression**: When continuation entries are present (`extra_l2_entries` is non-empty), the standard `result_entry` is NOT included in the L2 entry pairs. `convert_l1_entries_to_l2_pairs` skips the `result_entry` push. Including it would cause `ExecutionNotFound` because the `actionHash` of the result entry would conflict with Entry 0's continuation chain.

### 14f. Recursive Cross-Chain Discovery (Depth > 1)

The recursive discovery mechanism enables detection of arbitrarily deep L2-to-L1 and L1-to-L2 call chains, extending the depth-1 flash loan pattern (section 14b) to N-hop ping-pong patterns. A depth-1 interaction is a single L2-to-L1 call whose L1 execution produces at most one L1-to-L2 return call. Depth > 1 arises when the return call's L2 execution makes another L2-to-L1 call, whose L1 execution may produce another return call, and so on.

**Discovery algorithm**: The L2-to-L1 composer RPC (`composer_rpc/l2_to_l1.rs`) runs a Phase A/B alternating loop after initial call detection:

```
Phase A: Simulate L2-to-L1 calls on L1 (via simulate_l1_combined_delivery)
         -> discover L1-to-L2 return calls
Phase B: Simulate those return calls on L2 (via simulate_l2_return_call_delivery)
         -> discover nested L2-to-L1 calls
Repeat until no new calls are found or MAX_RECURSIVE_DEPTH=5 is reached.
```

The loop runs in two code paths:

1. **Multi-call path** (>= 2 detected L2-to-L1 calls from initial trace): Phase A simulates the current batch of L2-to-L1 calls via `simulate_l1_combined_delivery`. Phase B simulates newly discovered return calls on L2. New L2-to-L1 calls feed into the next Phase A iteration.

2. **Single-call path** (1 detected L2-to-L1 call): First simulates the single call on L1 via `simulate_l1_delivery`. If the L1 trace reveals return calls, the same Phase A/B loop runs. If nested L2-to-L1 calls are discovered, the single-call path **promotes** to the multi-call entry construction path (`queue_l2_to_l1_multi_call_entries`), since the original single call plus its nested descendants form a multi-call continuation.

**`simulate_l2_return_call_delivery`**: Simulates return calls on L2 to discover nested L2-to-L1 proxy calls. For each return call:

1. Trace the call directly (from=CCM, to=destination, data=calldata) using `debug_traceCall` with `callTracer`
2. Walk the trace tree via `walk_trace_tree_l2` to find calls targeting L2 `authorizedProxies` (L2-to-L1 proxy calls)
3. If the simple trace finds nothing (e.g., because an inner call reverts without entries), retry with `debug_traceCallMany` that pre-loads dummy entries via `loadExecutionTable`
4. Return discovered `DetectedL2InternalCall` entries for the next Phase A iteration

**Return call address routing (`is_return_call`)**: Entry construction in `build_l2_to_l1_continuation_entries` must distinguish L2-to-L1 children (standard direction) from L1-to-L2 return call children (reverse direction) when constructing `callReturn` entries and L1 trigger actions. The distinction is made by checking `child.call_action.rollup_id == our_rollup_id`:

- **L2-to-L1 child** (`rollup_id == 0`, targeting L1): The child's `source_address` is the L2 contract and `destination` is the L1 contract. For L2 `callReturn` entries, addresses are swapped: `callReturn.destination = child.source_address` (L2), `callReturn.source_address = child.destination` (L1). For L1 trigger entries, `trigger.destination = child.source_address` (proxy represents L2 source on L1).

- **L1-to-L2 return call child** (`rollup_id == our_rollup_id`, targeting L2): The child's `destination` is the L2 contract and `source_address` is the L1 contract. Addresses are used **without swapping**: `callReturn.destination = child.destination` (L2), `callReturn.source_address = child.source_address` (L1). For L1 trigger entries, `trigger.destination = child.destination` (proxy represents L2 destination on L1).

The rationale: for L2-to-L1 children, the L1 proxy is `proxy(L2_source, our_rollup_id)` -- the proxy represents the L2 source on L1. For L1-to-L2 return call children, the L1 proxy is `proxy(L2_destination, our_rollup_id)` -- the proxy represents the L2 destination on L1. The address that names the proxy is always the address that lives on L2 and is being represented on L1.

**Recursive entry generation**: `build_l2_to_l1_continuation_entries` uses `generate_l2_entries_recursive` and `push_reentrant_child_entries` to produce L2 and L1 entries for arbitrarily deep call trees. At every level, a call with children gets `callReturn{scope=[0]}` (or `scope=[N]` for additional children). Scope is always relative to the current `newScope()` context, so `scope=[0]` is correct regardless of absolute nesting depth. Each reentrant `executeCrossChainCall` starts its own scope tree.

**Entry count scaling**: For a depth-N ping-pong pattern (N L2-to-L1 calls + (N-1) L1-to-L2 return calls = 2N-1 hops):

| Component | Count | Formula |
|-----------|-------|---------|
| L2 table entries | 2N-1 + (N-1) scope exits | 3N-2 |
| L1 deferred entries | N triggers + (N-1) return entries + scope exits | Varies with tree shape |
| L1 trigger transactions | N | One per L2-to-L1 call |

**`MAX_RECURSIVE_DEPTH = 5`**: The loop runs at most 5 iterations of Phase A/B, supporting up to 5 rounds of L2-to-L1 calls (= 9 cross-chain hops). This is a safety bound; most practical patterns use depth 1-3.

**Backward compatibility**: Depth-1 patterns (flash loans from section 14b) work unchanged. The multi-call path runs one Phase A iteration that discovers return calls; Phase B returns empty (the return call's L2 execution does not make further L2-to-L1 calls), and the loop exits after one iteration. The single-call path with no return calls skips the loop entirely.

**Single-call promotion with return calls (issue #245)**: The single-call path promotes to multi-call entry construction when return calls are discovered, even if Phase B finds no nested L2-to-L1 calls. The promotion condition is `all_l2_calls.len() > 1 || !all_return_calls.is_empty()`. Without the return-call check, a depth-2 pattern with 1 L2-to-L1 call and 1 terminal return call (e.g., Logger → Logger → Counter, where Counter does not make further cross-chain calls) would fall through to the simple `initiateL2CrossChainCall` path, losing the return call entries entirely.

**Continuation entry routing (issue #245)**: The driver routes L1 deferred entries from `QueuedWithdrawal` to one of two queues: `pending_withdrawal_l1_entries` (pair-based, for simple L2-to-L1 calls) or `pending_continuation_l1_entries` (5-entry structure, for continuation patterns). The routing condition is `trigger_source.is_none()`: withdrawals queued via `initiateL2CrossChainCall` set `trigger_source: None`; continuations queued via `buildL2ToL1ExecutionTable` set `trigger_source: Some(source_address)`.

**Delivery return data in RESULT hashes (issues #245, #246)**: The `_processCallAtScope` function (docs/SYNC_ROLLUPS_PROTOCOL_SPEC.md line 497) builds a RESULT action with `data: returnData` from `executeOnBehalf`. When the delivery function returns non-empty data (e.g., `Logger.execute` returns `abi.encode(bytes(...))`), the RESULT hash must include that data — not `result_void`. Four sites in `build_l2_to_l1_continuation_entries` construct delivery RESULT entries:

1. **Root call delivery result** (first call, `root_pos == 0`): uses `l2_call.delivery_return_data`
2. **Scope resolution** (first call with children): uses `l2_call.delivery_return_data`
3. **Reentrant child scope** (`push_reentrant_child_entries`): uses `child.delivery_return_data`
4. **Subsequent call delivery** (calls after `root_pos == 0`): uses `l2_call.delivery_return_data`

Each site checks `delivery_return_data.is_empty()` and falls back to `result_void` for void functions.

**L2 scope resolution return data (`l2_return_data`)**: On L2, `_processCallAtScope` in the CCM calls `proxy.executeOnBehalf(destination, data)` for the return call. The RESULT hash depends on the raw return bytes from the destination. For functions returning data (e.g., `Counter.increment()` returns `uint256`), the L2 scope resolution entry must include that data. The `l2_return_data` field is:

1. **Captured** by the L2 proxy via `eth_call(from=proxy_address, to=destination, data=calldata)` before queuing entries. Using `from=proxy_address` ensures `msg.sender` matches the real `executeOnBehalf` context.
2. **Propagated** through `DetectedReturnCall.l2_return_data` → RPC JSON `l2ReturnData` field → `L2ReturnCall.l2_return_data` → `DetectedCall.l2_return_data`.
3. **Used** in `generate_l2_entries_recursive` for the scope resolution entry hash: `RESULT{rollupId=our_rollup_id, data=l2_return_data}` instead of `result_void(our_rollup_id)`.

For multi-child patterns, intermediate scope transitions use the *previous* child's `l2_return_data`, and the final scope resolution uses the *last* child's data.

**Simulation inner entries**: During `simulate_l1_delivery` iteration 2+, the simulation bundle includes a simple `CALL → RESULT_VOID` entry for each discovered return call. This forces the inner `executeCrossChainCall` in the simulation to return empty data (matching the real execution where inner entries also use `result_void`). Without these simulation entries, the inner call executes directly and may return different data (e.g., `Counter.increment()` returns `uint256(1)` without entries, but `bytes("")` with entries), causing a `delivery_return_data` mismatch.

**`reorder_for_swap_and_pop`**: The builder applies `reorder_for_swap_and_pop` to both the L2 and L1 entry arrays before submission. This function partitions entries into two groups: entries whose `action_hash` appears 2 or more times are placed first (with intra-group reordering to `[E0, E(N-1), ..., E1]` for correct FIFO consumption under swap-and-pop), followed by entries with unique action hashes. The rationale: the CCM uses swap-and-pop to remove consumed entries from its storage array, which shifts the last element into the consumed slot. Without this reordering, a consumed entry from one same-hash group could cause the swap-and-pop to displace an entry belonging to a different group, disrupting the relative order of subsequent same-hash entries and producing `ExecutionNotFound` at depth >= 3.

**`parent_call_index` override**: The builder must override `parent_call_index` on return calls after `simulate_l1_combined_delivery` returns, using the actual index in `all_l2_calls` rather than the function's internal `call_idx=0`. Without this, all return calls collapse onto call[0], producing sequential scopes instead of nested scopes — which breaks at depth >= 3 due to swap-and-pop disruption of 4+ same-hash entries.

**State delta assignment**: The driver assigns `currentState = clean_root` to the first trigger entry (the entry whose `actionHash` is not `result_void_hash`) and `currentState = speculative_root` to all other entries. This accounts for `reorder_for_swap_and_pop` moving RESULT entries to the front of the array — the first trigger entry may not be at index 0.

**`LOAD_TABLE_GAS_LIMIT`**: The `loadExecutionTable` protocol transaction uses a gas limit of 3,000,000, constraining the maximum number of entries per transaction to approximately 30. For depth-5 patterns (9 entries), this provides adequate headroom. Exceeding this limit causes the protocol transaction to revert, preventing entry loading for that block.

### 14g. PingPong Test Contract (Depth > 1 Reference)

The canonical test for recursive cross-chain discovery is the **PingPong** contract pair (`contracts/test-depth2/src/PingPongL2.sol`, `contracts/test-depth2/src/PingPongL1.sol`). PingPong demonstrates configurable-depth L2-to-L1 and L1-to-L2 alternation without token transfers.

**Call sequence for `start(maxRounds=N)`**:

```
1. User calls PingPongL2.start(N) on L2
   -> L2-to-L1: PingPongL1.ping(1, N)
2. L1 ping(1, N): round < N
   -> L1-to-L2 return: PingPongL2.pong(2, N)
3. L2 pong(2, N)
   -> L2-to-L1: PingPongL1.ping(2, N)
...
2k.   L1 ping(k, N): round < N -> L1-to-L2 return: pong(k+1, N)
2k+1. L2 pong(k+1, N) -> L2-to-L1: ping(k+1, N)
...
2N.   L1 ping(N, N): round == N -> done = true (terminal, no return call)
```

Total cross-chain hops: `2N - 1` (step 1 is the L2 trigger, steps 2-2N are cross-chain). Total L2-to-L1 calls: `N`. Total L1-to-L2 return calls: `N - 1`.

**Verification criteria**: After execution, `PingPongL2.pingCount == N` (one `start` + N-1 `pong` calls) and `PingPongL1.pongCount == N` and `PingPongL1.done == true`.
