//! Cross-chain composability types for synchronous rollup execution.
//!
//! These types mirror the Solidity structs in EEZ's `ICrossChainManager.sol`:
//! - `ActionType` / `Action` — represent cross-chain call/result/revert actions
//! - `StateDelta` — rollup state transitions (state root + ether balance changes)
//! - `ExecutionEntry` — a pre-computed execution table entry consumed by builder protocol transactions
//!
//! The execution flow:
//! 1. An off-chain prover pre-computes cross-chain state transitions
//! 2. Entries are posted to L1 via `Rollups.postBatch()` with a ZK/ECDSA proof
//! 3. On L2, the driver loads entries into `CrossChainManagerL2` via `loadExecutionTable()`
//! 4. L2 contracts interact through `CrossChainProxy` contracts, consuming table entries

use alloy_primitives::{Address, B256, Bytes, I256, U256, keccak256};
use alloy_rpc_types::Log;
use alloy_sol_types::{SolCall, SolEvent, SolType, sol};
use serde::{Deserialize, Serialize};
use tracing::warn;

// ──────────────────────────────────────────────
//  ABI bindings generated from EEZ contracts
// ──────────────────────────────────────────────

sol! {
    /// CrossChainManagerL2.loadExecutionTable(ExecutionEntry[] entries)
    #[derive(Debug, PartialEq)]
    interface ICrossChainManagerL2 {
        enum ActionType {
            CALL,
            RESULT,
            L2TX,
            REVERT,
            REVERT_CONTINUE
        }

        struct Action {
            ActionType actionType;
            uint256 rollupId;
            address destination;
            uint256 value;
            bytes data;
            bool failed;
            address sourceAddress;
            uint256 sourceRollup;
            uint256[] scope;
        }

        struct StateDelta {
            uint256 rollupId;
            bytes32 currentState;
            bytes32 newState;
            int256 etherDelta;
        }

        struct ExecutionEntry {
            StateDelta[] stateDeltas;
            bytes32 actionHash;
            Action nextAction;
        }

        function loadExecutionTable(ExecutionEntry[] calldata entries) external;
        function executeIncomingCrossChainCall(
            address destination,
            uint256 value,
            bytes calldata data,
            address sourceAddress,
            uint256 sourceRollup,
            uint256[] calldata scope
        ) external returns (bytes memory result);

        /// Rollups.BatchPosted event — emitted when execution entries are posted via postBatch().
        event BatchPosted(ExecutionEntry[] entries, bytes32 publicInputsHash);

        /// Rollups.ExecutionConsumed event — emitted when a deferred entry is consumed
        /// by a user's proxy call on L1 (executeCrossChainCall succeeds).
        event ExecutionConsumed(bytes32 indexed actionHash, Action action);

        /// CrossChainManagerL2.CrossChainCallExecuted — emitted when a proxy calls
        /// executeCrossChainCall on L2, indicating an outgoing L2→L1 cross-chain call.
        /// Used by receipt-based §4f filtering to identify L2→L1 txs generically.
        event CrossChainCallExecuted(bytes32 indexed actionHash, address indexed proxy, address sourceAddress, bytes callData, uint256 value);

        /// Rollups.postBatch — submit execution entries with proof to L1.
        /// Defined here (not on Rollups) so we can reuse the same type namespace.
        function postBatch(
            ExecutionEntry[] entries,
            uint256 blobCount,
            bytes callData,
            bytes proof
        );

        /// Protocol errors that occur when entries aren't loaded (simulation artifacts).
        /// These are NOT terminal failures — the delivery will succeed once entries are posted.
        error ExecutionNotFound();
        error ExecutionNotInCurrentBlock();
        error CallExecutionFailed();
        error InvalidRevertData();
    }
}

/// Check if revert data is a protocol simulation artifact (entry not loaded yet).
///
/// Simulation artifacts are reverts that only occur because entries aren't loaded
/// during delivery simulation. They disappear once entries are posted. Two patterns:
///
/// 1. **Selector-only reverts** (≤4 bytes): any parameterless error like
///    `ExecutionNotFound()`, `UnauthorizedCaller()`, etc. Real terminal failures
///    always carry ABI-encoded params (Error(string) ≥ 68 bytes, Panic(uint256) = 36
///    bytes, custom errors with data > 4 bytes).
///
/// 2. **Known protocol errors** with params: `ExecutionNotInCurrentBlock`,
///    `CallExecutionFailed`, `InvalidRevertData` — identified by typed selectors.
///
/// 3. **Wrapped errors**: any `error Xyz(bytes)` wrapper (e.g., proxy contracts wrap
///    inner revert data). Detected generically by attempting ABI decode of `(bytes)`,
///    then checking the inner data recursively. No contract-specific selectors needed.
pub fn is_simulation_artifact(data: &[u8]) -> bool {
    use alloy_sol_types::SolError;
    // Pattern 1: empty or selector-only (no ABI params)
    if data.len() <= 4 {
        return true;
    }
    // Pattern 2: known protocol errors (may have params in future versions)
    let sel = &data[..4];
    if sel == ICrossChainManagerL2::ExecutionNotFound::SELECTOR
        || sel == ICrossChainManagerL2::ExecutionNotInCurrentBlock::SELECTOR
        || sel == ICrossChainManagerL2::CallExecutionFailed::SELECTOR
        || sel == ICrossChainManagerL2::InvalidRevertData::SELECTOR
    {
        return true;
    }
    // Pattern 3: generic wrapper unwrap — try to ABI-decode as (bytes) and recurse.
    // Catches any error that wraps inner revert data, regardless of the wrapper's
    // selector (e.g., Bridge's ProxyCallFailed, or any future wrapper contract).
    // ABI layout: 4-byte selector + abi.encode(bytes) = offset(32) + length(32) + data
    if data.len() > 68 {
        // Try to decode the params after the 4-byte selector as a single `bytes` field.
        // ABI: [selector(4)][offset(32)][length(32)][data(length)]
        let params = &data[4..];
        if let Some(len_bytes) = params.get(32..64) {
            let inner_len =
                alloy_primitives::U256::from_be_slice(len_bytes).saturating_to::<usize>();
            if inner_len > 0 && params.len() >= 64 + inner_len {
                let inner = &params[64..64 + inner_len];
                return is_simulation_artifact(inner);
            }
        }
    }
    false
}

sol! {
    /// Bridge contract ABI bindings (protocol transactions only).
    interface IBridge {
        function initialize(address _manager, uint256 _rollupId, address _admin) external;
        function setCanonicalBridgeAddress(address bridgeAddress) external;
    }
}

sol! {
    /// Read-only view on the Bridge contract for querying canonical address.
    interface IBridgeView {
        function canonicalBridgeAddress() external view returns (address);
    }
}

// ──────────────────────────────────────────────
//  Rust-native types for driver/derivation use
// ──────────────────────────────────────────────

/// Consumed events map: actionHash → Vec of consumed CALL actions.
/// Vec preserves duplicate consumed events with the same actionHash
/// (e.g., CallTwice calling increment() twice produces 2 events with same hash).
pub type ConsumedMap = std::collections::HashMap<ActionHash, Vec<CrossChainAction>>;

/// Check if a set of cross-chain calls contains duplicates (same action identity).
/// Uses full 4-tuple (destination, calldata, value, sourceAddress) matching the
/// fields that compose the actionHash. Direction-agnostic — used by both
/// L1→L2 and L2→L1 composer RPCs.
pub fn has_duplicate_calls(calls: &[(Address, &[u8], U256, Address)]) -> bool {
    let mut seen = std::collections::HashMap::new();
    for (dest, data, value, source) in calls {
        let count = seen
            .entry((*dest, *data, *value, *source))
            .or_insert(0usize);
        *count += 1;
        if *count > 1 {
            return true;
        }
    }
    false
}

/// Address where CrossChainManagerL2 is predeployed on L2.
pub const CROSS_CHAIN_MANAGER_L2_ADDRESS: Address = Address::new([
    0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x03,
]);

// ──────────────────────────────────────────────
//  RollupId newtype (refactor PLAN step 1.1a)
//
//  A zero-cost wrapper over `U256` that makes rollup identifiers a
//  distinct type at the compile-time level. Exists to prevent two
//  classes of bugs:
//    1. Passing a generic `U256` where a rollup id is expected (or
//       vice versa — e.g. calling with a block number).
//    2. Uncontrolled construction from raw bytes: every non-trivial
//       path that turns raw bytes / topics / decoded ABI values into
//       a `RollupId` must go through an explicit `from_*_boundary`
//       function so that `grep from_.*_boundary` reveals every entry
//       point a human auditor needs to review.
//
//  **Step 1.1a deliberately does NOT migrate call sites.** The type
//  is introduced alongside the existing `U256`-typed fields (e.g.
//  `CrossChainAction::rollup_id: U256`). Callsite migration happens in
//  step 1.1b on a dedicated branch because it touches >20 files.
// ──────────────────────────────────────────────

/// Rollup identifier — the `rollupId` field of `ICrossChainManager.Action`.
///
/// A newtype wrapper over [`U256`]. Construction is controlled:
/// - Internal code uses [`RollupId::new`] (module-private `pub(crate)`)
/// - ABI-decoded values use [`RollupId::from_abi_boundary`]
/// - Log topics use [`RollupId::from_log_boundary`]
/// - Raw bytes use [`RollupId::from_bytes_at_boundary`]
///
/// Every `from_*_boundary` function is grep-able. Auditors can list
/// every uncontrolled construction with `rg 'from_.*_boundary'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
pub struct RollupId(U256);

impl RollupId {
    /// Mainnet (L1) rollup ID — conventionally `U256::ZERO` in the
    /// sync-rollups-protocol spec. Available as a `const` so it can
    /// appear in pattern matches and `if` expressions.
    pub const MAINNET: Self = Self(U256::ZERO);

    /// Module-private constructor for internal use.
    ///
    /// Prefer the explicit `from_*_boundary` functions for any
    /// construction that crosses a serialization / decoding boundary.
    /// This constructor exists for purely-internal code paths (e.g.
    /// tests, helpers that already have a validated `U256`).
    pub(crate) fn new(value: U256) -> Self {
        Self(value)
    }

    /// Construct from a `U256` value decoded at an ABI boundary — for
    /// example, the `rollupId` field of a `sol!`-decoded struct or
    /// the return value of an `eth_call` against a Solidity getter.
    ///
    /// Grep `from_abi_boundary` to audit every ABI decode path that
    /// produces a rollup id.
    pub fn from_abi_boundary(value: U256) -> Self {
        Self(value)
    }

    /// Construct from a log topic (`B256`) taken from an on-chain event.
    /// The topic is interpreted as big-endian 32 bytes.
    ///
    /// Grep `from_log_boundary` to audit every log parsing path.
    pub fn from_log_boundary(topic: B256) -> Self {
        Self(U256::from_be_bytes(topic.0))
    }

    /// Construct from raw bytes at a serialization boundary. Accepts
    /// any slice up to 32 bytes; shorter slices are interpreted as
    /// big-endian with left-padding zeros (the same convention as
    /// `U256::from_be_slice`). Slices longer than 32 bytes are
    /// truncated from the right (low-order bytes preserved).
    ///
    /// Grep `from_bytes_at_boundary` to audit every raw-bytes path.
    pub fn from_bytes_at_boundary(bytes: &[u8]) -> Self {
        let mut buf = [0u8; 32];
        let len = bytes.len().min(32);
        // Copy the last `len` bytes of the input into the rightmost
        // `len` bytes of the 32-byte buffer (big-endian padding).
        buf[32 - len..].copy_from_slice(&bytes[bytes.len() - len..]);
        Self(U256::from_be_bytes(buf))
    }

    /// Return the underlying `U256` value. Used at ABI encode boundaries
    /// and in internal comparisons that have not been migrated yet.
    pub fn as_u256(&self) -> U256 {
        self.0
    }

    /// Return the underlying value as a `u64`, or `None` if it does
    /// not fit. Every current rollup id in the codebase is small, but
    /// the on-chain type is `uint256` so we encode the possibility of
    /// overflow explicitly.
    pub fn to_u64(&self) -> Option<u64> {
        if self.0 <= U256::from(u64::MAX) {
            Some(self.0.to::<u64>())
        } else {
            None
        }
    }

    /// Return `true` if this is the mainnet / L1 rollup id.
    pub fn is_mainnet(&self) -> bool {
        self.0 == U256::ZERO
    }
}

impl std::fmt::Display for RollupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_mainnet() {
            write!(f, "MAINNET")
        } else {
            write!(f, "rollup-{}", self.0)
        }
    }
}

// ──────────────────────────────────────────────
//  ScopePath newtype (refactor PLAN step 1.1c)
//
//  A zero-cost wrapper over `Vec<U256>` that makes a cross-chain
//  execution scope a distinct type at the compile-time level. The
//  protocol (`_resolveScopes` in `CrossChainManagerL2`) represents a
//  scope as the indexed path taken into a call tree — each element is
//  a child index, `[]` means "root", `[0]` means "first child of the
//  root", `[0, 1]` means "second child of the first child", and so
//  on. Treating this as a generic `Vec<U256>` made it easy to pass a
//  scope where a plain list was expected (and vice versa); the newtype
//  closes that at the type level.
//
//  **Helpers `enter` / `exit` carry zero current callers in the
//  codebase** — every scope is built as a literal or cloned by value.
//  They are included per PLAN §8 step 1.1c spec because the protocol
//  (and future builder refactors in 1.9b / 3.x) treats a scope as a
//  tree-walk stack. Having domain-named `enter` / `exit` available
//  when those call sites land avoids a mid-refactor API expansion.
// ──────────────────────────────────────────────

/// Scope path for a cross-chain execution entry — the indexed path
/// from the root of a call tree to a specific descendant. Wraps a
/// `Vec<U256>` with domain semantics so arbitrary `Vec<U256>` values
/// cannot be passed where a scope is expected.
///
/// Construction:
/// - [`ScopePath::root`] — empty path, the root of a call tree.
/// - [`ScopePath::from_index`] — single child index, e.g. `[0]`.
/// - [`ScopePath::from_parts`] — at ABI / deserialization boundaries.
///
/// Mutation (tree-walk semantics, per PLAN §8 step 1.1c):
/// - [`ScopePath::enter`] — push a child index (enter a subtree).
/// - [`ScopePath::exit`] — pop the current child index (return to
///   parent).
///
/// Borrow / conversion:
/// - [`ScopePath::as_slice`] — read-only view as `&[U256]`, used at
///   ABI encode boundaries and inside equality checks.
/// - [`ScopePath::into_inner`] — owning conversion back to
///   `Vec<U256>` for wire-format types and sol! structs that still
///   hold a raw `Vec<U256>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct ScopePath(Vec<U256>);

impl ScopePath {
    /// The root of a call tree — an empty scope path.
    pub fn root() -> Self {
        Self(Vec::new())
    }

    /// A single-element scope path `[idx]`. Shorthand for the very
    /// common "first child of the root" pattern that shows up in
    /// almost every L2→L1 / L1→L2 continuation entry.
    pub fn from_index(idx: U256) -> Self {
        Self(vec![idx])
    }

    /// Construct a scope from a raw `Vec<U256>` at a boundary — ABI
    /// decode, JSON deserialization, sol! → Rust conversion. Every
    /// non-literal construction from an untyped value must go
    /// through here so auditors can `grep ScopePath::from_parts` to
    /// list every entry point.
    pub fn from_parts(parts: Vec<U256>) -> Self {
        Self(parts)
    }

    /// Enter a subtree by pushing a child index onto the path.
    ///
    /// Currently has no callers — see the module comment above. This
    /// helper exists per PLAN §8 step 1.1c spec so future builder
    /// refactors do not need to expand the public API mid-refactor.
    pub fn enter(&mut self, idx: U256) {
        self.0.push(idx);
    }

    /// Exit the current subtree by popping the last index from the
    /// path. Returns the popped index, or `None` if already at the
    /// root. Currently has no callers (see `enter`).
    pub fn exit(&mut self) -> Option<U256> {
        self.0.pop()
    }

    /// Borrow the scope as a raw slice — used at ABI encode
    /// boundaries (`to_sol_action`, `abi_encode`) and inside internal
    /// equality / length checks.
    pub fn as_slice(&self) -> &[U256] {
        &self.0
    }

    /// Consume the scope and return the inner `Vec<U256>`. Used
    /// when crossing into wire-format types (`SerializableAction`,
    /// `ICrossChainManagerL2::Action`) that still hold a raw
    /// `Vec<U256>`.
    pub fn into_inner(self) -> Vec<U256> {
        self.0
    }

    /// Number of elements in the scope path. Zero means "root".
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `true` if this is the root path (no children entered).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for ScopePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            write!(f, "root")
        } else {
            write!(f, "[")?;
            for (i, idx) in self.0.iter().enumerate() {
                if i > 0 {
                    write!(f, ",")?;
                }
                write!(f, "{}", idx)?;
            }
            write!(f, "]")
        }
    }
}

// ──────────────────────────────────────────────
//  State-root newtypes (refactor PLAN step 1.2)
//
//  Four zero-cost wrappers over `B256` that distinguish the four
//  semantically distinct hash slots in the cross-chain protocol:
//
//    - `CleanStateRoot`        — the post-block state root WITHOUT
//                                cross-chain entries applied; what L1
//                                sees as the rollup's current state.
//    - `SpeculativeStateRoot`  — the post-block state root WITH
//                                cross-chain entries applied; what
//                                reth holds locally before postBatch
//                                lands.
//    - `NewStateRoot`          — the per-entry "new state" produced
//                                by a single state delta (the
//                                `newState` field of a `StateDelta`).
//    - `ActionHash`            — `keccak256(abi.encode(action))`,
//                                the lookup key for execution entries.
//
//  The PLAN groups them in step 1.2 because they all close invariant
//  #3 ("NEVER align state roots by overwriting `pre_state_root`") and
//  related anti-patterns. The most load-bearing of the four is
//  [`CleanStateRoot`]: making it impossible to construct one outside
//  the canonical `compute_intermediate_roots` path means you can no
//  longer paper over a divergence by writing a freshly-computed value
//  back into the slot that L1 sees.
//
//  ## Migration scope (step 1.2 partial)
//
//  Per PLAN §8 (option B from the user check-in), this step:
//
//    1. Introduces all four newtypes with their boundary constructors
//       (this commit).
//    2. Migrates [`CleanStateRoot`] (7 callsites — closes invariant #3
//       at the type level).
//    3. Migrates [`ActionHash`] (183 callsites — biggest mechanical
//       cascade, closes the "passing a state root where an action
//       hash is expected" class of bugs).
//    4. Leaves [`SpeculativeStateRoot`] and [`NewStateRoot`] as
//       scaffolding only — the field name already discriminates them
//       (`state_root` vs `clean_state_root` on `PendingBlock`,
//       `current_state` vs `new_state` on `CrossChainStateDelta`),
//       so cascade cost outweighs marginal benefit until a future
//       refactor needs to disambiguate them.
//
//  When step 1.2b lands a real caller for the deferred two, the
//  scaffolding is ready and migration is mechanical.
//
//  ## Boundary discipline
//
//  Every newtype follows the same shape (mirrors `RollupId` from
//  step 1.1a):
//
//    - `pub(crate) fn new(B256) -> Self` — internal-only constructor
//      for code that already has a validated value.
//    - `pub fn from_abi_boundary(B256) -> Self` — explicit ABI decode
//      entry point. Grep `from_abi_boundary` to audit every site.
//    - `pub fn from_log_boundary(B256) -> Self` — explicit event-log
//      decode entry point.
//    - `pub fn as_b256(&self) -> B256` — read-only view for ABI
//      encode boundaries and equality checks against raw `B256`.
// ──────────────────────────────────────────────

/// The post-block state root WITHOUT cross-chain entries applied.
///
/// This is what L1 sees as the rollup's canonical state — the
/// `currentState` field of the next batch's `postBatch` call. It is
/// computed by trial-executing every non-trigger transaction in the
/// block and reading the resulting state root, effectively rolling
/// back any cross-chain entry effects.
///
/// **Closes invariant #3** ("NEVER align state roots by overwriting
/// `pre_state_root`") at the type level: a `CleanStateRoot` cannot
/// be constructed from a freshly-computed `B256` outside the
/// canonical `compute_intermediate_roots` path (which uses
/// `pub(crate) new`). Code that wants to "make a mismatch go away"
/// by assigning a different value into the slot will not compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
#[repr(transparent)]
pub struct CleanStateRoot(B256);

impl CleanStateRoot {
    /// Module-private constructor. Only callable from inside the
    /// `cross_chain` module and its sibling tests. The canonical
    /// production caller is `Driver::compute_intermediate_roots`,
    /// which trial-executes the block and pulls `roots[0]`.
    pub(crate) fn new(value: B256) -> Self {
        Self(value)
    }

    /// Construct from a `B256` decoded at an ABI boundary — for
    /// example, the `stateRoot` field returned by the
    /// `Rollups.rollups(rollupId)` view call.
    pub fn from_abi_boundary(value: B256) -> Self {
        Self(value)
    }

    /// Construct from a `B256` taken from a log topic.
    pub fn from_log_boundary(value: B256) -> Self {
        Self(value)
    }

    /// Read-only view of the underlying hash. Used at ABI encode
    /// boundaries and inside equality checks against raw `B256`.
    pub fn as_b256(&self) -> B256 {
        self.0
    }
}

impl std::fmt::Display for CleanStateRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The post-block state root WITH cross-chain entries applied.
///
/// This is what reth holds locally as the canonical block state
/// after executing every transaction (including the protocol txs
/// that consume cross-chain entries). It is the value of
/// `BuiltBlock.state_root` and `PendingBlock.state_root`.
///
/// **Scaffolding only in step 1.2.** No fields are migrated to this
/// type yet — see the module comment above for the rationale. The
/// type exists so a future step (1.2b or later) can flip
/// `BuiltBlock.state_root` and propagate without an API change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
#[repr(transparent)]
#[allow(dead_code)]
pub struct SpeculativeStateRoot(B256);

#[allow(dead_code)]
impl SpeculativeStateRoot {
    pub(crate) fn new(value: B256) -> Self {
        Self(value)
    }
    pub fn from_abi_boundary(value: B256) -> Self {
        Self(value)
    }
    pub fn from_log_boundary(value: B256) -> Self {
        Self(value)
    }
    pub fn as_b256(&self) -> B256 {
        self.0
    }
}

/// The "new state" produced by a single cross-chain state delta.
///
/// Distinct from [`CleanStateRoot`] because a `NewStateRoot` is the
/// per-entry intermediate result inside a chain of deltas, not the
/// post-block rollback state. In the chain
/// `clean → root1 → root2 → ... → speculative`, every arrow's RHS
/// is a `NewStateRoot` (and the next arrow's LHS).
///
/// **Scaffolding only in step 1.2.** Field name (`new_state`) already
/// discriminates from `current_state`. Migration deferred per the
/// module comment above.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
#[repr(transparent)]
#[allow(dead_code)]
pub struct NewStateRoot(B256);

#[allow(dead_code)]
impl NewStateRoot {
    pub(crate) fn new(value: B256) -> Self {
        Self(value)
    }
    pub fn from_abi_boundary(value: B256) -> Self {
        Self(value)
    }
    pub fn from_log_boundary(value: B256) -> Self {
        Self(value)
    }
    pub fn as_b256(&self) -> B256 {
        self.0
    }
}

/// `keccak256(abi.encode(action))` — the lookup key the on-chain
/// `_consumeExecution` function uses to find the execution entry
/// matching a given cross-chain action.
///
/// Distinct from any state root: an `ActionHash` identifies a
/// specific action (`CALL(target, data, ...)`, `RESULT(...)`, etc.)
/// and is computed by hashing the ABI-encoded `Action` struct, not
/// by trial-executing a block.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
#[repr(transparent)]
pub struct ActionHash(B256);

impl ActionHash {
    /// Module-private constructor. The canonical caller is
    /// `table_builder::compute_action_hash` (and its in-module
    /// equivalents in `cross_chain.rs`), which keccaks an
    /// ABI-encoded `Action`.
    pub(crate) fn new(value: B256) -> Self {
        Self(value)
    }

    /// Construct from a `B256` decoded at an ABI boundary — for
    /// example, the `actionHash` field of an
    /// `ICrossChainManagerL2::ExecutionEntry` decoded from a
    /// `BatchPosted` event.
    pub fn from_abi_boundary(value: B256) -> Self {
        Self(value)
    }

    /// Construct from an event-log topic.
    pub fn from_log_boundary(value: B256) -> Self {
        Self(value)
    }

    /// Read-only view of the underlying hash. Used at ABI encode
    /// boundaries and inside equality checks against raw `B256`.
    pub fn as_b256(&self) -> B256 {
        self.0
    }

    /// The all-zero `ActionHash`, used as a sentinel by some
    /// pre-execution and immediate-entry helpers (entries whose
    /// `actionHash == 0` are immediate, not lookup-keyed).
    pub const ZERO: Self = Self(B256::ZERO);
}

impl std::fmt::Display for ActionHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ──────────────────────────────────────────────
//  ParentLink + AbsoluteCallIndex (refactor PLAN step 1.3)
//
//  Two zero-cost wrappers that replace the `parent_call_index:
//  Option<usize>` field used by every detected-call struct
//  (`DetectedCall`, `L1DetectedCall`, `L2ReturnCall`, and the wire
//  twins in `rpc.rs` / `composer_rpc/`). The protocol distinguishes
//  two parent-link kinds:
//
//    - **`Root`**  — the call is a top-level cross-chain call. It
//                    has no parent in the call tree.
//    - **`Child(idx)`** — the call was made as a side-effect of an
//                    earlier call's L2 execution; `idx` points at
//                    that parent call in the absolute (post-rebase)
//                    `all_l2_calls` slice.
//
//  Modeling this as a generic `Option<usize>` made it easy to:
//
//    1. Pass an unrelated index (e.g. a block number, a vec index
//       into a different slice) where a parent index was expected.
//    2. Treat `None` and `Some(0)` as semantically equivalent.
//    3. Forget to rebase the index after `simulate_l1_combined_delivery`
//       (which assigns `call_idx=0` relative to its single-call
//       slice — see CLAUDE.md "Parent Call Index After Combined
//       Simulation"). The newtype gives us a single grep-able
//       `from_usize_at_boundary` site to audit every rebase.
//
//  ## Wire compatibility
//
//  `#[serde(into / from)]` makes both types serialize as the same
//  JSON shape they had before:
//
//    - `ParentLink`        ↔ `Option<usize>` — `Root` ↔ `null`,
//                            `Child(i)` ↔ `i`.
//    - `AbsoluteCallIndex` ↔ `usize` — flat number, no wrapper
//                            object.
//
//  Persisted state and in-flight RPC payloads are byte-identical to
//  the pre-1.3 wire format.
//
//  ## Closes invariant #7 (partial)
//
//  PLAN §6 invariant #7: "`parent_call_index` MUST be rebased after
//  `simulate_l1_combined_delivery`". The full closure (single helper
//  `rebase_parent_links`) lands in step 3.3 once the composer is
//  unified behind a `Direction` trait. Step 1.3 introduces the type
//  so that:
//
//    - The compiler distinguishes "parent link in the original
//      detected slice" from "parent link in `all_l2_calls`".
//    - Every assignment that crosses that boundary must go through
//      a named conversion (`from_usize_at_boundary`), making the
//      grep audit trivial.
// ──────────────────────────────────────────────

/// Index into the *absolute* `all_l2_calls` slice — the post-rebase
/// position of a call across the entire batch, distinct from a
/// position within any single-call simulation slice.
///
/// Construction follows the now-standard newtype shape:
///
/// - [`AbsoluteCallIndex::new`] — `pub(crate)` constructor for
///   internal code that already has a validated index.
/// - [`AbsoluteCallIndex::from_usize_at_boundary`] — explicit
///   "I am crossing a slice boundary" constructor. Grep
///   `from_usize_at_boundary` to audit every rebase point in the
///   composer.
/// - [`AbsoluteCallIndex::as_usize`] — read-only view for arithmetic
///   and comparisons.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
#[repr(transparent)]
pub struct AbsoluteCallIndex(usize);

impl AbsoluteCallIndex {
    /// Module-private constructor. Internal code that already has a
    /// validated absolute index uses this directly.
    pub(crate) fn new(value: usize) -> Self {
        Self(value)
    }

    /// Construct from a `usize` at a slice boundary — i.e. when an
    /// index is being rebased from one slice into the absolute
    /// `all_l2_calls` slice. Grep `from_usize_at_boundary` to audit
    /// every rebase point.
    pub fn from_usize_at_boundary(value: usize) -> Self {
        Self(value)
    }

    /// Read-only view of the underlying `usize`.
    pub fn as_usize(&self) -> usize {
        self.0
    }
}

impl std::fmt::Display for AbsoluteCallIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Parent link for a detected cross-chain call.
///
/// Replaces the previous `Option<usize>` representation so the
/// "no parent (root call)" and "child of call N" cases are distinct
/// at the type level.
///
/// `#[serde(into / try_from = "Option<usize>")]` keeps the on-the-wire
/// JSON identical to the pre-1.3 representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ParentLink {
    /// A top-level cross-chain call with no parent.
    #[default]
    Root,
    /// A child call made by the L2 execution of an earlier call.
    /// `idx` points into the absolute `all_l2_calls` slice.
    Child(AbsoluteCallIndex),
}

impl ParentLink {
    /// Construct from an `Option<usize>` (typically deserialized JSON
    /// or a legacy field). `None` becomes `Root`, `Some(i)` becomes
    /// `Child(AbsoluteCallIndex(i))`.
    pub fn from_option(opt: Option<usize>) -> Self {
        match opt {
            None => Self::Root,
            Some(i) => Self::Child(AbsoluteCallIndex::from_usize_at_boundary(i)),
        }
    }

    /// Convert back to an `Option<usize>` for the wire / sol! /
    /// legacy boundary.
    pub fn to_option(self) -> Option<usize> {
        match self {
            Self::Root => None,
            Self::Child(idx) => Some(idx.as_usize()),
        }
    }

    /// `true` iff this call has no parent (top-level call).
    pub fn is_root(&self) -> bool {
        matches!(self, Self::Root)
    }

    /// `true` iff this call has a parent.
    pub fn is_child(&self) -> bool {
        matches!(self, Self::Child(_))
    }

    /// Return the parent index if this is a child call.
    pub fn child_index(&self) -> Option<AbsoluteCallIndex> {
        match self {
            Self::Root => None,
            Self::Child(idx) => Some(*idx),
        }
    }

    /// Update the parent index in place. If currently `Root`, this
    /// transitions to `Child`. Used at the rebase site after
    /// `simulate_l1_combined_delivery` collapses children onto
    /// `call_idx=0`.
    pub fn set_child(&mut self, idx: AbsoluteCallIndex) {
        *self = Self::Child(idx);
    }

    /// Mutably borrow the inner index if this is a child. Used by
    /// the few sites that need to update the index in place
    /// (`if let Some(ref mut idx) = rc.parent_call_index { *idx = … }`).
    pub fn child_index_mut(&mut self) -> Option<&mut AbsoluteCallIndex> {
        match self {
            Self::Root => None,
            Self::Child(idx) => Some(idx),
        }
    }
}

// Wire compatibility: serialize as `Option<usize>`. The transparent
// `Serialize` cannot be derived for an enum, so we go through
// `Option<usize>` explicitly via serde's `into` / `try_from`.
impl serde::Serialize for ParentLink {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_option().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ParentLink {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<usize>::deserialize(deserializer).map(ParentLink::from_option)
    }
}

// ──────────────────────────────────────────────
//  TxOutcome + EntryGroupMode (refactor PLAN step 1.4)
//
//  Two semantic enums that replace boolean fields whose meaning was
//  not visible at the call site. The refactor target was named in
//  PLAN §6 invariants and §8 step 1.4: every place that branches on
//  `tx_reverts: bool` and `l1_independent_entries: bool` should have
//  a self-documenting type, not a true/false flag.
//
//  ## What this step does NOT introduce
//
//  PLAN §8 1.4 also names a third internal helper enum:
//
//      enum EntryClass { Trigger, Continuation, Result, RevertContinue }
//
//  As of step 1.4 there is no single concentrated call site that
//  benefits from a 4-way classification. The closest candidate is
//  `partition_entries` (which currently does a 2-way split into
//  `(table_entries, trigger_entries)`) and the `entry_counts` loop
//  in `driver.rs` (which already filters by 3 different criteria
//  inline). Neither is a clean win for an `EntryClass` switch.
//
//  Per the same partial-migration discipline used in step 1.2 (where
//  `SpeculativeStateRoot` and `NewStateRoot` were left as scaffolding
//  rather than fake migrations), `EntryClass` is **deferred to a
//  later step** that has a real consumer. Likely candidates:
//
//    - 1.4b (`QueuedCallRequest::{Simple, WithContinuations}`) — if
//      the dispatch needs to discriminate continuation entries.
//    - 2.4 (`ProtocolTxPlan` with stages) — if the planner needs to
//      tag entries by class as it iterates.
//    - 2.5 (`VerificationDecision`) — if the verify path needs to
//      classify by entry kind.
//
//  Adding it as dead-code scaffolding now would violate the CLAUDE.md
//  rule "Don't create helpers, utilities, or abstractions for one-time
//  operations" and would not make the code any more correct.
// ──────────────────────────────────────────────

/// Outcome of an L2 user transaction that initiated cross-chain calls.
///
/// Replaces the previous `tx_reverts: bool` field, which was
/// notorious for being read backwards (`if tx_reverts { … }` is the
/// "the user's tx FAILED" branch, but at glance it can read like
/// "the tx works in some reverted state"). The named variants make
/// the intent obvious at every call site:
///
/// - **`Success`** — the L2 tx executed all its cross-chain calls
///   and committed the changes. Entries are processed normally.
/// - **`Revert`** — the L2 tx made cross-chain calls and then
///   reverted. The L1 entries must include `REVERT` /
///   `REVERT_CONTINUE` actions so the protocol can roll back the
///   side effects (spec §D.12).
///
/// Wire compatibility: `#[serde(into / from = "bool")]` keeps the
/// JSON representation byte-identical (`true` = `Revert`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TxOutcome {
    /// The L2 user transaction succeeded after making cross-chain
    /// calls. The corresponding boolean is `false`.
    #[default]
    Success,
    /// The L2 user transaction reverted after making cross-chain
    /// calls. Triggers the spec §D.12 `REVERT` / `REVERT_CONTINUE`
    /// path. The corresponding boolean is `true`.
    Revert,
}

impl TxOutcome {
    /// Construct from a `bool` at a JSON / wire boundary. `false`
    /// becomes `Success`, `true` becomes `Revert`.
    pub fn from_bool(reverts: bool) -> Self {
        if reverts {
            Self::Revert
        } else {
            Self::Success
        }
    }

    /// Convert back to the legacy `tx_reverts: bool` representation.
    pub fn as_bool(self) -> bool {
        matches!(self, Self::Revert)
    }

    /// `true` iff the L2 tx reverted after making cross-chain calls.
    pub fn is_revert(&self) -> bool {
        matches!(self, Self::Revert)
    }

    /// `true` iff the L2 tx succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }
}

impl serde::Serialize for TxOutcome {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_bool().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for TxOutcome {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        bool::deserialize(deserializer).map(TxOutcome::from_bool)
    }
}

/// State-delta chaining mode for a group of L1 deferred entries.
///
/// Replaces the previous `l1_independent_entries: bool` field. The
/// boolean was used to distinguish two structurally distinct entry
/// arrangements:
///
/// - **`Chained`** — every entry's `currentState` matches the
///   previous entry's `newState`, forming a chain
///   `clean → root1 → root2 → … → speculative`. This is the default
///   for normal cross-chain entries (deposits, withdrawals, multi-call
///   continuations).
/// - **`Independent`** — every entry in the group sees the SAME
///   `currentState` (the pre-revert root). Used for L1→L2 partial
///   revert patterns where the L1 try/catch rolls back the
///   reverted call's state, so subsequent entries must observe the
///   pre-revert state, not the chained intermediate root.
///
/// Wire compatibility: `#[serde(into / from = "bool")]` keeps the
/// JSON representation byte-identical (`true` = `Independent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EntryGroupMode {
    /// Entries form a chained delta sequence
    /// (`prev.newState == curr.currentState`). The default. Boolean
    /// equivalent: `false`.
    #[default]
    Chained,
    /// Entries are independent — each sees the same pre-revert
    /// `currentState`. Used for L1→L2 partial-revert patterns.
    /// Boolean equivalent: `true`.
    Independent,
}

impl EntryGroupMode {
    /// Construct from a `bool` at a JSON / wire boundary. `false`
    /// becomes `Chained`, `true` becomes `Independent`.
    pub fn from_bool(independent: bool) -> Self {
        if independent {
            Self::Independent
        } else {
            Self::Chained
        }
    }

    /// Convert back to the legacy `l1_independent_entries: bool`
    /// representation.
    pub fn as_bool(self) -> bool {
        matches!(self, Self::Independent)
    }

    /// `true` iff entries in this group are independent (override
    /// chained currentState with the pre-revert root).
    pub fn is_independent(&self) -> bool {
        matches!(self, Self::Independent)
    }

    /// `true` iff entries in this group form a chained delta
    /// sequence.
    pub fn is_chained(&self) -> bool {
        matches!(self, Self::Chained)
    }
}

impl serde::Serialize for EntryGroupMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_bool().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for EntryGroupMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        bool::deserialize(deserializer).map(EntryGroupMode::from_bool)
    }
}

// ──────────────────────────────────────────────
//  CallOrientation (refactor PLAN step 1.9a)
//
//  Distinguishes a **forward** L2→L1 child call from a **return**
//  L1→L2 child call. The two kinds have opposite (destination,
//  source) pairings when building their L1 trigger entries, and
//  swapping the pair by mistake was a real class of bug:
//
//  - **Forward** L2→L1 children: the proxy on L1 represents the L2
//    *source* (e.g., Bridge_L2). The L1 *destination* is the L1
//    contract that the L2 source is calling (e.g., Bridge_L1
//    receiving withdrawn tokens). Trigger entry pair:
//
//        (trigger_dest, trigger_source) = (source_address, destination)
//
//    i.e. **swap** the CALL action's (destination, source).
//
//  - **Return** L1→L2 children: the proxy on L1 represents the L2
//    *destination* (e.g., PingPongL2). The L1 *source_address* is
//    the L1 contract that called the proxy. Trigger entry pair:
//
//        (trigger_dest, trigger_source) = (destination, source_address)
//
//    i.e. **do not swap** — preserve the CALL action's pair.
//
//  ## Closes invariant #19 (more explicitly)
//
//  Pre-1.9a, the swap logic was duplicated at three call sites in
//  `table_builder.rs`, each with its own `let is_return_call =
//  child.call_action.rollup_id == our_rollup_id;` followed by an
//  `if is_return_call` block. Copy-pasting or refactoring any of
//  these sites was brittle — it took one inverted `if` to
//  reintroduce the CLAUDE.md "Return Call Address Direction" bug
//  that cost hours to diagnose during the PingPong depth-2 debug.
//
//  Post-1.9a, the swap lives in exactly one place:
//  [`CallOrientation::address_pair_for`]. The three former sites
//  call this helper and pass the orientation computed by
//  [`CallOrientation::from_child`]. A future bug in the swap logic
//  is a single-site edit that the test suite catches (the mirror
//  tests + the PingPong-depth2 property test exercise both
//  orientations). A future bug in the ORIENTATION detection (the
//  `rollup_id == our_rollup_id` heuristic) is also a single-site
//  edit.
// ──────────────────────────────────────────────

/// Orientation of a child cross-chain call inside a multi-depth
/// pattern. See the module comment above for the precise semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallOrientation {
    /// L2→L1 child call. The child's CALL action targets L1
    /// (`rollup_id = RollupId::MAINNET`) and originates from an
    /// L2 contract. Trigger entries **swap** (dest, source).
    Forward,
    /// L1→L2 return call child. The child's CALL action targets
    /// our L2 rollup (`rollup_id == our_rollup_id`) and originates
    /// from an L1 contract. Trigger entries **do not swap** —
    /// preserve the CALL action's (dest, source) pair.
    Return,
}

impl CallOrientation {
    /// Detect the orientation from a child's `CrossChainAction
    /// .rollup_id`. This is the canonical gate — **every** site
    /// that used to write `is_return_call = child.rollup_id ==
    /// our_rollup_id` must go through this helper so the detection
    /// rule lives in one place.
    pub fn from_child(child_rollup_id: RollupId, our_rollup_id: RollupId) -> Self {
        if child_rollup_id == our_rollup_id {
            Self::Return
        } else {
            Self::Forward
        }
    }

    /// Produce the `(trigger_dest, trigger_source)` tuple for a
    /// child's L1 trigger / L2 `callReturn` entry. **Closes
    /// invariant #19** — any future code that wants to rebuild
    /// this pair must either call this function or rename the
    /// enum's variants (which is visible in review).
    pub fn address_pair_for(
        &self,
        call_destination: Address,
        call_source_address: Address,
    ) -> (Address, Address) {
        match self {
            // Return call: preserve — proxy represents the L2
            // destination, source is the L1 caller.
            Self::Return => (call_destination, call_source_address),
            // Forward call: swap — proxy represents the L2 source,
            // destination is the L1 contract being called.
            Self::Forward => (call_source_address, call_destination),
        }
    }

    /// `true` iff this is a return call (`L1→L2 child`).
    pub fn is_return(&self) -> bool {
        matches!(self, Self::Return)
    }

    /// `true` iff this is a forward call (`L2→L1 child`).
    pub fn is_forward(&self) -> bool {
        matches!(self, Self::Forward)
    }
}

// ──────────────────────────────────────────────
//  ReturnData (refactor PLAN step 1.10)
//
//  Tagged union over the "zero bytes = void function" convention
//  that the codebase uses in 23+ places. Every cross-chain call
//  result (delivery simulation, L2 eth_call, L1 trigger trace) either
//  returns actual ABI-encoded data or zero bytes to indicate a void
//  function. Pre-1.10, this was represented as a bare `Vec<u8>` and
//  tested with `.is_empty()` at every read site:
//
//      if child.delivery_return_data.is_empty() && !child.delivery_failed {
//          // void path
//      }
//
//  Reading `.is_empty()` on a `Vec<u8>` requires the reviewer to
//  remember that "empty means void" is a project convention, not
//  a natural invariant. Post-1.10 the same check becomes:
//
//      if ReturnData::from_bytes(child.delivery_return_data.clone()).is_void()
//          && !child.delivery_failed
//      { ... }
//
//  Or, when a struct field migrates to `ReturnData` directly:
//
//      if child.delivery_return_data.is_void() && !child.delivery_failed { ... }
//
//  `is_void()` is a named method whose docstring says exactly what
//  the convention means, which makes the invariant self-documenting.
//
//  ## Closes invariant #20 (partial)
//
//  Invariant #20: "Return data shape — Void = 0 bytes;
//  `delivery_return_data` → hashes; `l2_return_data` → scope
//  resolution". The invariant lives in four RESULT hash sites, each
//  of which must choose between `result_void()` (void) and an
//  explicit RESULT with `data: return_bytes` (non-void) based on the
//  return data's emptiness. `ReturnData::is_void` is the typed gate.
//
//  ## Scope decision (see commit message)
//
//  The PLAN calls for `ReturnData` to be propagated through every
//  struct field that currently holds `Vec<u8>` return data
//  (`DetectedCall`, `L1DetectedCall`, `L2DetectedCall`, `L2ReturnCall`,
//  RPC JSON types). Auditing found ~101 access sites across those
//  types, most in cascading wire-type chains that serialize to JSON
//  and would require boundary conversions at every entry and exit.
//
//  This step ships `ReturnData` as a **standalone helper type** with
//  `from_bytes` / `from_slice` / `is_void` / `as_bytes` /
//  `into_bytes`, usable by any site that wants the typed
//  `is_void()` check, without forcing the wire-type cascade. Field
//  migration is deferred to a future step that has a concrete
//  caller needing the full type-level propagation. This matches the
//  partial-migration discipline used in 1.2 (state roots), 1.4
//  (EntryClass), 1.7 (proposer API typestate), 1.8 (L1NonceReservation/
//  L1Client), 1.9a (ImmediateEntryBuilder), and 1.9b/1.9c (deferred
//  builder wrappers). The load-bearing part — a named `is_void()`
//  method — exists now.
// ──────────────────────────────────────────────

/// Tagged return data from a cross-chain call's L1 delivery
/// simulation or L2 execution. See the module comment for the
/// "empty bytes = void function" convention this type encodes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReturnData {
    /// Zero bytes — the underlying function had no return value
    /// (void function, reached via assembly `return(0, 0)`).
    #[default]
    Void,
    /// Non-empty ABI-encoded return bytes.
    NonVoid(Vec<u8>),
}

impl ReturnData {
    /// Construct from a `Vec<u8>` at a boundary — ABI decode, eth_call
    /// response, trace output, RPC JSON deserialization. Empty input
    /// becomes [`ReturnData::Void`], non-empty becomes
    /// [`ReturnData::NonVoid`].
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        if bytes.is_empty() {
            Self::Void
        } else {
            Self::NonVoid(bytes)
        }
    }

    /// Construct from a slice. See [`ReturnData::from_bytes`].
    pub fn from_slice(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            Self::Void
        } else {
            Self::NonVoid(bytes.to_vec())
        }
    }

    /// `true` iff this represents a void return. **Closes
    /// invariant #20** at the read site — every future check that
    /// used to write `.is_empty()` on a `Vec<u8>` return data can
    /// now call `.is_void()` and get a named method whose
    /// docstring documents the convention.
    pub fn is_void(&self) -> bool {
        matches!(self, Self::Void)
    }

    /// `true` iff this carries actual return bytes.
    pub fn is_non_void(&self) -> bool {
        matches!(self, Self::NonVoid(_))
    }

    /// Borrow the underlying bytes. Returns an empty slice for
    /// [`ReturnData::Void`] — matches the pre-1.10 `Vec<u8>::as_slice()`
    /// behavior for zero-byte data.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Void => &[],
            Self::NonVoid(bytes) => bytes,
        }
    }

    /// Consume the `ReturnData` and return the underlying `Vec<u8>`.
    /// Used at wire / ABI encode boundaries that still take a raw
    /// byte buffer.
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Void => Vec::new(),
            Self::NonVoid(bytes) => bytes,
        }
    }

    /// Length of the underlying bytes. `0` for `Void`.
    pub fn len(&self) -> usize {
        match self {
            Self::Void => 0,
            Self::NonVoid(bytes) => bytes.len(),
        }
    }

    /// `true` iff length is zero — alias for `is_void` for API
    /// parity with `Vec<u8>::is_empty`.
    pub fn is_empty(&self) -> bool {
        self.is_void()
    }
}

/// Action types in the cross-chain execution protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CrossChainActionType {
    Call,
    Result,
    L2Tx,
    Revert,
    RevertContinue,
}

/// A cross-chain action (Rust mirror of Solidity `Action` struct).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossChainAction {
    pub action_type: CrossChainActionType,
    pub rollup_id: RollupId,
    pub destination: Address,
    pub value: U256,
    pub data: Vec<u8>,
    pub failed: bool,
    pub source_address: Address,
    pub source_rollup: RollupId,
    pub scope: ScopePath,
}

/// A state delta for a rollup (Rust mirror of Solidity `StateDelta` struct).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossChainStateDelta {
    pub rollup_id: RollupId,
    pub current_state: B256,
    pub new_state: B256,
    pub ether_delta: I256,
}

/// An execution table entry (Rust mirror of Solidity `ExecutionEntry` struct).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossChainExecutionEntry {
    pub state_deltas: Vec<CrossChainStateDelta>,
    pub action_hash: ActionHash,
    pub next_action: CrossChainAction,
}

// ──────────────────────────────────────────────
//  Rust → Solidity ABI conversion helpers
// ──────────────────────────────────────────────

impl CrossChainActionType {
    /// Convert to the Solidity ABI enum representation.
    fn to_sol(self) -> ICrossChainManagerL2::ActionType {
        match self {
            Self::Call => ICrossChainManagerL2::ActionType::CALL,
            Self::Result => ICrossChainManagerL2::ActionType::RESULT,
            Self::L2Tx => ICrossChainManagerL2::ActionType::L2TX,
            Self::Revert => ICrossChainManagerL2::ActionType::REVERT,
            Self::RevertContinue => ICrossChainManagerL2::ActionType::REVERT_CONTINUE,
        }
    }
}

impl CrossChainAction {
    /// Convert to the Solidity ABI struct representation.
    pub fn to_sol_action(&self) -> ICrossChainManagerL2::Action {
        ICrossChainManagerL2::Action {
            actionType: self.action_type.to_sol(),
            rollupId: self.rollup_id.as_u256(),
            destination: self.destination,
            value: self.value,
            data: self.data.clone().into(),
            failed: self.failed,
            sourceAddress: self.source_address,
            sourceRollup: self.source_rollup.as_u256(),
            scope: self.scope.as_slice().to_vec(),
        }
    }
}

// ──────────────────────────────────────────────
//  REVERT / REVERT_CONTINUE helpers (§D.12)
// ──────────────────────────────────────────────

/// Build a canonical REVERT action.
///
/// Signals scope revert on L1. The `scope` determines which `newScope` level
/// catches the `ScopeReverted` error. Fields match spec §D.12 and
/// `IntegrationTest.t.sol:Scenario 5`.
pub fn revert_action(rollup_id: RollupId, scope: ScopePath) -> CrossChainAction {
    CrossChainAction {
        action_type: CrossChainActionType::Revert,
        rollup_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: false,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
        scope,
    }
}

/// Build a canonical REVERT_CONTINUE action.
///
/// Looked up via `_getRevertContinuation(rollupId)` on L1.
/// The hash of this action is deterministic for a given `rollup_id`.
/// Fields: `failed=true`, everything else zero/empty (spec §D.12).
pub fn revert_continue_action(rollup_id: RollupId) -> CrossChainAction {
    CrossChainAction {
        action_type: CrossChainActionType::RevertContinue,
        rollup_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![],
        failed: true,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
        scope: ScopePath::root(),
    }
}

/// Compute the deterministic action hash for REVERT_CONTINUE.
///
/// `keccak256(abi.encode(Action{REVERT_CONTINUE, rollupId, 0, 0, "", true, 0, 0, []}))`
pub fn compute_revert_continue_hash(rollup_id: RollupId) -> ActionHash {
    let action = revert_continue_action(rollup_id);
    ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
        &action.to_sol_action(),
    )))
}

impl CrossChainStateDelta {
    /// Convert to the Solidity ABI struct representation.
    fn to_sol(&self) -> ICrossChainManagerL2::StateDelta {
        ICrossChainManagerL2::StateDelta {
            rollupId: self.rollup_id.as_u256(),
            currentState: self.current_state,
            newState: self.new_state,
            etherDelta: self.ether_delta,
        }
    }
}

impl CrossChainExecutionEntry {
    /// Convert to the Solidity ABI struct representation.
    fn to_sol(&self) -> ICrossChainManagerL2::ExecutionEntry {
        ICrossChainManagerL2::ExecutionEntry {
            stateDeltas: self.state_deltas.iter().map(|d| d.to_sol()).collect(),
            actionHash: self.action_hash.as_b256(),
            nextAction: self.next_action.to_sol_action(),
        }
    }
}

/// Count-based dedup filter for iterative cross-chain call discovery.
///
/// During iterative `debug_traceCallMany` expansion, each iteration re-detects
/// ALL calls (not just new ones). A naive set-based filter (`!existing.contains(new)`)
/// incorrectly removes legitimate duplicate calls — e.g., when `CallTwice` calls
/// `increment()` twice with identical `(destination, calldata)`.
///
/// This function uses count-based comparison: for each item in `new_items`, it tries
/// to match against an unused item in `existing`. If a match is found, the existing
/// item is "consumed" (marked used) and the new item is dropped. If no unused match
/// exists, the new item is genuinely new and kept in the result.
///
/// The `eq` closure defines what constitutes a "match" between two items.
pub fn filter_new_by_count<T>(
    new_items: Vec<T>,
    existing: &[T],
    eq: impl Fn(&T, &T) -> bool,
) -> Vec<T> {
    let mut used = vec![false; existing.len()];
    let mut result = Vec::new();
    for item in new_items {
        let matched = existing
            .iter()
            .enumerate()
            .position(|(i, ex)| !used[i] && eq(&item, ex));
        if let Some(idx) = matched {
            used[idx] = true;
        } else {
            result.push(item);
        }
    }
    result
}

/// Try to decode `loadExecutionTable(entries)` calldata into Rust entry types.
/// Returns `None` if the selector doesn't match or decoding fails.
pub fn try_decode_load_execution_table(calldata: &[u8]) -> Option<Vec<CrossChainExecutionEntry>> {
    use alloy_sol_types::SolCall;
    let decoded = ICrossChainManagerL2::loadExecutionTableCall::abi_decode(calldata).ok()?;
    let entries: Vec<CrossChainExecutionEntry> = decoded
        .entries
        .iter()
        .filter_map(|e| CrossChainExecutionEntry::from_sol(e).ok())
        .collect();
    Some(entries)
}

/// Encode the calldata for `CrossChainManagerL2.loadExecutionTable(entries)`.
pub fn encode_load_execution_table_calldata(entries: &[CrossChainExecutionEntry]) -> Bytes {
    let sol_entries: Vec<ICrossChainManagerL2::ExecutionEntry> =
        entries.iter().map(|e| e.to_sol()).collect();
    let call = ICrossChainManagerL2::loadExecutionTableCall {
        entries: sol_entries,
    };
    Bytes::from(call.abi_encode())
}

/// Encode the calldata for `Rollups.postBatch(entries, blobCount, callData, proof)`.
pub fn encode_post_batch_calldata(
    entries: &[CrossChainExecutionEntry],
    call_data: Bytes,
    proof: Bytes,
) -> Bytes {
    let sol_entries: Vec<ICrossChainManagerL2::ExecutionEntry> =
        entries.iter().map(|e| e.to_sol()).collect();
    let call = ICrossChainManagerL2::postBatchCall {
        entries: sol_entries,
        blobCount: U256::ZERO,
        callData: call_data,
        proof,
    };
    Bytes::from(call.abi_encode())
}

/// Compute entry hashes for a set of execution entries, mirroring Rollups.sol's
/// `postBatch` computation. Each entry hash includes the entry's state deltas,
/// the verification keys for each delta's rollup, the action hash, and the
/// encoded next action.
///
/// The `verification_key` parameter is the VK for this rollup (all deltas in
/// our entries reference the same rollup).
pub fn compute_entry_hashes(
    entries: &[CrossChainExecutionEntry],
    verification_key: B256,
) -> Vec<B256> {
    entries
        .iter()
        .map(|entry| {
            let sol_entry = entry.to_sol();

            // abi.encode(entries[i].stateDeltas) — dynamic array of structs
            let encoded_deltas = alloy_sol_types::SolValue::abi_encode(&sol_entry.stateDeltas);

            // Verification keys: one per state delta, all the same for our rollup
            let vks: Vec<B256> = sol_entry
                .stateDeltas
                .iter()
                .map(|_| verification_key)
                .collect();
            // abi.encode(vks) — encode as bytes32[] dynamic array
            let encoded_vks = alloy_sol_types::SolValue::abi_encode(&vks);

            // abi.encode(entries[i].nextAction)
            let encoded_action = ICrossChainManagerL2::Action::abi_encode(&sol_entry.nextAction);

            // keccak256(abi.encodePacked(
            //     abi.encode(stateDeltas),
            //     abi.encode(vks),
            //     actionHash,
            //     abi.encode(nextAction)
            // ))
            let mut packed = Vec::new();
            packed.extend_from_slice(&encoded_deltas);
            packed.extend_from_slice(&encoded_vks);
            packed.extend_from_slice(sol_entry.actionHash.as_slice());
            packed.extend_from_slice(&encoded_action);

            keccak256(&packed)
        })
        .collect()
}

/// Compute the `publicInputsHash` that Rollups.sol computes inside `postBatch`.
///
/// This mirrors the Solidity computation:
/// ```solidity
/// keccak256(abi.encodePacked(
///     blockhash(block.number - 1),
///     block.timestamp,
///     abi.encode(entryHashes),
///     abi.encode(blobHashes),
///     keccak256(callData)
/// ))
/// ```
///
/// `parent_block_hash` is `blockhash(block.number - 1)` and `block_timestamp` is
/// `block.timestamp` at the time `postBatch` executes on L1.
pub fn compute_public_inputs_hash(
    entry_hashes: &[B256],
    call_data: &Bytes,
    parent_block_hash: B256,
    block_timestamp: u64,
) -> B256 {
    // abi.encode(entryHashes) — encode as bytes32[] dynamic array
    let encoded_entry_hashes = alloy_sol_types::SolValue::abi_encode(&entry_hashes.to_vec());

    // abi.encode(blobHashes) — always empty (blobCount = 0)
    let empty_blob_hashes: Vec<B256> = vec![];
    let encoded_blob_hashes = alloy_sol_types::SolValue::abi_encode(&empty_blob_hashes);

    // keccak256(callData)
    let call_data_hash = keccak256(call_data.as_ref());

    // abi.encodePacked(blockhash, block.timestamp, abi.encode(entryHashes), abi.encode(blobHashes), keccak256(callData))
    let mut packed = Vec::new();
    packed.extend_from_slice(parent_block_hash.as_slice()); // bytes32
    packed.extend_from_slice(&B256::from(U256::from(block_timestamp)).0); // uint256
    packed.extend_from_slice(&encoded_entry_hashes);
    packed.extend_from_slice(&encoded_blob_hashes);
    packed.extend_from_slice(call_data_hash.as_slice()); // bytes32

    keccak256(&packed)
}

/// Encode L2 block data as `callData` for `postBatch()`.
/// Format: `abi.encode(uint256[] l2BlockNumbers, bytes[] transactions)` — flat parameter
/// encoding without tuple wrapper.
pub fn encode_block_calldata(block_numbers: &[u64], transactions: &[Bytes]) -> Bytes {
    use alloy_sol_types::SolType;
    let numbers: Vec<U256> = block_numbers.iter().map(|&n| U256::from(n)).collect();
    let encoded = <(
        alloy_sol_types::sol_data::Array<alloy_sol_types::sol_data::Uint<256>>,
        alloy_sol_types::sol_data::Array<alloy_sol_types::sol_data::Bytes>,
    )>::abi_encode_sequence(&(numbers, transactions.to_vec()));
    Bytes::from(encoded)
}

/// Decode L2 block data from `callData` in a `postBatch()` transaction.
/// Returns (l2_block_numbers, transactions) pairs.
pub fn decode_block_calldata(data: &Bytes) -> Result<(Vec<u64>, Vec<Bytes>), String> {
    use alloy_sol_types::SolType;
    type BlockCalldata = (
        alloy_sol_types::sol_data::Array<alloy_sol_types::sol_data::Uint<256>>,
        alloy_sol_types::sol_data::Array<alloy_sol_types::sol_data::Bytes>,
    );
    let decoded = BlockCalldata::abi_decode_sequence(data)
        .map_err(|e| format!("failed to decode block calldata: {e}"))?;
    let numbers: Vec<u64> = decoded.0.iter().map(|n| n.to::<u64>()).collect();
    Ok((numbers, decoded.1))
}

/// Build immediate execution entries for block submission.
/// Each block gets one entry with a StateDelta tracking state root transitions.
pub fn build_block_entries(
    blocks: &[(u64, B256, B256, Bytes)], // (l2_block_number, pre_state_root, post_state_root, transactions)
    rollup_id: u64,
) -> Vec<CrossChainExecutionEntry> {
    blocks
        .iter()
        .map(
            |(_l2_block_number, pre_state_root, post_state_root, _transactions)| {
                CrossChainExecutionEntry {
                    state_deltas: vec![CrossChainStateDelta {
                        rollup_id: RollupId::new(U256::from(rollup_id)),
                        current_state: *pre_state_root,
                        new_state: *post_state_root,
                        ether_delta: I256::ZERO,
                    }],
                    action_hash: ActionHash::ZERO, // immediate — applied during postBatch()
                    next_action: CrossChainAction {
                        action_type: CrossChainActionType::L2Tx,
                        rollup_id: RollupId::MAINNET,
                        destination: Address::ZERO,
                        value: U256::ZERO,
                        data: vec![],
                        failed: false,
                        source_address: Address::ZERO,
                        source_rollup: RollupId::MAINNET,
                        scope: ScopePath::root(),
                    },
                }
            },
        )
        .collect()
}

/// Build a single aggregate immediate entry for a batch of blocks.
/// Spans `currentState = pre_state_root` → `newState = post_state_root`,
/// covering the entire batch in one entry. Saves gas by not creating
/// per-block entries (empty blocks add zero overhead).
pub fn build_aggregate_block_entry(
    pre_state_root: B256,
    post_state_root: B256,
    rollup_id: u64,
) -> CrossChainExecutionEntry {
    CrossChainExecutionEntry {
        state_deltas: vec![CrossChainStateDelta {
            rollup_id: RollupId::new(U256::from(rollup_id)),
            current_state: pre_state_root,
            new_state: post_state_root,
            ether_delta: I256::ZERO,
        }],
        action_hash: ActionHash::ZERO, // immediate — applied during postBatch()
        next_action: CrossChainAction {
            action_type: CrossChainActionType::L2Tx,
            rollup_id: RollupId::MAINNET,
            destination: Address::ZERO,
            value: U256::ZERO,
            data: vec![],
            failed: false,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: ScopePath::root(),
        },
    }
}

/// Decode a `postBatch(entries, blobCount, callData, proof)` call from raw calldata.
/// Returns (entries, callData) or an error.
pub fn decode_post_batch_calldata(
    data: &Bytes,
) -> Result<(Vec<CrossChainExecutionEntry>, Bytes), String> {
    use alloy_sol_types::SolCall;
    let decoded = ICrossChainManagerL2::postBatchCall::abi_decode(data)
        .map_err(|e| format!("failed to decode postBatch calldata: {e}"))?;
    let entries: Vec<CrossChainExecutionEntry> = decoded
        .entries
        .iter()
        .map(CrossChainExecutionEntry::from_sol)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((entries, decoded.callData))
}

/// Encode the calldata for `CrossChainManagerL2.executeIncomingCrossChainCall(...)`.
pub fn encode_execute_incoming_call_calldata(action: &CrossChainAction) -> Bytes {
    let call = ICrossChainManagerL2::executeIncomingCrossChainCallCall {
        destination: action.destination,
        value: action.value,
        data: action.data.clone().into(),
        sourceAddress: action.source_address,
        sourceRollup: action.source_rollup.as_u256(),
        scope: action.scope.as_slice().to_vec(),
    };
    Bytes::from(call.abi_encode())
}

// ──────────────────────────────────────────────
//  L1 event parsing (Rollups.sol → Rust types)
// ──────────────────────────────────────────────

// ──────────────────────────────────────────────
//  Solidity → Rust conversion helpers
// ──────────────────────────────────────────────

impl CrossChainActionType {
    /// Convert from the Solidity ABI enum representation.
    ///
    /// Returns an error for unknown enum variants rather than silently
    /// defaulting, since an unknown action type from L1 likely indicates
    /// a contract upgrade or data corruption.
    fn from_sol(sol_type: ICrossChainManagerL2::ActionType) -> Result<Self, String> {
        match sol_type {
            ICrossChainManagerL2::ActionType::CALL => Ok(Self::Call),
            ICrossChainManagerL2::ActionType::RESULT => Ok(Self::Result),
            ICrossChainManagerL2::ActionType::L2TX => Ok(Self::L2Tx),
            ICrossChainManagerL2::ActionType::REVERT => Ok(Self::Revert),
            ICrossChainManagerL2::ActionType::REVERT_CONTINUE => Ok(Self::RevertContinue),
            other => Err(format!("unknown ActionType variant: {other:?}")),
        }
    }
}

impl CrossChainAction {
    /// Convert from the Solidity ABI struct representation.
    fn from_sol(sol: &ICrossChainManagerL2::Action) -> Result<Self, String> {
        Ok(Self {
            action_type: CrossChainActionType::from_sol(sol.actionType)?,
            rollup_id: RollupId::from_abi_boundary(sol.rollupId),
            destination: sol.destination,
            value: sol.value,
            data: sol.data.to_vec(),
            failed: sol.failed,
            source_address: sol.sourceAddress,
            source_rollup: RollupId::from_abi_boundary(sol.sourceRollup),
            scope: ScopePath::from_parts(sol.scope.clone()),
        })
    }
}

impl CrossChainStateDelta {
    /// Convert from the Solidity ABI struct representation.
    fn from_sol(sol: &ICrossChainManagerL2::StateDelta) -> Self {
        Self {
            rollup_id: RollupId::from_abi_boundary(sol.rollupId),
            current_state: sol.currentState,
            new_state: sol.newState,
            ether_delta: sol.etherDelta,
        }
    }
}

impl CrossChainExecutionEntry {
    /// Convert from the Solidity ABI struct representation.
    pub fn from_sol(sol: &ICrossChainManagerL2::ExecutionEntry) -> Result<Self, String> {
        Ok(Self {
            state_deltas: sol
                .stateDeltas
                .iter()
                .map(CrossChainStateDelta::from_sol)
                .collect(),
            action_hash: ActionHash::from_abi_boundary(sol.actionHash),
            next_action: CrossChainAction::from_sol(&sol.nextAction)?,
        })
    }
}

/// Build a single execution entry for a non-nested cross-chain call.
///
/// This is the shared logic used by both `rpc::initiate_cross_chain_call` and
/// `l1_proxy` when detecting cross-chain calls in L1 traces.
///
/// Returns a pair of entries for **L2 execution**:
/// - `call_entry`: actionHash = hash(CALL), nextAction = CALL action
///   (triggers `executeIncomingCrossChainCall` on L2)
/// - `result_entry`: actionHash = hash(RESULT), nextAction = RESULT action
///   (loaded into the L2 execution table)
///
/// For **L1 submission**, use [`convert_pairs_to_l1_entries`] to transform
/// pairs into the non-nested format (actionHash=CALL, nextAction=RESULT).
#[allow(clippy::too_many_arguments)]
pub fn build_cross_chain_call_entries(
    rollup_id: RollupId,
    destination: Address,
    data: Vec<u8>,
    value: U256,
    source_address: Address,
    source_rollup: RollupId,
    call_success: bool,
    return_data: Vec<u8>,
) -> (CrossChainExecutionEntry, CrossChainExecutionEntry) {
    // Build CALL action: triggers executeIncomingCrossChainCall on L2,
    // and its hash is used by L1 for entry lookup.
    // The `value` field MUST match `msg.value` in the L1 proxy call — otherwise
    // the action hash won't match what Rollups.sol computes during
    // executeCrossChainCall and the entry will revert with ExecutionNotFound.
    let call_action = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id,
        destination,
        value,
        data,
        failed: false,
        source_address,
        source_rollup,
        scope: ScopePath::root(),
    };

    // Build RESULT action: loaded into L2 execution table, consumed when
    // the cross-chain call completes. Return data is simulated so the
    // action hash matches what _processCallAtScope() computes.
    let result_action = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: return_data,
        failed: !call_success,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
        scope: ScopePath::root(),
    };

    let call_action_hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
        &call_action.to_sol_action(),
    )));
    let result_action_hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
        &result_action.to_sol_action(),
    )));

    let call_entry = CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: call_action_hash,
        next_action: call_action,
    };
    let result_entry = CrossChainExecutionEntry {
        state_deltas: vec![],
        action_hash: result_action_hash,
        next_action: result_action,
    };

    (call_entry, result_entry)
}

/// Withdrawal entries for L2→L1 ETH withdrawals.
///
/// Contains both L2 table entries (loaded via loadExecutionTable, consumed by the
/// user's Bridge.bridgeEther tx on L2) and L1 deferred entries (posted via postBatch,
/// consumed by the builder's trigger tx on L1).
#[derive(Debug, Clone)]
pub struct WithdrawalEntries {
    /// L2 table entries: CALL+RESULT pair loaded into CCM execution table.
    /// The CALL is consumed when Bridge.bridgeEther(0) executes on L2.
    pub l2_table_entries: Vec<CrossChainExecutionEntry>,
    /// L1 deferred entries: nested CALL+RESULT pair posted via postBatch.
    /// Consumed when the builder's trigger tx calls the L1 proxy.
    pub l1_deferred_entries: Vec<CrossChainExecutionEntry>,
    /// User address (initiator of the withdrawal on L2).
    pub user: Address,
    /// Withdrawal amount in wei.
    pub amount: U256,
}

/// Build L2→L1 call entries for a general cross-chain call.
///
/// Produces both L2 table entries (loaded via loadExecutionTable, consumed by
/// the L2 proxy call) and L1 deferred entries (posted via postBatch, consumed
/// by the builder's trigger tx calling proxy(source_address, rollup_id) on L1).
///
/// Parameters:
/// - `destination`: L1 target address (originalAddress from the L2 proxy)
/// - `data`: calldata for the L1 execution (empty for ETH withdrawals)
/// - `value`: ETH value to deliver on L1
/// - `source_address`: the L2 initiator — msg.sender in the L2 proxy fallback.
///   Also used as the L1 proxy owner (proxy(source_address, rollup_id)) and as the
///   delivery source identity on L1.
/// - `rollup_id`: our rollup's ID
/// - `rlp_encoded_tx`: RLP-encoded L2 transaction for the L2TX trigger on L1
/// - `delivery_return_data`: return data from L1 simulation (empty for EOA/withdrawals)
/// - `delivery_failed`: whether the L1 simulation reverted (false for withdrawals)
#[allow(clippy::too_many_arguments)]
pub fn build_l2_to_l1_call_entries(
    destination: Address,
    data: Vec<u8>,
    value: U256,
    source_address: Address,
    rollup_id: u64,
    rlp_encoded_tx: Vec<u8>,
    delivery_return_data: Vec<u8>,
    delivery_failed: bool,
    l1_delivery_scope: Vec<U256>,
    tx_outcome: TxOutcome,
) -> WithdrawalEntries {
    // Construct the `RollupId` once from the u64 config value — this is
    // the point where a config-level u64 crosses into the domain's
    // type-safe representation.
    let rollup_id_typed = RollupId::new(U256::from(rollup_id));

    // ── L2 table entries ──
    // These match what CCM.executeCrossChainCall will build when
    // proxy(destination, 0) is called on L2.
    //
    // CCM action fields (from CrossChainManagerL2.sol:98-124):
    //   action.actionType = CALL
    //   action.rollupId = proxyInfo.originalRollupId = 0 (proxy for L1)
    //   action.destination = proxyInfo.originalAddress = destination
    //   action.value = msg.value = value
    //   action.data = callData = data
    //   action.sourceAddress = msg.sender in proxy fallback = source_address
    //   action.sourceRollup = ROLLUP_ID = rollup_id
    //   action.scope = [] (empty)
    let l2_call_action = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: RollupId::MAINNET, // target = L1 (rollup 0)
        destination,
        value,
        data: data.clone(),
        failed: false,
        source_address,
        source_rollup: rollup_id_typed,
        scope: ScopePath::root(),
    };
    let l2_result_action = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: RollupId::MAINNET,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: delivery_return_data.clone(),
        failed: delivery_failed,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
        scope: ScopePath::root(),
    };

    let l2_call_hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
        &l2_call_action.to_sol_action(),
    )));
    let l2_result_hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
        &l2_result_action.to_sol_action(),
    )));

    let l2_table_entries = vec![
        CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: l2_call_hash,
            // nextAction = RESULT: after the L2 side completes, _resolveScopes
            // sees RESULT → returns immediately.
            next_action: l2_result_action.clone(),
        },
        CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: l2_result_hash,
            next_action: l2_result_action,
        },
    ];

    // ── L1 deferred entries (nested format) ──
    // Entry 0: L2TX trigger — matches what Rollups.executeL2TX builds
    // (Rollups.sol:307-331).
    //
    // Rollups.sol action fields:
    //   action.actionType = L2TX
    //   action.rollupId = rollupId (our L2 rollup ID)
    //   action.destination = address(0)
    //   action.value = 0
    //   action.data = rlpEncodedTx (the RLP-encoded L2 transaction)
    //   action.sourceAddress = address(0)
    //   action.sourceRollup = MAINNET_ROLLUP_ID = 0
    //   action.scope = [] (empty)
    let l1_trigger_action = CrossChainAction {
        action_type: CrossChainActionType::L2Tx,
        rollup_id: rollup_id_typed,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: rlp_encoded_tx,
        failed: false,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET, // MAINNET_ROLLUP_ID = 0
        scope: ScopePath::root(),
    };
    let l1_trigger_hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
        &l1_trigger_action.to_sol_action(),
    )));

    tracing::debug!(
        target: "based_rollup::cross_chain",
        %l1_trigger_hash,
        rlp_len = l1_trigger_action.data.len(),
        rlp_prefix = %format!("0x{}", hex::encode(&l1_trigger_action.data[..std::cmp::min(20, l1_trigger_action.data.len())])),
        rollup_id = %l1_trigger_action.rollup_id,
        "L2TX entry action_hash (from build_l2_to_l1_call_entries)"
    );

    // nextAction for L2TX trigger = delivery CALL (executes on L1 via _resolveScopes)
    // Scope determines how deeply _resolveScopes nests newScope() calls before
    // executing at _processCallAtScope. Depth = number of user contract boundaries
    // between the L2 tx entry and the proxy call in the L2 trace.
    // Example: SCA→SCB→proxy = scope=[0,0] (2 levels of wrapping).
    //
    // For REVERT entries (tx_outcome=Revert): the delivery CALL scope must be at
    // least [0] so that _resolveScopes enters newScope([0]), giving REVERT a
    // scope frame to match against. With scope=[], the CALL executes at root
    // scope via _processCallAtScope directly — no newScope frame to catch
    // ScopeReverted (per revertCounterL2 protocol E2E test).
    let effective_scope = if tx_outcome.is_revert() && l1_delivery_scope.is_empty() {
        vec![U256::ZERO] // minimum scope=[0] for REVERT matching
    } else {
        l1_delivery_scope.clone()
    };
    let l1_delivery_action = CrossChainAction {
        action_type: CrossChainActionType::Call,
        rollup_id: RollupId::MAINNET, // L1 scope
        destination,
        value,
        data,
        failed: false,
        source_address, // L2 initiator is the source on L1
        source_rollup: rollup_id_typed,
        scope: ScopePath::from_parts(effective_scope.clone()),
    };

    // Entry 2 action_hash: matches what _processCallAtScope builds after executing
    // the delivery call on L1: RESULT(rollupId=CALL.rollupId=0, data=returnData).
    // For withdrawals (EOA): returnData empty. For contracts: from L1 simulation.
    let delivery_return_data_len = delivery_return_data.len();
    let delivery_return_data_hex_snap = format!(
        "0x{}",
        hex::encode(&delivery_return_data[..delivery_return_data.len().min(40)])
    );
    let l1_delivery_result = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: RollupId::MAINNET,
        destination: Address::ZERO,
        value: U256::ZERO,
        data: delivery_return_data,
        failed: delivery_failed,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
        scope: ScopePath::root(),
    };
    let l1_delivery_result_hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
        &l1_delivery_result.to_sol_action(),
    )));

    // Entry 2 nextAction: terminal RESULT for L2TX (per SYNC_ROLLUPS_PROTOCOL_SPEC §C.6).
    // Always void with rollupId = triggering rollupId (L2). This applies regardless of
    // whether the inner delivery returns data — the terminal is always empty.
    let l2tx_terminal = CrossChainAction {
        action_type: CrossChainActionType::Result,
        rollup_id: rollup_id_typed, // triggering rollupId (L2)
        destination: Address::ZERO,
        value: U256::ZERO,
        data: vec![], // always empty per §C.6
        failed: false,
        source_address: Address::ZERO,
        source_rollup: RollupId::MAINNET,
        scope: ScopePath::root(),
    };

    // Nested format: [trigger CALL entry, delivery RESULT entry]
    // The trigger CALL's nextAction is the delivery CALL (enters newScope).
    // The delivery RESULT's nextAction is a terminal RESULT (exits scope).
    // Ether accounting: the delivery CALL sends ETH via the proxy.
    // _processCallAtScope tracks _etherDelta -= value DURING execution.
    // _applyStateDeltas checks totalEtherDelta == _etherDelta at consumption time.
    //
    // Entry[0] (L2TX trigger → delivery CALL) is consumed BEFORE the CALL executes.
    //   At consumption: _etherDelta = 0 (no ETH sent yet) → ether_delta must be 0.
    //
    // Entry[1] (scope resolution RESULT) is consumed AFTER the CALL executes.
    //   At consumption: _etherDelta = -value (ETH was sent) → ether_delta must be -value.
    let delivery_ether_delta = if value.is_zero() {
        I256::ZERO
    } else {
        -I256::try_from(value).unwrap_or(I256::ZERO)
    };

    // When tx_outcome=Revert, Entry 1's nextAction becomes REVERT and we add
    // a REVERT_CONTINUE entry (Entry 2). The scope revert mechanism in
    // Rollups.sol undoes the delivery call's L1 state changes.
    //
    // Entry 2's ether_delta = 0 because _etherDelta is RESET to 0 after
    // each _applyStateDeltas call (Rollups.sol:517). No ETH flows between
    // Entry 1 and Entry 2 consumption.
    let l1_deferred_entries = if tx_outcome.is_revert() {
        // REVERT scope is ALWAYS [0] — the first child scope of _resolveScopes.
        // This is independent of the delivery scope (which can be [0], [0,0], etc.).
        // REVERT([0]) is caught by newScope([0]) → ScopeReverted bubbles to
        // newScope([]) → caught by _resolveScopes. This reverts ALL effects
        // within the scope, regardless of delivery depth.
        // Evidence: both revertCounterL2 (delivery=[0]) and deepScopeRevert
        // (delivery=[0,0]) use REVERT(scope=[0]).
        let revert_next = revert_action(rollup_id_typed, ScopePath::from_index(U256::ZERO));
        let revert_continue_hash = compute_revert_continue_hash(rollup_id_typed);
        vec![
            CrossChainExecutionEntry {
                state_deltas: vec![CrossChainStateDelta {
                    rollup_id: rollup_id_typed,
                    current_state: B256::ZERO,
                    new_state: B256::ZERO,
                    ether_delta: I256::ZERO, // consumed BEFORE ETH sent
                }],
                action_hash: l1_trigger_hash,
                next_action: l1_delivery_action,
            },
            CrossChainExecutionEntry {
                state_deltas: vec![CrossChainStateDelta {
                    rollup_id: rollup_id_typed,
                    current_state: B256::ZERO,
                    new_state: B256::ZERO,
                    ether_delta: delivery_ether_delta, // consumed AFTER ETH sent
                }],
                action_hash: l1_delivery_result_hash,
                next_action: revert_next, // REVERT instead of terminal RESULT
            },
            CrossChainExecutionEntry {
                state_deltas: vec![CrossChainStateDelta {
                    rollup_id: rollup_id_typed,
                    current_state: B256::ZERO,
                    new_state: B256::ZERO,
                    ether_delta: I256::ZERO, // _etherDelta reset after Entry 1
                }],
                action_hash: revert_continue_hash,
                next_action: l2tx_terminal, // terminal RESULT(failed=false)
            },
        ]
    } else {
        vec![
            CrossChainExecutionEntry {
                state_deltas: vec![CrossChainStateDelta {
                    rollup_id: rollup_id_typed,
                    current_state: B256::ZERO,
                    new_state: B256::ZERO,
                    ether_delta: I256::ZERO, // consumed BEFORE ETH sent
                }],
                action_hash: l1_trigger_hash,
                next_action: l1_delivery_action,
            },
            CrossChainExecutionEntry {
                state_deltas: vec![CrossChainStateDelta {
                    rollup_id: rollup_id_typed,
                    current_state: B256::ZERO,
                    new_state: B256::ZERO,
                    ether_delta: delivery_ether_delta, // consumed AFTER ETH sent
                }],
                action_hash: l1_delivery_result_hash,
                next_action: l2tx_terminal, // §C.6: L2TX terminal is always void
            },
        ]
    };

    if tx_outcome.is_revert() {
        tracing::info!(
            target: "based_rollup::cross_chain",
            l2_entries = l2_table_entries.len(),
            l1_entries = l1_deferred_entries.len(),
            %l2_call_hash,
            %l2_result_hash,
            delivery_return_data_len,
            delivery_failed,
            delivery_return_data_hex = %delivery_return_data_hex_snap,
            "build_l2_to_l1_call_entries: REVERT entries built"
        );
        for (i, entry) in l1_deferred_entries.iter().enumerate() {
            tracing::debug!(
                target: "based_rollup::cross_chain",
                idx = i,
                action_hash = %entry.action_hash,
                next_action_type = ?entry.next_action.action_type,
                next_action_rollup_id = %entry.next_action.rollup_id,
                next_action_failed = entry.next_action.failed,
                next_action_data_len = entry.next_action.data.len(),
                next_action_scope_len = entry.next_action.scope.len(),
                ether_delta = %entry.state_deltas.first().map(|d| d.ether_delta).unwrap_or_default(),
                "build_l2_to_l1_call_entries: REVERT L1 entry"
            );
        }
        for (i, entry) in l2_table_entries.iter().enumerate() {
            tracing::debug!(
                target: "based_rollup::cross_chain",
                idx = i,
                action_hash = %entry.action_hash,
                next_action_type = ?entry.next_action.action_type,
                next_action_failed = entry.next_action.failed,
                next_action_data_len = entry.next_action.data.len(),
                next_action_data_hex = %format!("0x{}", hex::encode(&entry.next_action.data[..entry.next_action.data.len().min(40)])),
                "build_l2_to_l1_call_entries: REVERT L2 entry"
            );
        }
    }

    WithdrawalEntries {
        l2_table_entries,
        l1_deferred_entries,
        user: source_address,
        amount: value,
    }
}

/// Convert L2-format entry pairs to L1-format entries for submission.
///
/// L2 uses pairs: `[CALL trigger, RESULT table entry]` per cross-chain call.
/// L1 uses a single non-nested entry: `actionHash=hash(CALL), nextAction=RESULT`.
/// This prevents Rollups.sol from entering `newScope()` for simple calls.
///
/// State deltas (if attached) are carried from the CALL entry (even index).
pub fn convert_pairs_to_l1_entries(
    entries: &[CrossChainExecutionEntry],
) -> Vec<CrossChainExecutionEntry> {
    entries
        .chunks(2)
        .filter_map(|pair| {
            if pair.len() == 2 {
                Some(CrossChainExecutionEntry {
                    state_deltas: pair[0].state_deltas.clone(),
                    action_hash: pair[0].action_hash,
                    next_action: pair[1].next_action.clone(),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Reconstruct L2-format entry pairs from L1-format entries and CALL actions.
///
/// During derivation, fullnodes receive L1-format entries (actionHash=hash(CALL),
/// nextAction=RESULT) from `BatchPosted` events, plus the original CALL actions
/// from `ExecutionConsumed` events. This function reconstructs the L2-format
/// pairs that `evm_config` expects: `[CALL trigger, RESULT table entry]`.
///
/// Matching is by action hash (not position) because the `l1_entries` may be
/// a filtered subset of the original entries (e.g., only consumed entries).
pub fn convert_l1_entries_to_l2_pairs(
    l1_entries: &[CrossChainExecutionEntry],
    call_actions: &[CrossChainAction],
) -> Vec<CrossChainExecutionEntry> {
    // Build a lookup: hash(CALL action) → Vec of CALL actions (occurrence-aware).
    // Multiple consumed events with the same actionHash are preserved so that
    // duplicate-call patterns (e.g., CallTwice) can be matched 1:1.
    let mut action_map: std::collections::HashMap<ActionHash, Vec<&CrossChainAction>> =
        std::collections::HashMap::new();
    for a in call_actions {
        let hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
            &a.to_sol_action(),
        )));
        action_map.entry(hash).or_default().push(a);
    }
    // Track which occurrence of each hash has been consumed so far.
    let mut consumed_idx: std::collections::HashMap<ActionHash, usize> = std::collections::HashMap::new();

    // Detect if this batch has continuation entries (multi-call patterns).
    // Continuation entries have nextAction.action_type == CALL.
    let has_continuations = l1_entries
        .iter()
        .any(|e| e.next_action.action_type == CrossChainActionType::Call);

    let mut result = Vec::with_capacity(l1_entries.len() * 2);
    for entry in l1_entries {
        // Skip continuation entries (nextAction is CALL, not RESULT).
        // These are handled by reconstruct_continuation_l2_entries.
        if entry.next_action.action_type == CrossChainActionType::Call {
            continue;
        }
        // Skip REVERT/REVERT_CONTINUE entries — these are L1-only scope revert
        // entries (§D.12 atomicity) with no L2 counterpart. The L2 tx reverts
        // atomically via EVM; the L2 entries (CALL→RESULT pair) are already
        // in the block body's loadExecutionTable calldata.
        if entry.next_action.action_type == CrossChainActionType::Revert
            || entry.next_action.action_type == CrossChainActionType::RevertContinue
        {
            continue;
        }
        let idx = consumed_idx.entry(entry.action_hash).or_insert(0);
        if let Some(call_action) = action_map
            .get(&entry.action_hash)
            .and_then(|actions| actions.get(*idx))
        {
            *idx += 1;
            // When continuations are present, skip entries whose consumed action
            // is NOT a CALL (e.g., RESULT resolution entries from scope navigation).
            // These are reconstructed by reconstruct_continuation_l2_entries.
            if has_continuations && call_action.action_type != CrossChainActionType::Call {
                continue;
            }
            // Reconstruct CALL trigger entry
            let call_entry = CrossChainExecutionEntry {
                state_deltas: entry.state_deltas.clone(),
                action_hash: entry.action_hash,
                next_action: (*call_action).clone(),
            };
            // Reconstruct RESULT table entry
            let result_action_hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
                &entry.next_action.to_sol_action(),
            )));
            let result_entry = CrossChainExecutionEntry {
                state_deltas: vec![],
                action_hash: result_action_hash,
                next_action: entry.next_action.clone(),
            };
            result.push(call_entry);
            // Skip RESULT table entry when continuations are present — the continuation
            // entries (reconstruct_continuation_l2_entries) provide their own RESULT entries.
            // Including this one would conflict: same actionHash but wrong nextAction.
            if !has_continuations {
                result.push(result_entry);
            }
        } else {
            // No matching CALL action — pass through as-is
            result.push(entry.clone());
        }
    }
    result
}

/// Reconstruct L2 continuation entries for multi-call patterns.
///
/// During multi-call continuations, an L1 entry may have `nextAction.action_type == CALL`
/// instead of the usual `RESULT` (simple deposit). This signals a reentrant cross-chain
/// call pattern where CALL_A triggers on L2, then a child CALL_C fires back to L1 (or
/// another rollup), and the result resolves back on L2.
///
/// For each such continuation L1 entry (actionHash=hash(CALL_B), nextAction=CALL_C with scope),
/// this function generates 3 additional L2 entries that the CCM execution table needs:
///
/// 1. `hash(RESULT(our_rollup, void)) → CALL_B` — consumed after the parent CALL returns on L2
/// 2. `hash(CALL_C_unscoped) → RESULT(target_rollup, data)` — consumed by the reentrant bridge call
/// 3. `hash(RESULT(our_rollup, void)) → RESULT(our_rollup, data)` — terminal entry
///
/// Entries 2 and 3 use real delivery return data from the L1 resolution entries when
/// available (paired by traversal order). Falls back to void for backward compatibility.
/// Entries 1 and 3 use void for the action_hash because L2 return data is not available
/// to the fullnode without L2 simulation.
///
/// These entries are appended AFTER the standard pairs reconstructed by
/// `convert_l1_entries_to_l2_pairs`, preserving the order expected by `loadExecutionTable`.
///
/// Returns an empty vec if no continuation patterns are found (simple deposits only).
pub fn reconstruct_continuation_l2_entries(
    l1_entries: &[CrossChainExecutionEntry],
    call_actions: &[CrossChainAction],
) -> Vec<CrossChainExecutionEntry> {
    use std::collections::HashMap;

    // Build lookup: hash(action) → action for all consumed actions.
    // This includes CALL triggers (consumed by executeCrossChainCall) and
    // RESULT actions (consumed by scope resolution via _consumeExecution).
    let action_map: HashMap<ActionHash, &CrossChainAction> = call_actions
        .iter()
        .map(|a| {
            let hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
                &a.to_sol_action(),
            )));
            (hash, a)
        })
        .collect();

    // Determine our_rollup_id from the L1 entries' state deltas.
    // The rollupId in state deltas identifies which rollup this batch belongs to.
    let our_rollup_id = l1_entries
        .iter()
        .flat_map(|e| &e.state_deltas)
        .map(|d| d.rollup_id)
        .find(|id| !id.is_mainnet())
        .unwrap_or(RollupId::new(U256::from(1)));

    // Build hash-based lookup for L1 entries: action_hash → Vec<entry>.
    // Uses Vec to preserve multiple entries with the same hash — the protocol
    // supports duplicate calls (e.g., CallTwice calling increment() twice
    // produces two entries with the same action_hash).
    let mut l1_entry_map: std::collections::HashMap<ActionHash, Vec<&CrossChainExecutionEntry>> =
        std::collections::HashMap::new();
    for entry in l1_entries.iter() {
        l1_entry_map
            .entry(entry.action_hash)
            .or_default()
            .push(entry);
    }
    // Track consumption index per hash for occurrence-aware matching.
    // Each lookup consumes the NEXT entry, not always the first (#256).
    let mut consumed_idx: std::collections::HashMap<ActionHash, usize> = std::collections::HashMap::new();

    let mut continuation_entries = Vec::new();

    // Skip reconstruction for REVERT batches — L2 entries are already in the
    // block body's loadExecutionTable calldata. REVERT entries (§D.12) are L1-only;
    // the L2 tx reverts atomically and CCM entries remain in the block body.
    let has_revert_entries = l1_entries
        .iter()
        .any(|e| e.next_action.action_type == CrossChainActionType::Revert);
    if has_revert_entries {
        tracing::info!(
            target: "based_rollup::cross_chain",
            "skipping continuation reconstruction for REVERT batch (L2 entries in block body)"
        );
        return continuation_entries;
    }

    for entry in l1_entries {
        // Continuation pattern: nextAction is a CALL (not RESULT)
        if entry.next_action.action_type != CrossChainActionType::Call {
            continue;
        }

        // This is a continuation L1 entry: actionHash=hash(CALL_B), nextAction=CALL_C(scoped)
        let call_c_scoped = &entry.next_action;

        // Find CALL_B from the call_actions map (it's the consumed action for this entry)
        let call_b = match action_map.get(&entry.action_hash) {
            Some(action) => (*action).clone(),
            None => {
                warn!(
                    target: "based_rollup::cross_chain",
                    action_hash = %entry.action_hash,
                    "continuation entry has no matching CALL action in consumed map — skipping"
                );
                continue;
            }
        };

        // ── Hash-based lookup for return data (#254 codex review) ──
        //
        // Instead of fragile positional pairing, compute the expected hashes
        // and look up the matching L1 entries directly:
        //
        // L1 entries structure:
        //   hash(trigger_CALL) → delivery_CALL(scope=[0])      ← this entry
        //   hash(CALL_C_unscoped) → RESULT(data=inner_return)  ← inner call result
        //   hash(scope_RESULT)  → RESULT(data=delivery_return) ← scope resolution

        // Build CALL_C_unscoped to compute its hash for lookup.
        let call_c_unscoped = CrossChainAction {
            action_type: call_c_scoped.action_type,
            rollup_id: call_c_scoped.rollup_id,
            destination: call_c_scoped.destination,
            value: call_c_scoped.value,
            data: call_c_scoped.data.clone(),
            failed: call_c_scoped.failed,
            source_address: call_c_scoped.source_address,
            source_rollup: call_c_scoped.source_rollup,
            scope: ScopePath::root(),
        };
        let call_c_unscoped_hash = ActionHash::new(keccak256(
            ICrossChainManagerL2::Action::abi_encode(&call_c_unscoped.to_sol_action()),
        ));

        // Look up inner call result: hash(CALL_C_unscoped) → RESULT(inner_data)
        // Occurrence-aware: consume the NEXT matching entry, not always the first (#256).
        let (inner_data, inner_failed) = {
            let idx = consumed_idx.entry(call_c_unscoped_hash).or_insert(0);
            l1_entry_map
                .get(&call_c_unscoped_hash)
                .and_then(|entries| {
                    let matching: Vec<_> = entries
                        .iter()
                        .filter(|e| e.next_action.action_type == CrossChainActionType::Result)
                        .collect();
                    matching.get(*idx).map(|e| {
                        *idx += 1;
                        (e.next_action.data.clone(), e.next_action.failed)
                    })
                })
                .unwrap_or_else(|| (vec![], false))
        };

        // Compute scope resolution hash to find delivery return data.
        // On L1, _processCallAtScope builds RESULT{data=inner_data} after
        // the inner call returns. The scope resolution entry consumes this hash.
        let scope_result_for_lookup = CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: call_c_scoped.rollup_id,
            destination: Address::ZERO,
            value: U256::ZERO,
            data: inner_data.clone(),
            failed: inner_failed,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: ScopePath::root(),
        };
        let scope_result_hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
            &scope_result_for_lookup.to_sol_action(),
        )));

        // Look up scope resolution: hash(scope_RESULT) → RESULT(delivery_data)
        // Occurrence-aware: consume the NEXT matching entry (#256).
        let (delivery_data, delivery_failed) = {
            let idx = consumed_idx.entry(scope_result_hash).or_insert(0);
            l1_entry_map
                .get(&scope_result_hash)
                .and_then(|entries| {
                    let matching: Vec<_> = entries
                        .iter()
                        .filter(|e| e.next_action.action_type == CrossChainActionType::Result)
                        .collect();
                    matching.get(*idx).map(|e| {
                        *idx += 1;
                        (e.next_action.data.clone(), e.next_action.failed)
                    })
                })
                .unwrap_or_else(|| (vec![], false))
        };

        // Entry 1: hash(RESULT(our_rollup, inner_data)) → CALL_B
        // This is consumed after the parent CALL_A returns on L2.
        //
        // On-chain, _processCallAtScope builds RESULT{data: executeOnBehalf_return}
        // after the parent call. The return data from the L2 parent call matches
        // the inner call's return data extracted from the L1 entries (inner_data).
        // Using the actual data (instead of void) ensures the hash matches what
        // the on-chain CCM computes for non-void continuation patterns.
        let result_our_rollup = CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: our_rollup_id,
            destination: Address::ZERO,
            value: U256::ZERO,
            data: inner_data.clone(),
            failed: inner_failed,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: ScopePath::root(),
        };
        let result_our_hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
            &result_our_rollup.to_sol_action(),
        )));
        continuation_entries.push(CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: result_our_hash,
            next_action: call_b,
        });

        // Entry 2: hash(CALL_C_unscoped) → RESULT(target_rollup, inner_data)
        // call_c_unscoped and call_c_unscoped_hash already computed above for lookup.
        // RESULT targeting the rollup that CALL_C was aimed at, with the
        // inner call's return data (what executeCrossChainCall returns to the
        // L1 caller). This is inner_data, NOT delivery_data — the distinction
        // matters when the outer function transforms the return.
        let result_target = CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: call_c_scoped.rollup_id,
            destination: Address::ZERO,
            value: U256::ZERO,
            data: inner_data,
            failed: inner_failed,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: ScopePath::root(),
        };
        continuation_entries.push(CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: call_c_unscoped_hash,
            next_action: result_target,
        });

        // Entry 3: hash(RESULT(our_rollup, inner_data)) → RESULT(our_rollup, delivery_data)
        // Terminal entry — same action_hash as Entry 1 (result_our_hash).
        //
        // The next_action carries real delivery return data from L1 so that
        // _resolveScopes returns it to the L2 caller.
        let result_terminal = CrossChainAction {
            action_type: CrossChainActionType::Result,
            rollup_id: our_rollup_id,
            destination: Address::ZERO,
            value: U256::ZERO,
            data: delivery_data,
            failed: delivery_failed,
            source_address: Address::ZERO,
            source_rollup: RollupId::MAINNET,
            scope: ScopePath::root(),
        };
        continuation_entries.push(CrossChainExecutionEntry {
            state_deltas: vec![],
            action_hash: result_our_hash,
            next_action: result_terminal,
        });
    }

    continuation_entries
}

/// Attach chained state deltas to L2-format cross-chain entry pairs.
///
/// Entries are arranged as `[CALL₁, RESULT₁, CALL₂, RESULT₂, ...]`.
/// Given N pairs and N+1 intermediate state roots `[Y, X₁, X₂, ..., X]`,
/// each CALL entry (even index) gets a `StateDelta(roots[i] → roots[i+1])`.
/// RESULT entries (odd index) get no state deltas.
///
/// # Panics
/// Panics if `intermediate_roots.len() != entries.len() / 2 + 1`.
pub fn attach_chained_state_deltas(
    entries: &mut [CrossChainExecutionEntry],
    intermediate_roots: &[B256],
    rollup_id: u64,
) {
    let pair_count = entries.len() / 2;
    assert_eq!(
        intermediate_roots.len(),
        pair_count + 1,
        "need {} intermediate roots for {} pairs, got {}",
        pair_count + 1,
        pair_count,
        intermediate_roots.len(),
    );

    for i in 0..pair_count {
        // The ether_delta for a cross-chain call entry equals the ETH value
        // deposited by the call. This must match the `_etherDelta` accumulated
        // by Rollups.sol during `executeCrossChainCall` (which tracks msg.value).
        // Without this, postBatch reverts with `EtherDeltaMismatch`.
        let call_value = entries[i * 2].next_action.value;
        let ether_delta = if call_value.is_zero() {
            I256::ZERO
        } else {
            I256::try_from(call_value).unwrap_or(I256::ZERO)
        };
        entries[i * 2].state_deltas = vec![CrossChainStateDelta {
            rollup_id: RollupId::new(U256::from(rollup_id)),
            current_state: intermediate_roots[i],
            new_state: intermediate_roots[i + 1],
            ether_delta,
        }];
    }
}

/// The signature hash of the `BatchPosted` event for log filtering.
pub fn batch_posted_signature_hash() -> B256 {
    ICrossChainManagerL2::BatchPosted::SIGNATURE_HASH
}

/// The signature hash of the `ExecutionConsumed` event for log filtering.
pub fn execution_consumed_signature_hash() -> B256 {
    ICrossChainManagerL2::ExecutionConsumed::SIGNATURE_HASH
}

/// Parse `ExecutionConsumed` event logs and return consumed action hashes with
/// their full CALL actions.
///
/// Each `ExecutionConsumed` log has `actionHash` as the first indexed topic
/// (topic[1]) and the full `Action` struct in the event data. The CALL action
/// contains destination, data, sourceAddress — everything fullnodes need to
/// reconstruct L2-format entry pairs during derivation.
///
/// Returns a `ConsumedMap` (actionHash → Vec<CrossChainAction>) preserving all
/// occurrences. Duplicate-call patterns (e.g., CallTwice) emit multiple
/// `ExecutionConsumed` events with the same actionHash; each is recorded so
/// that occurrence-aware consumption in derivation can match them 1:1.
pub fn parse_execution_consumed_logs(logs: &[Log]) -> ConsumedMap {
    let mut consumed: ConsumedMap = std::collections::HashMap::new();
    for log in logs {
        let topics = log.inner.topics();
        if topics.len() < 2 {
            warn!(
                target: "based_rollup::cross_chain",
                "skipping ExecutionConsumed log with fewer than 2 topics"
            );
            continue;
        }
        let action_hash = ActionHash::from_log_boundary(topics[1]);

        // Decode the full Action from event data
        match ICrossChainManagerL2::ExecutionConsumed::decode_log_data(&log.inner.data) {
            Ok(event) => match CrossChainAction::from_sol(&event.action) {
                Ok(action) => {
                    consumed.entry(action_hash).or_default().push(action);
                }
                Err(err) => {
                    warn!(
                        target: "based_rollup::cross_chain",
                        %err,
                        %action_hash,
                        "failed to convert ExecutionConsumed action"
                    );
                    // Still record the hash so the entry is treated as consumed
                    // even if we can't decode the action (defensive).
                    // Only push placeholder if no decoded entry exists yet for this hash.
                    let entries = consumed.entry(action_hash).or_default();
                    if entries.is_empty() {
                        entries.push(CrossChainAction {
                            action_type: CrossChainActionType::Call,
                            rollup_id: RollupId::MAINNET,
                            destination: Address::ZERO,
                            value: U256::ZERO,
                            data: Vec::new(),
                            failed: false,
                            source_address: Address::ZERO,
                            source_rollup: RollupId::MAINNET,
                            scope: ScopePath::root(),
                        });
                    }
                }
            },
            Err(err) => {
                warn!(
                    target: "based_rollup::cross_chain",
                    %err,
                    %action_hash,
                    "failed to decode ExecutionConsumed event data — using hash only"
                );
                // Only push placeholder if no decoded entry exists yet for this hash.
                let entries = consumed.entry(action_hash).or_default();
                if entries.is_empty() {
                    entries.push(CrossChainAction {
                        action_type: CrossChainActionType::Call,
                        rollup_id: RollupId::MAINNET,
                        destination: Address::ZERO,
                        value: U256::ZERO,
                        data: Vec::new(),
                        failed: false,
                        source_address: Address::ZERO,
                        source_rollup: RollupId::MAINNET,
                        scope: ScopePath::root(),
                    });
                }
            }
        }
    }
    consumed
}

/// A parsed execution entry with the L1 block it was posted in.
#[derive(Debug, Clone)]
pub struct DerivedExecutionEntry {
    pub entry: CrossChainExecutionEntry,
    pub l1_block_number: u64,
}

/// Parse `BatchPosted` event logs into execution entries, filtering for the given rollup ID.
///
/// Returns entries where at least one state delta references `rollup_id`,
/// along with the L1 block number each entry was posted in.
pub fn parse_batch_posted_logs(logs: &[Log], rollup_id: U256) -> Vec<DerivedExecutionEntry> {
    let mut entries = Vec::new();

    for log in logs {
        let l1_block = match log.block_number {
            Some(n) => n,
            None => {
                warn!(
                    target: "based_rollup::cross_chain",
                    "skipping BatchPosted log with no block_number"
                );
                continue;
            }
        };

        let event = match ICrossChainManagerL2::BatchPosted::decode_log_data(&log.inner.data) {
            Ok(e) => e,
            Err(err) => {
                warn!(
                    target: "based_rollup::cross_chain",
                    %err,
                    "failed to decode BatchPosted event"
                );
                continue;
            }
        };

        for sol_entry in &event.entries {
            // Include this entry if any state delta references our rollup OR
            // if the nextAction targets our rollup (incoming cross-chain call).
            // Without the nextAction check, incoming calls would be silently dropped.
            let has_state_delta = sol_entry
                .stateDeltas
                .iter()
                .any(|d| d.rollupId == rollup_id);
            let has_incoming_action = sol_entry.nextAction.rollupId == rollup_id;
            let relevant = has_state_delta || has_incoming_action;
            if relevant {
                match CrossChainExecutionEntry::from_sol(sol_entry) {
                    Ok(entry) => {
                        entries.push(DerivedExecutionEntry {
                            entry,
                            l1_block_number: l1_block,
                        });
                    }
                    Err(err) => {
                        warn!(
                            target: "based_rollup::cross_chain",
                            %err,
                            l1_block,
                            "skipping execution entry with invalid action type"
                        );
                    }
                }
            }
        }
    }

    entries
}

// ──────────────────────────────────────────────
//  Builder-signed transaction construction helpers
// ──────────────────────────────────────────────

use alloy_consensus::TxLegacy;
use alloy_signer_local::PrivateKeySigner;
use reth_ethereum_primitives::TransactionSigned;

/// Gas limits for builder protocol transactions.
/// These are upper bounds for the gas_limit field in each tx. The actual gas used
/// is much lower (e.g. ~500K total for a normal block's protocol txs), but we keep
/// moderate headroom to avoid reverts from underestimation.
const DEPLOY_GAS_LIMIT: u64 = 5_000_000;
/// setContext writes 4 storage slots (BlockContext struct) to the contexts mapping
/// plus updates `latest` — cold SSTOREs cost ~20K each, total ~200K gas.
const SET_CONTEXT_GAS_LIMIT: u64 = 250_000;
/// loadExecutionTable writes N entries to storage — each entry requires ~100K gas
/// for the per-actionHash self-clean (delete + push) and pendingEntryCount update.
/// 3M supports up to ~30 entries, covering MAX_RECURSIVE_DEPTH=5 (9 entries for
/// 5-round PingPong) with headroom.
const LOAD_TABLE_GAS_LIMIT: u64 = 3_000_000;
/// Cross-chain calls can trigger complex execution: WrappedToken deployment
/// via CREATE2 (~700K), proxy creation, nested callbacks, etc. 2M provides
/// headroom beyond the ~956K observed for receiveTokens while
/// keeping 3+ protocol txs within the ~30M block gas limit.
const EXECUTE_INCOMING_GAS_LIMIT: u64 = 2_000_000;

/// Build and sign a legacy transaction helper.
#[allow(clippy::too_many_arguments)]
fn build_signed_legacy_tx(
    to: Option<Address>,
    data: Vec<u8>,
    signer: &PrivateKeySigner,
    nonce: u64,
    chain_id: u64,
    gas_price: u128,
    gas_limit: u64,
    value: u128,
) -> eyre::Result<TransactionSigned> {
    use alloy_network::TxSignerSync;

    let mut tx = TxLegacy {
        chain_id: Some(chain_id),
        nonce,
        gas_price,
        gas_limit,
        to: match to {
            Some(addr) => alloy_primitives::TxKind::Call(addr),
            None => alloy_primitives::TxKind::Create,
        },
        value: alloy_primitives::U256::from(value),
        input: alloy_primitives::Bytes::from(data),
    };
    let sig = signer.sign_transaction_sync(&mut tx)?;
    // EthereumTypedTransaction<TxEip4844>::Legacy(tx).into_envelope(sig)
    // produces EthereumTxEnvelope<TxEip4844> which is reth's TransactionSigned
    let typed_tx = reth_ethereum_primitives::Transaction::Legacy(tx);
    Ok(typed_tx.into_envelope(sig))
}

/// Sign a setContext transaction.
pub fn build_set_context_tx(
    l1_block_number: u64,
    l1_block_hash: B256,
    l2_context_address: Address,
    signer: &PrivateKeySigner,
    nonce: u64,
    chain_id: u64,
    gas_price: u128,
) -> eyre::Result<TransactionSigned> {
    let calldata =
        crate::payload_builder::encode_set_context_calldata(&crate::payload_builder::L1BlockInfo {
            l1_block_number,
            l1_block_hash,
        });
    build_signed_legacy_tx(
        Some(l2_context_address),
        calldata.to_vec(),
        signer,
        nonce,
        chain_id,
        gas_price,
        SET_CONTEXT_GAS_LIMIT,
        0,
    )
}

/// Sign a loadExecutionTable transaction.
pub fn build_load_table_tx(
    entries: &[CrossChainExecutionEntry],
    ccm_address: Address,
    signer: &PrivateKeySigner,
    nonce: u64,
    chain_id: u64,
    gas_price: u128,
) -> eyre::Result<TransactionSigned> {
    let calldata = encode_load_execution_table_calldata(entries);
    build_signed_legacy_tx(
        Some(ccm_address),
        calldata.to_vec(),
        signer,
        nonce,
        chain_id,
        gas_price,
        LOAD_TABLE_GAS_LIMIT,
        0,
    )
}

/// Sign an executeIncomingCrossChainCall transaction.
pub fn build_execute_incoming_tx(
    action: &CrossChainAction,
    ccm_address: Address,
    signer: &PrivateKeySigner,
    nonce: u64,
    chain_id: u64,
    gas_price: u128,
) -> eyre::Result<TransactionSigned> {
    let calldata = encode_execute_incoming_call_calldata(action);
    build_signed_legacy_tx(
        Some(ccm_address),
        calldata.to_vec(),
        signer,
        nonce,
        chain_id,
        gas_price,
        EXECUTE_INCOMING_GAS_LIMIT,
        0,
    )
}

/// Sign CREATE transaction for L2Context deployment (block 1).
pub fn build_deploy_l2context_tx(
    authorized_caller: Address,
    signer: &PrivateKeySigner,
    chain_id: u64,
    gas_price: u128,
) -> eyre::Result<TransactionSigned> {
    // Constructor: L2Context(address _authorizedCaller)
    // We need the creation bytecode. For now, use the compiled bytecode
    // from forge. The bytecode includes constructor args appended.
    let bytecode = l2context_creation_bytecode();
    // ABI-encode constructor arg: address padded to 32 bytes
    let mut deploy_data = bytecode;
    deploy_data.extend_from_slice(
        &<alloy_sol_types::sol_data::Address as alloy_sol_types::SolType>::abi_encode(
            &authorized_caller,
        ),
    );
    build_signed_legacy_tx(
        None, // CREATE
        deploy_data,
        signer,
        0, // nonce=0 for first deployment
        chain_id,
        gas_price,
        DEPLOY_GAS_LIMIT,
        0,
    )
}

/// Sign CREATE transaction for CrossChainManagerL2 deployment (block 1).
pub fn build_deploy_ccm_tx(
    rollup_id: u64,
    system_address: Address,
    signer: &PrivateKeySigner,
    chain_id: u64,
    gas_price: u128,
) -> eyre::Result<TransactionSigned> {
    let bytecode = ccm_creation_bytecode();
    // Constructor args: (uint256 _rollupId, address _systemAddress)
    let mut deploy_data = bytecode;
    deploy_data.extend_from_slice(&<(
        alloy_sol_types::sol_data::Uint<256>,
        alloy_sol_types::sol_data::Address,
    )>::abi_encode(&(
        U256::from(rollup_id),
        system_address,
    )));
    build_signed_legacy_tx(
        None, // CREATE
        deploy_data,
        signer,
        1, // nonce=1 for second deployment
        chain_id,
        gas_price,
        DEPLOY_GAS_LIMIT,
        0,
    )
}

/// Sign CREATE transaction for Bridge deployment on L2 (block 1, nonce=2).
/// Bridge has no constructor args — initialized separately via `build_initialize_bridge_tx`.
pub fn build_deploy_bridge_tx(
    signer: &PrivateKeySigner,
    chain_id: u64,
    gas_price: u128,
) -> eyre::Result<TransactionSigned> {
    let bytecode = bridge_creation_bytecode();
    build_signed_legacy_tx(
        None, // CREATE
        bytecode,
        signer,
        2, // nonce=2 (after L2Context=0, CCM=1)
        chain_id,
        gas_price,
        DEPLOY_GAS_LIMIT,
        0,
    )
}

/// Sign initialize transaction for Bridge on L2 (block 1, nonce=3).
/// Calls `initialize(address _manager, uint256 _rollupId, address _admin)`.
pub fn build_initialize_bridge_tx(
    ccm_address: Address,
    rollup_id: u64,
    admin: Address,
    bridge_address: Address,
    signer: &PrivateKeySigner,
    chain_id: u64,
    gas_price: u128,
) -> eyre::Result<TransactionSigned> {
    // initialize(address,uint256,address) selector + args
    let calldata = IBridge::initializeCall {
        _manager: ccm_address,
        _rollupId: U256::from(rollup_id),
        _admin: admin,
    }
    .abi_encode();
    build_signed_legacy_tx(
        Some(bridge_address),
        calldata,
        signer,
        3, // nonce=3
        chain_id,
        gas_price,
        SET_CONTEXT_GAS_LIMIT, // 250K is plenty for initialize
        0,
    )
}

/// Build a signed transaction that calls `Bridge.setCanonicalBridgeAddress(address)`.
///
/// Used as a protocol transaction in block 2 to set the canonical bridge address
/// on the L2 bridge contract. Required for multi-call continuation entries where
/// Bridge_L2._bridgeAddress() must return the L1 bridge address.
pub fn build_set_canonical_bridge_tx(
    bridge_l2_address: Address,
    canonical_address: Address,
    signer: &PrivateKeySigner,
    nonce: u64,
    chain_id: u64,
    gas_price: u128,
) -> eyre::Result<TransactionSigned> {
    let calldata = IBridge::setCanonicalBridgeAddressCall {
        bridgeAddress: canonical_address,
    }
    .abi_encode();
    build_signed_legacy_tx(
        Some(bridge_l2_address),
        calldata,
        signer,
        nonce,
        chain_id,
        gas_price,
        SET_CONTEXT_GAS_LIMIT,
        0,
    )
}

/// Sign a legacy ETH transfer for bootstrap funding at block 1.
pub fn build_bootstrap_transfer_tx(
    to: Address,
    amount_wei: u128,
    signer: &PrivateKeySigner,
    nonce: u64,
    chain_id: u64,
    gas_price: u128,
) -> eyre::Result<TransactionSigned> {
    build_signed_legacy_tx(
        Some(to),
        Vec::new(),
        signer,
        nonce,
        chain_id,
        gas_price,
        21_000,
        amount_wei,
    )
}

/// Estimate total gas budget for a set of builder protocol transactions.
/// Uses the gas_limit from each tx as a conservative upper bound.
pub fn estimate_builder_tx_gas(txs: &[TransactionSigned]) -> u64 {
    use alloy_consensus::Transaction;
    txs.iter().map(|tx| tx.gas_limit()).sum()
}

/// Partition execution entries into table entries (for loadExecutionTable)
/// and trigger entries (CALL actions targeting our rollup, for executeIncomingCrossChainCall).
pub fn partition_entries(
    entries: &[CrossChainExecutionEntry],
    our_rollup_id: RollupId,
) -> (Vec<CrossChainExecutionEntry>, Vec<CrossChainExecutionEntry>) {
    let mut table_entries = Vec::new();
    let mut trigger_entries = Vec::new();
    for entry in entries {
        // A trigger is an entry whose action_hash == hash(next_action).
        // This is true for CALL triggers from convert_l1_entries_to_l2_pairs
        // (action_hash=hash(CALL_A), next_action=CALL_A) but NOT for continuation
        // table entries (action_hash=hash(RESULT), next_action=CALL_B).
        let is_call_to_us = entry.next_action.action_type == CrossChainActionType::Call
            && entry.next_action.rollup_id == our_rollup_id;
        let next_hash = ActionHash::new(keccak256(ICrossChainManagerL2::Action::abi_encode(
            &entry.next_action.to_sol_action(),
        )));
        if is_call_to_us && next_hash == entry.action_hash {
            trigger_entries.push(entry.clone());
        } else {
            table_entries.push(entry.clone());
        }
    }
    (table_entries, trigger_entries)
}

/// L2Context creation bytecode (compiled from L2Context.sol with parameterized constructor).
/// This must be regenerated when L2Context.sol changes via `forge build`.
///
/// The bytecode is loaded from the `L2_CONTEXT_BYTECODE` env var at runtime
/// (set by deploy.sh) or from the forge build artifacts.
/// Falls back to empty bytecode with a warning if not available.
pub fn l2context_creation_bytecode() -> Vec<u8> {
    let bytecode = if let Ok(hex) = std::env::var("L2_CONTEXT_BYTECODE") {
        let hex = hex.strip_prefix("0x").unwrap_or(&hex);
        hex::decode(hex).unwrap_or_else(|_| {
            tracing::warn!("failed to decode L2_CONTEXT_BYTECODE hex, using empty bytecode");
            Vec::new()
        })
    } else {
        // Try to load from forge build output
        let path = std::path::Path::new("contracts/out/L2Context.sol/L2Context.json");
        load_bytecode_from_artifact(path)
    };
    if bytecode.is_empty() {
        tracing::error!(
            "L2Context creation bytecode is empty — contract will deploy with no code! \
             Set L2_CONTEXT_BYTECODE env var or ensure forge build artifacts are available"
        );
    }
    bytecode
}

/// CrossChainManagerL2 creation bytecode.
pub fn ccm_creation_bytecode() -> Vec<u8> {
    let bytecode = if let Ok(hex) = std::env::var("CCM_BYTECODE") {
        let hex = hex.strip_prefix("0x").unwrap_or(&hex);
        hex::decode(hex).unwrap_or_else(|_| {
            tracing::warn!("failed to decode CCM_BYTECODE hex, using empty bytecode");
            Vec::new()
        })
    } else {
        let path =
            std::path::Path::new("contracts/out/CrossChainManagerL2.sol/CrossChainManagerL2.json");
        load_bytecode_from_artifact(path)
    };
    if bytecode.is_empty() {
        tracing::error!(
            "CCM creation bytecode is empty — contract will deploy with no code! \
             Set CCM_BYTECODE env var or ensure forge build artifacts are available"
        );
    }
    bytecode
}

/// Bridge creation bytecode (no constructor args).
pub fn bridge_creation_bytecode() -> Vec<u8> {
    let bytecode = if let Ok(hex) = std::env::var("BRIDGE_BYTECODE") {
        let hex = hex.strip_prefix("0x").unwrap_or(&hex);
        hex::decode(hex).unwrap_or_else(|_| {
            tracing::warn!("failed to decode BRIDGE_BYTECODE hex, using empty bytecode");
            Vec::new()
        })
    } else {
        let path = std::path::Path::new("contracts/out/Bridge.sol/Bridge.json");
        let bc = load_bytecode_from_artifact(path);
        if bc.is_empty() {
            let alt =
                std::path::Path::new("contracts/sync-rollups-protocol/out/Bridge.sol/Bridge.json");
            load_bytecode_from_artifact(alt)
        } else {
            bc
        }
    };
    if bytecode.is_empty() {
        tracing::error!(
            "Bridge creation bytecode is empty — contract will deploy with no code! \
             Set BRIDGE_BYTECODE env var or ensure forge build artifacts are available"
        );
    }
    bytecode
}

/// Load creation bytecode from a forge artifact JSON file.
fn load_bytecode_from_artifact(path: &std::path::Path) -> Vec<u8> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    let Some(hex_str) = json["bytecode"]["object"].as_str() else {
        return Vec::new();
    };
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    hex::decode(hex_str).unwrap_or_default()
}

// ──────────────────────────────────────────────
//  L1 withdrawal trigger helpers
// ──────────────────────────────────────────────

sol! {
    /// Rollups contract interface for proxy management and L2TX trigger.
    interface IRollups {
        function createCrossChainProxy(address originalAddress, uint256 originalRollupId) external returns (address);
        function computeCrossChainProxyAddress(address originalAddress, uint256 originalRollupId) external view returns (address);
        function executeL2TX(uint256 rollupId, bytes calldata rlpEncodedTx) external returns (bytes memory);
    }
}

/// ABI-encode `createCrossChainProxy(address, uint256)` calldata.
pub fn encode_create_proxy_calldata(user: Address, rollup_id: u64) -> Bytes {
    let calldata = IRollups::createCrossChainProxyCall {
        originalAddress: user,
        originalRollupId: U256::from(rollup_id),
    }
    .abi_encode();
    Bytes::from(calldata)
}

// ──────────────────────────────────────────────
//  Generic trigger-based filtering (Step 1 of generic filtering plan)
// ──────────────────────────────────────────────

/// Scan L2 block receipts for transactions that produce `ExecutionConsumed` events
/// from the CrossChainManagerL2. Returns deduplicated tx indices in order.
///
/// A "trigger tx" is any transaction that causes entry consumption — whether it's
/// a protocol tx (`executeIncomingCrossChainCall`) or a user tx (via proxy).
/// The protocol is agnostic to trigger type; only the event matters.
pub fn identify_trigger_tx_indices(
    receipts: &[alloy_consensus::Receipt<alloy_primitives::Log>],
    ccm_address: Address,
) -> Vec<usize> {
    let sig = execution_consumed_signature_hash();
    let mut seen = std::collections::BTreeSet::new();
    for (tx_idx, receipt) in receipts.iter().enumerate() {
        let has_consumed = receipt.logs.iter().any(|log| {
            log.address == ccm_address
                && !log.data.topics().is_empty()
                && log.data.topics()[0] == sig
        });
        if has_consumed {
            seen.insert(tx_idx);
        }
    }
    seen.into_iter().collect()
}

/// Filter a block's transactions by keeping the first `keep_count` trigger txs
/// and removing the rest. Keeps ALL non-trigger txs (loadTable, setContext, user txs).
///
/// `trigger_tx_indices` is the output of [`identify_trigger_tx_indices`].
/// This is the generic replacement for all type-specific filtering functions.
pub fn filter_block_by_trigger_prefix(
    encoded_transactions: &Bytes,
    trigger_tx_indices: &[usize],
    keep_count: usize,
) -> eyre::Result<Bytes> {
    use alloy_rlp::Decodable;

    if encoded_transactions.is_empty() {
        return Ok(Bytes::new());
    }

    // Build the set of tx indices to REMOVE (triggers beyond the kept prefix)
    let remove_set: std::collections::HashSet<usize> = trigger_tx_indices
        .iter()
        .skip(keep_count)
        .copied()
        .collect();

    let txs: Vec<TransactionSigned> = Decodable::decode(&mut encoded_transactions.as_ref())
        .map_err(|e| eyre::eyre!("failed to RLP-decode transactions: {e}"))?;

    let filtered: Vec<TransactionSigned> = txs
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| !remove_set.contains(idx))
        .map(|(_, tx)| tx)
        .collect();

    let mut buf = Vec::new();
    alloy_rlp::encode_list(&filtered, &mut buf);
    Ok(Bytes::from(buf))
}

/// Determine how many L2 trigger txs had their entries fully consumed on L1.
///
/// Walks trigger txs in block order. For each trigger tx, extracts the `actionHash`es
/// from its `ExecutionConsumed` events. Checks that ALL hashes have remaining count > 0
/// in the L1 consumed map. If yes: consumed, decrement counters, continue.
/// If any hash is missing: STOP (prefix counting — §4f).
///
/// Returns the number of consecutive consumed trigger txs from the start.
pub fn compute_consumed_trigger_prefix(
    receipts: &[alloy_consensus::Receipt<alloy_primitives::Log>],
    ccm_address: Address,
    l1_consumed_remaining: &mut std::collections::HashMap<ActionHash, usize>,
    trigger_tx_indices: &[usize],
) -> usize {
    let sig = execution_consumed_signature_hash();
    let mut consumed_count: usize = 0;

    for &tx_idx in trigger_tx_indices {
        let receipt = match receipts.get(tx_idx) {
            Some(r) => r,
            None => return consumed_count,
        };

        // Collect all actionHashes from ExecutionConsumed events in this tx's receipt
        let action_hashes: Vec<ActionHash> = receipt
            .logs
            .iter()
            .filter(|log| {
                log.address == ccm_address
                    && log.data.topics().len() >= 2
                    && log.data.topics()[0] == sig
            })
            .map(|log| ActionHash::from_log_boundary(log.data.topics()[1]))
            .collect();

        if action_hashes.is_empty() {
            // This trigger tx had no ExecutionConsumed events — should not happen
            // for a properly identified trigger, but stop defensively.
            return consumed_count;
        }

        // Check that ALL action hashes have remaining > 0 in the L1 map.
        // We must verify BEFORE decrementing — if any fails, we stop without
        // side effects on the counters for this trigger tx.
        let all_available = action_hashes
            .iter()
            .all(|hash| l1_consumed_remaining.get(hash).copied().unwrap_or(0) > 0);

        if !all_available {
            return consumed_count;
        }

        // All passed — decrement counters
        for hash in &action_hashes {
            if let Some(count) = l1_consumed_remaining.get_mut(hash) {
                *count = count.saturating_sub(1);
            }
        }

        consumed_count += 1;
    }

    consumed_count
}

/// Assign chained state deltas to L1 deferred entries using the intermediate root chain.
///
/// `group_starts[k]` is the index of the first entry in trigger group k.
/// `roots` has T+1 values (T = number of trigger groups).
///
/// For group k with N entries (from `group_starts[k]` to `group_starts[k+1]` or end):
///   - When N == 1: `StateDelta(roots[k] → roots[k+1])`
///   - When N > 1: generates N+1 unique chained roots `[r₀, r₁, ..., rₙ]` where
///     `r₀ = roots[k]`, `rₙ = roots[k+1]`, and intermediate `rⱼ` are synthetic:
///     `rⱼ = keccak256(pre_root || post_root || rollup_id || j)`.
///     Each entry i gets `StateDelta(rᵢ → rᵢ₊₁)`.
///
/// Unique `currentState` per entry is REQUIRED by the protocol: entries with the
/// same `actionHash` (e.g., siblingScopes RESULT chaining) are disambiguated by
/// `_findAndApplyExecution` checking `rollups[rollupId].stateRoot == currentState`.
/// See `contracts/sync-rollups-protocol/script/e2e/siblingScopes/E2E.s.sol:101`.
///
/// Synthetic roots are safe because:
/// - L1 `_findAndApplyExecution` only checks `stateRoot == currentState` (not "real" L2 root)
/// - All entries in a group are consumed atomically by one L1 tx (EVM atomicity)
/// - Fullnode reads entries from L1 postBatch as-is (no independent root recomputation)
/// - The final root (`roots[k+1]`) IS the real L2 state root
pub fn attach_generic_state_deltas(
    entries: &mut [CrossChainExecutionEntry],
    roots: &[B256],
    rollup_id: u64,
    group_starts: &[usize],
) {
    let rollup_id_typed = RollupId::new(U256::from(rollup_id));
    let num_groups = group_starts.len();

    for k in 0..num_groups {
        let start = group_starts[k];
        let end = if k + 1 < num_groups {
            group_starts[k + 1]
        } else {
            entries.len()
        };

        // roots must have at least k+2 values (roots[k] and roots[k+1])
        if k + 1 >= roots.len() {
            warn!(
                target: "based_rollup::cross_chain",
                group = k,
                roots_len = roots.len(),
                "attach_generic_state_deltas: insufficient roots for group"
            );
            break;
        }

        let pre_root = roots[k];
        let post_root = roots[k + 1];
        let group_size = end.saturating_sub(start);
        // Detect REVERT groups by inspecting entry action types directly —
        // no external revert_group_flags parameter needed.
        let is_revert_group = entries[start..end]
            .iter()
            .any(|e| e.next_action.action_type == CrossChainActionType::Revert);

        // Build the per-entry root chain: [r_0, r_1, ..., r_N]
        // r_0 = pre_root, r_N = post_root, intermediates are synthetic.
        //
        // For REVERT groups: _handleScopeRevert captures stateRoot AFTER Entry 1's
        // delta but BEFORE Entry 2's (Rollups.sol:375). The captured value becomes
        // the final stateRoot. So Entry 1's newState must equal post_root (the real
        // block root). Entry 2's delta is consumed inside the reverted scope — its
        // effect is discarded. We use identity (post→post) for the last entry.
        //
        // Normal:  [pre, syn_1, ..., syn_{N-1}, post]
        // REVERT:  [pre, syn_1, ..., post, post]  (last 2 = post)
        let entry_roots: Vec<B256> = (0..=group_size)
            .map(|j| {
                if j == 0 {
                    pre_root
                } else if is_revert_group && j >= group_size.saturating_sub(1) {
                    // REVERT group: second-to-last and last roots = post_root
                    post_root
                } else if j == group_size {
                    post_root
                } else {
                    // Synthetic: keccak256(pre || post || rollup_id || j)
                    use alloy_primitives::keccak256;
                    let mut buf = Vec::with_capacity(32 + 32 + 8 + 8);
                    buf.extend_from_slice(pre_root.as_slice());
                    buf.extend_from_slice(post_root.as_slice());
                    buf.extend_from_slice(&rollup_id.to_be_bytes());
                    buf.extend_from_slice(&(j as u64).to_be_bytes());
                    keccak256(&buf)
                }
            })
            .collect();

        for (idx, i) in (start..end).enumerate() {
            if i >= entries.len() {
                break;
            }

            // Preserve existing ether_delta if the entry already has state deltas
            let existing_ether_delta = entries[i]
                .state_deltas
                .first()
                .map(|d| d.ether_delta)
                .unwrap_or(I256::ZERO);

            entries[i].state_deltas = vec![CrossChainStateDelta {
                rollup_id: rollup_id_typed,
                current_state: entry_roots[idx],
                new_state: entry_roots[idx + 1],
                ether_delta: existing_ether_delta,
            }];
        }
    }
}

#[cfg(test)]
#[path = "cross_chain_tests.rs"]
mod tests;
