//! L1 block submission via the Rollups contract.
//!
//! The proposer posts sealed L2 blocks to L1 by calling `postBatch()` with
//! execution entries (immediate block entries + optional cross-chain deferred
//! entries), block calldata, and a proof.

use crate::config::RollupConfig;
use crate::cross_chain::{CleanStateRoot, CrossChainExecutionEntry};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_network::EthereumWallet;
use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, TxKind};
use alloy_primitives::{B256, Bytes, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use eyre::Result;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

sol! {
    /// Rollups.rollups(uint256) view returns (address, bytes32, bytes32, uint256)
    function rollups(uint256 rollupId) external view returns (address owner, bytes32 verificationKey, bytes32 stateRoot, uint256 etherBalance);
}

/// A block pending submission to L1.
#[derive(Debug, Clone)]
pub struct PendingBlock {
    pub l2_block_number: u64,
    pub pre_state_root: B256,
    pub state_root: B256,
    pub clean_state_root: CleanStateRoot,
    pub encoded_transactions: Bytes,
    /// Intermediate state roots for cross-chain entries (deposits and/or withdrawals).
    /// Empty when no cross-chain entries in this block.
    /// Contains D+W+1 roots for D deposits and W withdrawals, where roots[0] is the
    /// clean root (state without any cross-chain entry txs) and the last root is the
    /// speculative root.
    pub intermediate_roots: Vec<B256>,
    /// L1 block number stamped into this L2 block's header `mix_hash` at build
    /// time. Used by the proposer to compute `target_block_number` for bundle
    /// submission so that the signed `(parent_hash, timestamp)` matches the
    /// inclusion context of the L1 block where derivation will assign
    /// `l1_context = l1_context_block`. Without this, `build_proof_context`
    /// reads a fresh `latest_l1_block` at flush time which has typically
    /// drifted past the build-time `mix_hash`, producing post-confirmation
    /// L1-context mismatches.
    pub l1_context_block: u64,
}

/// Gas price hint for the postBatch transaction.
///
/// Derived from queued user L1 transactions to ensure the builder's
/// postBatch is ordered before the user's cross-chain tx within the
/// same L1 block. Miners/validators order transactions by priority fee.
#[derive(Debug, Clone)]
pub struct GasPriceHint {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// Inputs to [`Proposer::sign_proof`]. Introduced in PLAN §8 step 1.8
/// to close invariant #22 ("`publicInputsHash` uses
/// `block.timestamp`, not `block.number`").
///
/// Pre-1.8, `sign_proof` took `(entries, call_data)` and computed the
/// timestamp internally from `latest_l1_block().timestamp + block_time`.
/// That was correct, but the field name inside `compute_public_inputs_hash`
/// was a bare `u64` parameter called `block_timestamp`. A future
/// refactor that passed `block.number` instead of `block.timestamp`
/// (e.g., copy-paste from a different hash site) would silently
/// change the `publicInputsHash` and cause the on-chain proof check
/// to fail.
///
/// With `ProofContext`, the caller must construct the struct with a
/// named `block_timestamp: u64` field — any site that accidentally
/// passes `block.number` is visible at the construction site and
/// gets the field name to read during review.
///
/// The struct also groups the inputs that logically belong together
/// (parent hash, timestamp, entry hashes, calldata) so future inputs
/// (e.g., blob hashes) have one place to land.
#[derive(Debug, Clone)]
pub struct ProofContext {
    /// Target L1 block number (informational — not part of the
    /// public inputs hash). Used for retry logic and logging.
    pub target_block_number: u64,
    /// Parent block hash (`blockhash(block.number - 1)` on-chain).
    /// This IS part of the public inputs hash.
    pub parent_block_hash: B256,
    /// Predicted `block.timestamp` at the time `postBatch` executes.
    /// Computed as `latest_ts + block_time` (or `max(latest+1, now)`
    /// for chains without fixed block time). This IS part of the
    /// public inputs hash — per invariant #22 it is the TIMESTAMP,
    /// NOT the block number.
    pub block_timestamp: u64,
    /// Hashes of the cross-chain execution entries being submitted.
    /// Computed via `compute_entry_hashes(entries, verification_key)`.
    pub entry_hashes: Vec<B256>,
}

/// `#[must_use]` sentinel returned by [`Proposer::send_l1_tx_with_nonce`]
/// on failure. The only way to consume the token is by calling
/// [`Proposer::reset_nonce`]. Introduced in PLAN §8 step 1.8 to
/// close invariant #2 ("ALWAYS call `reset_nonce` after any L1 tx
/// failure").
///
/// Pre-1.8, the rule was enforced by hand discipline: callers had to
/// remember to call `reset_nonce()` in the error arm. Any future
/// refactor that added a new L1 send path and forgot to reset would
/// reintroduce the CLAUDE.md bug "Nonce gap = invisible death —
/// builder keeps building while submissions are stuck in queued
/// pool" permanently.
///
/// Post-1.8, the function returns `NonceSendError { reset_required,
/// source }` on failure. The `reset_required` field is
/// `#[must_use]`, so the caller that ignores it gets a clippy warning
/// promoted to an error by `-D warnings`. The only way to discharge
/// the obligation is by calling `proposer.reset_nonce(token)`, which
/// takes the token by value and consumes it.
#[must_use = "NonceResetRequired must be discharged by calling \
              proposer.reset_nonce(token) — invariant #2 requires \
              a nonce reset after any L1 tx failure"]
#[derive(Debug)]
pub struct NonceResetRequired {
    /// Opaque seal — prevents external construction. The only
    /// way to obtain a `NonceResetRequired` is from a
    /// `NonceSendError` returned by
    /// [`Proposer::send_l1_tx_with_nonce`].
    _seal: (),
}

/// Error returned by [`Proposer::send_l1_tx_with_nonce`] when the
/// underlying L1 transaction fails. Carries both the original error
/// and a [`NonceResetRequired`] token the caller must consume by
/// calling [`Proposer::reset_nonce`].
#[must_use = "NonceSendError must be consumed — callers must discharge \
              the NonceResetRequired token (invariant #2)"]
#[derive(Debug)]
pub struct NonceSendError {
    /// The token that forces a `reset_nonce` call.
    pub reset_required: NonceResetRequired,
    /// The original error from the L1 RPC call.
    pub source: eyre::Report,
}

impl std::fmt::Display for NonceSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "L1 tx with explicit nonce failed: {}", self.source)
    }
}

impl std::error::Error for NonceSendError {}

/// Posts sealed L2 blocks to the L1 Rollups contract.
///
/// Uses a wallet-equipped provider to sign and send transactions.
/// Minimum wallet balance (0.01 ETH) — warn when below this threshold.
const LOW_BALANCE_THRESHOLD: u128 = 10_000_000_000_000_000;

pub struct Proposer {
    config: Arc<RollupConfig>,
    /// Type-erased provider with wallet fillers. Always points at
    /// `config.l1_rpc_url` — used for both reads and (when no builder
    /// RPC is configured) writes via `send_transaction`.
    provider: Box<dyn Provider + Send + Sync>,
    /// The signer for ECDSA proof generation.
    signer: PrivateKeySigner,
    /// The signer address (for balance checks).
    signer_address: Address,
    /// HTTP client for POSTing `eth_sendBundle` to `config.l1_builder_rpc_url`.
    /// `None` when no builder RPC is configured; in that case writes go
    /// through `provider.send_transaction`.
    builder_http: Option<reqwest::Client>,
    /// Target L1 block number of the most recent bundle submission.
    ///
    /// Set by `send_via_bundle` so that `wait_for_l1_receipt` can use the
    /// bundle's deterministic drop-on-miss semantics: the tx is either
    /// included in this exact block or discarded, so we know the outcome
    /// as soon as `target_block` is produced on L1 (~5s max). This
    /// eliminates the 120s receipt-poll worst-case for dropped bundles
    /// and structurally prevents "silent confirm after timeout" drift.
    ///
    /// `0` when no bundle has been submitted yet OR the most recent
    /// submission used the `send_transaction` (non-bundle) path; in
    /// that case `wait_for_l1_receipt` falls through to the legacy
    /// time-bounded polling loop.
    last_bundle_target: std::sync::atomic::AtomicU64,
}

impl Proposer {
    pub fn new(config: Arc<RollupConfig>) -> Result<Self> {
        let key_hex = config
            .builder_private_key
            .as_deref()
            .ok_or_else(|| eyre::eyre!("BUILDER_PRIVATE_KEY required for proposer"))?;

        let signer: PrivateKeySigner = key_hex.parse()?;
        let signer_address = signer.address();
        let wallet = EthereumWallet::from(signer.clone());

        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_http(config.l1_rpc_url.parse()?);

        let builder_http = if config.l1_builder_rpc_url.is_some() {
            Some(
                reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .map_err(|e| eyre::eyre!("failed to build builder HTTP client: {e}"))?,
            )
        } else {
            None
        };

        Ok(Self {
            config,
            provider: Box::new(provider),
            signer,
            signer_address,
            builder_http,
            last_bundle_target: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Read the on-chain state root from the Rollups contract for this rollup.
    pub async fn last_submitted_state_root(&self) -> Result<B256> {
        let calldata = rollupsCall {
            rollupId: U256::from(self.config.rollup_id),
        }
        .abi_encode();

        let result = self
            .provider
            .call(
                alloy_rpc_types::TransactionRequest::default()
                    .to(self.config.rollups_address)
                    .input(calldata.into()),
            )
            .await?;

        let decoded = rollupsCall::abi_decode_returns(&result)?;
        Ok(decoded.stateRoot)
    }

    /// Read the on-chain verification key from the Rollups contract for this rollup.
    pub async fn verification_key(&self) -> Result<B256> {
        let calldata = rollupsCall {
            rollupId: U256::from(self.config.rollup_id),
        }
        .abi_encode();

        let result = self
            .provider
            .call(
                alloy_rpc_types::TransactionRequest::default()
                    .to(self.config.rollups_address)
                    .input(calldata.into()),
            )
            .await?;

        let decoded = rollupsCall::abi_decode_returns(&result)?;
        Ok(decoded.verificationKey)
    }

    /// Fetch the latest L1 block number, hash, and timestamp.
    ///
    /// Used to predict the L1 execution context for ECDSA proof signing.
    /// The builder signs `publicInputsHash` computed with the latest block's hash as
    /// `blockhash(block.number - 1)` and predicts the next block's `block.timestamp`.
    async fn latest_l1_block(&self) -> Result<(u64, B256, u64)> {
        let block = self
            .provider
            .get_block_by_number(alloy_rpc_types::BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre::eyre!("failed to fetch latest L1 block"))?;
        let number = block.header.number;
        let hash = block.header.hash;
        let timestamp = block.header.timestamp;
        Ok((number, hash, timestamp))
    }

    /// Compute and sign the ECDSA proof for `postBatch`.
    ///
    /// Mirrors Rollups.sol's `publicInputsHash` computation:
    /// ```solidity
    /// keccak256(abi.encodePacked(
    ///     blockhash(block.number - 1), block.timestamp,
    ///     abi.encode(entryHashes), abi.encode(blobHashes),
    ///     keccak256(callData)
    /// ))
    /// ```
    ///
    /// The builder targets the NEXT L1 block. The proof includes `block.timestamp`
    /// (predicted as current system time). If the tx lands in a later block or the
    /// timestamp doesn't match, the proof fails and retry logic re-attempts.
    /// Build the [`ProofContext`] for the NEXT L1 block. Separates
    /// "what goes into the hash" from "how the hash is signed".
    /// Introduced in PLAN §8 step 1.8 so that `sign_proof` has a
    /// named, typed input struct (closes invariant #22).
    ///
    /// `anchor_l1_block_hint`: if `Some(N)`, build the proof targeting
    /// L1 block `N + 1` and fetch hash/timestamp of block `N` directly,
    /// rather than reading the current L1 latest. Used when submitting
    /// a batch whose blocks were stamped with `mix_hash = N` at build
    /// time — keeps the signed `(parent_hash, timestamp)` consistent
    /// with what derivation will assign as `l1_context = N` once the
    /// batch lands at L1 block `N + 1`. Without this hint, fresh
    /// `latest_l1_block` would have drifted past `N` (especially during
    /// catchup), the bundle would target `latest+1 ≠ N+1`, and the
    /// post-confirmation verify path would detect an L1-context
    /// mismatch and force a sibling reorg.
    async fn build_proof_context(
        &self,
        entries: &[CrossChainExecutionEntry],
        anchor_l1_block_hint: Option<u64>,
    ) -> Result<ProofContext> {
        let vk = self.verification_key().await?;
        let (latest_number, latest_hash, latest_timestamp) = match anchor_l1_block_hint {
            Some(n) => {
                let block = self
                    .provider
                    .get_block_by_number(n.into())
                    .await?
                    .ok_or_else(|| {
                        eyre::eyre!(
                            "anchor L1 block {n} not yet available on RPC for proof context"
                        )
                    })?;
                (n, block.header.hash, block.header.timestamp)
            }
            None => self.latest_l1_block().await?,
        };
        // Predict the next block's timestamp.
        // On L1 with fixed block time (e.g., reth --dev.block-time=12s):
        //   next_ts = latest_ts + block_time
        // On L1 without fixed block time:
        //   next_ts = max(latest_ts + 1, now)
        // Use the L1 slot time from config (defaults to 12s).
        //
        // **Invariant #22** — the public inputs hash uses
        // `block.timestamp`, NOT `block.number`. This struct's
        // `block_timestamp` field is named to make that explicit at
        // every construction site.
        let block_timestamp = latest_timestamp + self.config.block_time;
        let entry_hashes = crate::cross_chain::compute_entry_hashes(entries, vk);
        Ok(ProofContext {
            target_block_number: latest_number + 1,
            parent_block_hash: latest_hash,
            block_timestamp,
            entry_hashes,
        })
    }

    /// Sign the publicInputsHash from a [`ProofContext`].
    ///
    /// This is the canonical site that turns a `ProofContext` into an
    /// ECDSA signature. `compute_public_inputs_hash` reads
    /// `ctx.block_timestamp` — per invariant #22, this is the
    /// TIMESTAMP, not the block number. The named field on
    /// `ProofContext` documents this at every call site and makes
    /// the "pass block.number by mistake" bug impossible without a
    /// visible field name mismatch.
    fn sign_proof(&self, ctx: &ProofContext, call_data: &Bytes) -> Result<Bytes> {
        let public_inputs_hash = crate::cross_chain::compute_public_inputs_hash(
            &ctx.entry_hashes,
            call_data,
            ctx.parent_block_hash,
            ctx.block_timestamp,
        );

        // Sign the raw hash (NO EIP-191 prefix) — tmpECDSAVerifier uses ecrecover directly
        let sig = self.signer.sign_hash_sync(&public_inputs_hash)?;

        // Encode as 65 bytes: r(32) + s(32) + v(1), where v is 27 or 28
        let sig_bytes = sig.as_bytes();
        // alloy's as_bytes() returns [r, s, v] with v as 0 or 1 — normalize to 27/28
        let mut proof = sig_bytes.to_vec();
        if proof.len() == 65 && proof[64] < 27 {
            proof[64] += 27;
        }

        info!(
            target: "based_rollup::proposer",
            target_block = ctx.target_block_number,
            parent_hash = %ctx.parent_block_hash,
            public_inputs_hash = %public_inputs_hash,
            "signed ECDSA proof for postBatch"
        );

        Ok(Bytes::from(proof))
    }

    /// Check the builder wallet balance and warn if it's below the threshold.
    /// Returns the balance in wei, or an error if the RPC call fails.
    pub async fn check_wallet_balance(&self) -> Result<u128> {
        let balance = self.provider.get_balance(self.signer_address).await?;

        let balance_u128: u128 = balance.try_into().unwrap_or(u128::MAX);
        if balance_u128 < LOW_BALANCE_THRESHOLD {
            warn!(
                target: "based_rollup::proposer",
                address = %self.signer_address,
                balance_wei = balance_u128,
                balance_eth = %format!("{:.6}", balance_u128 as f64 / 1e18),
                "builder wallet balance is LOW — submissions may fail"
            );
        }
        Ok(balance_u128)
    }

    /// Maximum number of receipt polling attempts before giving up.
    /// Must cover at least one L1 block time to avoid submitting the
    /// next batch before the current one is confirmed.
    /// 15 × 2s = 30s — covers 2.5 Ethereum blocks (12s) or 6 Gnosis/Chiado
    /// blocks (5s).
    ///
    /// Only used on the legacy `eth_sendRawTransaction` path. On the
    /// bundle-RPC path (`wait_for_bundle_outcome`) the outcome is
    /// determined by whether the bundle's `target_block` contains the
    /// tx — that check resolves in ≤ one L1 slot regardless of drop
    /// rate, so no wide poll window is needed.
    const RECEIPT_POLL_ATTEMPTS: u32 = 15;

    /// Delay between receipt polling attempts.
    const RECEIPT_POLL_DELAY: Duration = Duration::from_secs(2);

    /// Poll for a transaction receipt. Returns `Ok((tx_hash, l1_block_number))`
    /// on confirmed success, `Err` on revert or timeout.
    ///
    /// Returning `Err` on timeout is critical: the driver re-queues the blocks
    /// and retries after a cooldown, preventing the next `postBatch` from
    /// racing the unconfirmed one. Two `postBatch` calls in the same L1 block
    /// will revert the second (`lastStateUpdateBlock` check in Rollups.sol).
    async fn wait_for_receipt(&self, tx_hash: B256, label: &str) -> Result<(B256, u64)> {
        for attempt in 1..=Self::RECEIPT_POLL_ATTEMPTS {
            tokio::time::sleep(Self::RECEIPT_POLL_DELAY).await;

            match self.provider.get_transaction_receipt(tx_hash).await {
                Ok(Some(receipt)) => {
                    if receipt.status() {
                        let l1_block_number = receipt.block_number.unwrap_or(0);
                        info!(
                            target: "based_rollup::proposer",
                            %tx_hash,
                            l1_block_number,
                            "{label} confirmed on L1"
                        );
                        return Ok((tx_hash, l1_block_number));
                    } else {
                        return Err(eyre::eyre!("{label} reverted on L1 (tx_hash={tx_hash})"));
                    }
                }
                Ok(None) => {
                    if attempt < Self::RECEIPT_POLL_ATTEMPTS {
                        warn!(
                            target: "based_rollup::proposer",
                            %tx_hash,
                            attempt,
                            "receipt not yet available, retrying"
                        );
                    }
                }
                Err(err) => {
                    warn!(
                        target: "based_rollup::proposer",
                        %tx_hash,
                        %err,
                        attempt,
                        "failed to fetch receipt, retrying"
                    );
                }
            }
        }

        Err(eyre::eyre!(
            "{label} receipt not available after {} attempts (tx_hash={tx_hash}) — \
             will re-queue and retry after cooldown",
            Self::RECEIPT_POLL_ATTEMPTS
        ))
    }

    /// Maximum estimated calldata gas per batch. Must be small enough that
    /// `MAX_CALLDATA_GAS + execution overhead` fits within `POST_BATCH_GAS_LIMIT`,
    /// which in turn must sit below the L1 block gas limit.
    const MAX_CALLDATA_GAS: u64 = 7_000_000;

    /// Explicit gas limit for `postBatch` transactions. Set on the tx so that
    /// alloy's filler chain skips `eth_estimateGas` before broadcast.
    ///
    /// Why skip estimation: `publicInputsHash` commits to `block.timestamp`,
    /// which the builder predicts as `latest_ts + block_time`. Some L1 RPC
    /// nodes (observed on Chiado 2026-04-22) run `eth_call`/`eth_estimateGas`
    /// with `block.timestamp = latest_ts` instead of the pending-slot
    /// timestamp. Under that policy every estimation reverts with
    /// `InvalidProof()` and the tx never reaches the mempool — deterministic
    /// 100% failure despite the proof being valid for the real mined block.
    /// Supplying a gas limit here bypasses estimation so the tx is broadcast;
    /// mining then uses the actual next-slot `block.timestamp`, which matches
    /// what we signed.
    ///
    /// Sizing: must sit below the L1 block gas limit, or RPC nodes reject
    /// with `-32000 exceeds block gas limit` pre-mempool. Observed Chiado
    /// block gas limit ~12.5M (dropping slowly via EIP-1559 elastic target);
    /// 10M gives headroom for future drops while still comfortably covering
    /// MAX_CALLDATA_GAS (7M) + ~2-3M execution overhead. For L1s with
    /// consistently smaller block gas limits this needs to be reduced.
    /// TODO: dynamic sizing that reads the current L1 block gas limit and
    /// caps to `block_gas_limit - safety_margin`, removing the hardcode.
    const POST_BATCH_GAS_LIMIT: u64 = 10_000_000;

    /// Submit L2 blocks and optionally cross-chain execution entries to L1
    /// via `postBatch()`. Blocks are aggregated into a single immediate entry
    /// with StateDelta spanning the entire batch (first pre → last post).
    /// Cross-chain entries are appended as deferred entries.
    ///
    /// Returns the postBatch tx hash immediately after sending (before mining).
    /// The caller MUST call [`wait_for_l1_receipt`] to confirm the tx was mined.
    /// This split allows the driver to forward queued L1 txs (user's cross-chain
    /// calls) into the same L1 block as postBatch, satisfying the
    /// `ExecutionNotInCurrentBlock` constraint in Rollups.sol.
    pub async fn send_to_l1(
        &self,
        blocks: &[PendingBlock],
        cross_chain_entries: &[CrossChainExecutionEntry],
        gas_price_hint: Option<GasPriceHint>,
    ) -> Result<B256> {
        if blocks.is_empty() && cross_chain_entries.is_empty() {
            return Err(eyre::eyre!("nothing to submit"));
        }

        // Build a single aggregate immediate entry for all blocks.
        // currentState = first block's pre_state_root (must match on-chain).
        // newState = last block's clean_state_root (block state WITHOUT cross-chain
        // entry effects — see docs/DERIVATION.md §3d). The on-chain stateRoot advances to
        // this clean root; deferred entry consumption evolves it further (§3e).
        // Empty blocks are included in callData but don't add extra entries.
        let mut all_entries = Vec::new();
        if !blocks.is_empty() {
            let first_pre = blocks.first().expect("non-empty").pre_state_root;
            let last_post = blocks.last().expect("non-empty").clean_state_root;
            all_entries.push(crate::cross_chain::build_aggregate_block_entry(
                first_pre,
                last_post.as_b256(),
                self.config.rollup_id,
            ));
        }
        all_entries.extend_from_slice(cross_chain_entries);

        // Encode block calldata
        let call_data = if blocks.is_empty() {
            Bytes::new()
        } else {
            let numbers: Vec<u64> = blocks.iter().map(|b| b.l2_block_number).collect();
            let txs: Vec<Bytes> = blocks
                .iter()
                .map(|b| b.encoded_transactions.clone())
                .collect();
            crate::cross_chain::encode_block_calldata(&numbers, &txs)
        };

        // Compute ECDSA proof: sign the publicInputsHash that Rollups.sol will
        // compute, targeting the next L1 block. If the tx lands in a later block
        // the proof fails and the existing retry logic will re-attempt.
        // Build the proof context (reads latest L1 block, predicts
        // next timestamp, computes entry hashes) and sign. The
        // `ProofContext` separation closes invariant #22 by giving
        // `block_timestamp` a named field on the input struct.
        //
        // When the first pending block carries an `l1_context_block` (set at
        // build time to the `mix_hash` L1 block number it was stamped with),
        // anchor the proof context to that block: target = anchor + 1, parent
        // hash and timestamp fetched for `anchor`. This keeps the signed
        // `(parent_hash, timestamp)` consistent with the L1 context that
        // derivation will assign when the batch lands. Falls back to fresh
        // `latest_l1_block` for tests / single-pending-cross-chain-only paths
        // where no `PendingBlock` is involved.
        let anchor_hint = blocks.first().map(|b| b.l1_context_block);
        let proof_ctx = self
            .build_proof_context(&all_entries, anchor_hint)
            .await?;
        let proof = self.sign_proof(&proof_ctx, &call_data)?;

        let calldata =
            crate::cross_chain::encode_post_batch_calldata(&all_entries, call_data, proof);

        // Check calldata gas cost
        let calldata_gas: u64 = calldata
            .iter()
            .map(|&b| if b == 0 { 4u64 } else { 16u64 })
            .sum();
        if calldata_gas > Self::MAX_CALLDATA_GAS {
            return Err(eyre::eyre!(
                "calldata gas ({calldata_gas}) exceeds limit ({}), reduce batch size",
                Self::MAX_CALLDATA_GAS
            ));
        }

        let mut tx = alloy_rpc_types::TransactionRequest::default()
            .to(self.config.rollups_address)
            .input(calldata.into())
            .gas_limit(Self::POST_BATCH_GAS_LIMIT);

        if let Some(hint) = &gas_price_hint {
            tx = tx
                .max_fee_per_gas(hint.max_fee_per_gas)
                .max_priority_fee_per_gas(hint.max_priority_fee_per_gas);
            info!(
                target: "based_rollup::proposer",
                max_fee_per_gas = hint.max_fee_per_gas,
                max_priority_fee_per_gas = hint.max_priority_fee_per_gas,
                overbid_pct = self.config.l1_gas_overbid_pct,
                "using gas overbid from queued user L1 tx"
            );
        }

        let tx_hash = if let (Some(http), Some(url)) = (
            self.builder_http.as_ref(),
            self.config.l1_builder_rpc_url.as_deref(),
        ) {
            // Builder-RPC / bundle mode: construct, sign, and send via
            // `eth_sendBundle` targeting `proof_ctx.target_block_number`.
            // Bundle is either included in that block or silently dropped —
            // matching the proof's committed `(parent_hash, timestamp)`.
            self.send_via_bundle(&tx, proof_ctx.target_block_number, http, url)
                .await
                .map_err(|err| {
                    warn!(target: "based_rollup::proposer", %err, "failed to submit bundle to builder RPC");
                    err
                })?
        } else {
            // Standard `eth_sendRawTransaction` path: tx enters public mempool
            // and lands in whichever block the next proposer picks it up in.
            let pending = self.provider.send_transaction(tx).await.map_err(|err| {
                warn!(target: "based_rollup::proposer", %err, "failed to submit to L1");
                eyre::eyre!(err)
            })?;
            *pending.tx_hash()
        };
        if !blocks.is_empty() {
            let first = blocks.first().expect("non-empty").l2_block_number;
            let last = blocks.last().expect("non-empty").l2_block_number;
            info!(
                target: "based_rollup::proposer",
                %tx_hash,
                block_count = blocks.len(),
                l2_blocks = %format!("{first}..{last}"),
                entry_count = cross_chain_entries.len(),
                "submitted to L1 via postBatch"
            );
        } else {
            info!(
                target: "based_rollup::proposer",
                %tx_hash,
                entry_count = cross_chain_entries.len(),
                "submitted cross-chain entries to L1 via postBatch"
            );
        }

        Ok(tx_hash)
    }

    /// Construct, sign, and submit the postBatch transaction via
    /// `eth_sendBundle` to a block-builder RPC.
    ///
    /// Unlike `eth_sendRawTransaction`, `eth_sendBundle` targets a specific L1
    /// block number: the builder either includes the tx in that block (if it
    /// is the proposer for that slot and the tx fits) or silently drops the
    /// bundle. Nothing ever lingers in a mempool to be picked up by a later
    /// block. That guarantees the signed `(parent_hash, block.timestamp)` in
    /// `publicInputsHash` matches the actual inclusion context, closing the
    /// timing race where `eth_sendRawTransaction` can land the tx 1+ blocks
    /// later than the builder predicted.
    ///
    /// When the bundle is dropped, the existing receipt-timeout path in the
    /// driver re-queues the pending blocks and retries on the next tick with
    /// a fresh `latest_l1_block`. Nonce is not consumed on drop (the tx never
    /// mines), so retries use the same nonce value.
    async fn send_via_bundle(
        &self,
        tx: &alloy_rpc_types::TransactionRequest,
        target_block_number: u64,
        http: &reqwest::Client,
        url: &str,
    ) -> Result<B256> {
        // Fill in any fields not already supplied by the caller.
        let chain_id = self.provider.get_chain_id().await?;
        let nonce = self
            .provider
            .get_transaction_count(self.signer_address)
            .pending()
            .await?;

        // Cap on `max_priority_fee_per_gas`. EIP-1559 validation requires the
        // sender's balance to cover `gas_limit * max_fee_per_gas + value`
        // BEFORE the tx executes — even if `gas_used` and the actual base fee
        // would charge a fraction of that. With our `gas_limit = 10_000_000`,
        // any priority fee above ~50 gwei pushes the budget past 0.5 xDAI; on
        // Chiado, `eth_maxPriorityFeePerGas` has been observed returning
        // ~230 gwei, which forces a 2.3 xDAI minimum balance just to be
        // accepted by the L1 mempool / bundle relay. Builder wallets
        // typically hold ≤2 xDAI, so the tx is silently rejected as invalid
        // by validators (no error returned to us by the relay's `result:null`
        // ack).
        //
        // 5 gwei is well above the practical Chiado floor (probes land with
        // 2 gwei) and keeps the worst-case fee budget at
        // `10_000_000 * 5 gwei = 0.05 xDAI`, two orders of magnitude under
        // any reasonable builder balance. base_fee is ~0 on Chiado so this
        // dominates `max_fee`.
        const PRIORITY_FEE_CAP: u128 = 5_000_000_000; // 5 gwei

        // Gas price: prefer caller's hint, else fetch latest from node.
        let (max_fee_per_gas, max_priority_fee_per_gas) =
            match (tx.max_fee_per_gas, tx.max_priority_fee_per_gas) {
                (Some(m), Some(p)) => (m.min(PRIORITY_FEE_CAP * 2), p.min(PRIORITY_FEE_CAP)),
                _ => {
                    let max_priority_fee = self
                        .provider
                        .get_max_priority_fee_per_gas()
                        .await
                        .unwrap_or(1_000_000_000)
                        .min(PRIORITY_FEE_CAP);
                    let latest = self
                        .provider
                        .get_block_by_number(alloy_rpc_types::BlockNumberOrTag::Latest)
                        .await?
                        .ok_or_else(|| eyre::eyre!("failed to fetch latest L1 block for gas"))?;
                    let base_fee = latest.header.base_fee_per_gas.unwrap_or(0) as u128;
                    // max_fee = 2 * base_fee + priority_fee (standard formula).
                    let max_fee = base_fee.saturating_mul(2).saturating_add(max_priority_fee);
                    (max_fee, max_priority_fee)
                }
            };

        let gas_limit = tx.gas.unwrap_or(Self::POST_BATCH_GAS_LIMIT);

        let input_bytes = tx
            .input
            .input
            .as_ref()
            .cloned()
            .unwrap_or_default();

        let to_address = match tx.to {
            Some(TxKind::Call(addr)) => addr,
            _ => {
                return Err(eyre::eyre!(
                    "send_via_bundle expects a call tx with a `to` address"
                ));
            }
        };

        let unsigned = TxEip1559 {
            chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to: TxKind::Call(to_address),
            value: U256::ZERO,
            input: input_bytes,
            access_list: Default::default(),
        };

        let sig_hash = unsigned.signature_hash();
        let sig = self.signer.sign_hash_sync(&sig_hash)?;
        // `into_signed` computes and caches the tx hash (keccak256 of the
        // 2718-encoded tx). Using `Signed::new_unchecked(.., Default::default())`
        // would store a zero hash — the receipt poll would then never match
        // any real tx.
        let signed = unsigned.into_signed(sig);
        let envelope = TxEnvelope::Eip1559(signed);
        let raw_bytes = envelope.encoded_2718();
        let tx_hash = *envelope.tx_hash();

        let raw_hex = format!("0x{}", alloy_primitives::hex::encode(&raw_bytes));
        let target_hex = format!("0x{:x}", target_block_number);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_sendBundle",
            "params": [{
                "txs": [raw_hex],
                "blockNumber": target_hex,
            }],
        });

        let resp = http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre::eyre!("builder RPC request failed: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| eyre::eyre!("builder RPC read body failed: {e}"))?;

        if !status.is_success() {
            return Err(eyre::eyre!(
                "builder RPC returned HTTP {status}: {text}"
            ));
        }

        // Parse JSON-RPC response. An `error` field means the bundle was
        // rejected outright (e.g. malformed). A `result` field (usually
        // `null` for rbuilder) means the bundle was accepted; whether it
        // actually lands in the target block is decided at sealing time.
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| eyre::eyre!("builder RPC response not JSON: {e} (body: {text})"))?;
        if let Some(err) = parsed.get("error") {
            return Err(eyre::eyre!("builder RPC error: {err}"));
        }

        // Record the target so `wait_for_l1_receipt` can use bundle-aware
        // inclusion check (drop-on-miss semantics) instead of open-ended
        // receipt polling.
        self.last_bundle_target
            .store(target_block_number, std::sync::atomic::Ordering::Relaxed);

        info!(
            target: "based_rollup::proposer",
            %tx_hash,
            target_block = target_block_number,
            url,
            "submitted bundle to builder RPC"
        );

        Ok(tx_hash)
    }

    /// Wait for the outcome of a bundle submitted via `eth_sendBundle`.
    ///
    /// Exploits the builder RPC's drop-on-miss semantics: a bundle is
    /// either included in the `target_block` it was submitted for, or
    /// dropped entirely (never retained for later blocks). So the
    /// outcome is determined the moment `target_block` is produced on L1.
    /// This yields:
    ///   - ≤ one L1 slot of wait on drops (vs. the 30–120s receipt-poll
    ///     window used for arbitrary mempool submissions),
    ///   - zero "silent confirm after poll timeout" drift — the tx
    ///     either shows up in the target block or is known-dropped
    ///     instantly.
    ///
    /// Errors and their meanings for the driver:
    ///   - `Err` with "reverted" in message → tx landed but `_verifyProof`
    ///     (or similar) reverted on-chain. Caller rewinds to anchor and
    ///     re-derives.
    ///   - `Err` with "dropped" in message → bundle didn't land; nonce
    ///     NOT consumed. Caller re-queues blocks and retries next tick.
    ///     (Falls into `flush.rs`'s "timeout" branch, which already
    ///     handles this correctly: re-queue + reset_nonce_unsolicited.)
    async fn wait_for_bundle_outcome(&self, tx_hash: B256, target_block: u64) -> Result<u64> {
        const SLOT_MS: u64 = 500;
        const MAX_WAIT_SECS: u64 = 15; // generous vs. 5s Chiado slot — tolerates one missed slot
        let deadline = std::time::Instant::now() + Duration::from_secs(MAX_WAIT_SECS);

        // Step 1: wait for target_block to exist on L1.
        loop {
            let latest = match self.provider.get_block_number().await {
                Ok(n) => n,
                Err(err) => {
                    warn!(target: "based_rollup::proposer", %err, "RPC failure while waiting for target L1 block, retrying");
                    tokio::time::sleep(Duration::from_millis(SLOT_MS)).await;
                    if std::time::Instant::now() >= deadline {
                        return Err(eyre::eyre!(
                            "bundle outcome wait: RPC unavailable past deadline (target_block={target_block}, tx_hash={tx_hash})"
                        ));
                    }
                    continue;
                }
            };
            if latest >= target_block {
                break;
            }
            tokio::time::sleep(Duration::from_millis(SLOT_MS)).await;
            if std::time::Instant::now() >= deadline {
                return Err(eyre::eyre!(
                    "bundle outcome wait: target_block {target_block} not produced within {MAX_WAIT_SECS}s (latest={latest}, tx_hash={tx_hash}) — bundle dropped"
                ));
            }
        }

        // Step 2: fetch target block and check if our tx is in it.
        let block = self
            .provider
            .get_block_by_number(target_block.into())
            .full()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch target L1 block {target_block}: {e}"))?
            .ok_or_else(|| eyre::eyre!("target L1 block {target_block} unexpectedly missing"))?;

        let included = block.transactions.hashes().any(|h| h == tx_hash);

        if !included {
            return Err(eyre::eyre!(
                "postBatch bundle dropped (tx_hash={tx_hash} not in target_block={target_block})"
            ));
        }

        // Step 3: landed — fetch receipt to distinguish success vs revert.
        let receipt = self
            .provider
            .get_transaction_receipt(tx_hash)
            .await
            .map_err(|e| eyre::eyre!("failed to fetch receipt for included tx {tx_hash}: {e}"))?
            .ok_or_else(|| eyre::eyre!(
                "bundle tx {tx_hash} is in block {target_block} but receipt is missing (chain reorg?)"
            ))?;

        if receipt.status() {
            info!(
                target: "based_rollup::proposer",
                %tx_hash,
                l1_block_number = target_block,
                "postBatch confirmed on L1"
            );
            Ok(target_block)
        } else {
            Err(eyre::eyre!(
                "postBatch reverted on L1 (tx_hash={tx_hash}, block={target_block})"
            ))
        }
    }

    /// Wait for an L1 transaction to be mined and confirmed.
    /// Called after [`send_to_l1`] and after forwarding queued L1 txs.
    ///
    /// Branches on submission mode:
    ///   - If the most recent submission used `send_via_bundle`, uses
    ///     `wait_for_bundle_outcome` (drop-on-miss semantics, ≤ one L1 slot).
    ///   - Otherwise (eth_sendRawTransaction via alloy send_transaction),
    ///     falls through to the legacy time-bounded receipt poll.
    pub async fn wait_for_l1_receipt(&self, tx_hash: B256) -> Result<u64> {
        let target = self
            .last_bundle_target
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        if target > 0 {
            self.wait_for_bundle_outcome(tx_hash, target).await
        } else {
            let (_tx_hash, l1_block) = self.wait_for_receipt(tx_hash, "postBatch").await?;
            Ok(l1_block)
        }
    }

    /// Convenience wrapper: send postBatch and wait for confirmation in one call.
    /// Used by tests and code paths that don't need to interleave L1 tx forwarding.
    pub async fn submit_to_l1(
        &self,
        blocks: &[PendingBlock],
        cross_chain_entries: &[CrossChainExecutionEntry],
    ) -> Result<u64> {
        if blocks.is_empty() && cross_chain_entries.is_empty() {
            return Ok(0);
        }
        let tx_hash = self.send_to_l1(blocks, cross_chain_entries, None).await?;
        self.wait_for_l1_receipt(tx_hash).await
    }

    /// Send an arbitrary L1 transaction with an explicit nonce.
    ///
    /// Uses a manually-specified nonce instead of the auto-nonce filler. This
    /// prevents nonce desynchronization when the tx fails during gas estimation
    /// (alloy's `CachedNonceManager` increments its cache even on failure,
    /// creating a permanent nonce gap that blocks all subsequent transactions).
    ///
    /// Callers must obtain the nonce via [`get_l1_nonce`] before calling this.
    ///
    /// ## Invariant #2 closure
    ///
    /// On failure, returns [`NonceSendError`] carrying a
    /// [`NonceResetRequired`] `#[must_use]` token. The only way to
    /// consume the token is via [`Proposer::reset_nonce`]. Ignoring
    /// it is a clippy warning promoted to an error by `-D warnings`,
    /// which makes it **impossible** to add a new L1 send path that
    /// forgets to reset the nonce after failure.
    pub async fn send_l1_tx_with_nonce(
        &self,
        to: Address,
        input: Bytes,
        value: U256,
        nonce: u64,
        gas_limit: u64,
    ) -> Result<B256, NonceSendError> {
        // Setting nonce explicitly causes NonceFiller to skip (FillerControlFlow::Finished).
        // Setting gas explicitly avoids gas estimation failures that corrupt nonce state.
        let tx = alloy_rpc_types::TransactionRequest::default()
            .to(to)
            .input(input.into())
            .value(value)
            .nonce(nonce)
            .gas_limit(gas_limit);

        let pending = match self.provider.send_transaction(tx).await {
            Ok(p) => p,
            Err(err) => {
                warn!(target: "based_rollup::proposer", %err, nonce, "failed to send L1 tx");
                return Err(NonceSendError {
                    reset_required: NonceResetRequired { _seal: () },
                    source: eyre::eyre!(err),
                });
            }
        };

        let tx_hash = *pending.tx_hash();
        info!(
            target: "based_rollup::proposer",
            %tx_hash,
            %to,
            nonce,
            "sent L1 tx with explicit nonce"
        );
        Ok(tx_hash)
    }

    /// Query the pending nonce for the builder's address on L1.
    ///
    /// Returns the next nonce to use (includes pending txs in the mempool).
    pub async fn get_l1_nonce(&self) -> Result<u64> {
        let nonce = self
            .provider
            .get_transaction_count(self.signer_address)
            .pending()
            .await
            .map_err(|err| eyre::eyre!("failed to fetch L1 nonce: {err}"))?;
        Ok(nonce)
    }

    /// Reset the provider to clear cached nonce state.
    ///
    /// Called after L1 tx failures to resynchronize alloy's `CachedNonceManager`
    /// with the actual on-chain nonce. Without this, a failed `send_transaction`
    /// leaves the cached nonce incremented past the on-chain nonce, causing all
    /// subsequent transactions to use wrong nonces (stuck in "queued" pool).
    ///
    /// Takes a [`NonceResetRequired`] token by value, consuming it.
    /// This is the **only** way to discharge the `#[must_use]`
    /// obligation that [`Proposer::send_l1_tx_with_nonce`] puts on
    /// the caller after a failed L1 send. See invariant #2.
    ///
    /// There is also an "unsolicited" path where the driver calls
    /// `reset_nonce` after a *successful* sequence of sends to
    /// refresh the alloy cache for the next postBatch (no token to
    /// consume because no send failed). That caller uses
    /// [`Proposer::reset_nonce_unsolicited`].
    pub fn reset_nonce(&mut self, _token: NonceResetRequired) -> Result<()> {
        self.reset_provider_cache()
    }

    /// Reset the provider cache without a failure token. Used after
    /// a successful sequence of L1 sends (e.g., after all `executeL2TX`
    /// triggers landed) to refresh the alloy `CachedNonceManager` for
    /// the next batch.
    ///
    /// Kept as a separate method from [`Proposer::reset_nonce`] so
    /// the failure path and the post-success housekeeping path are
    /// visibly distinct at call sites. Both delegate to the shared
    /// [`Proposer::reset_provider_cache`] implementation below.
    pub fn reset_nonce_unsolicited(&mut self) -> Result<()> {
        self.reset_provider_cache()
    }

    /// Shared implementation — tear down and rebuild the wallet
    /// provider so alloy's `CachedNonceManager` is dropped.
    fn reset_provider_cache(&mut self) -> Result<()> {
        let wallet = EthereumWallet::from(self.signer.clone());

        let provider = ProviderBuilder::new().wallet(wallet).connect_http(
            self.config
                .l1_rpc_url
                .parse()
                .map_err(|e| eyre::eyre!("invalid URL: {e}"))?,
        );

        self.provider = Box::new(provider);
        info!(
            target: "based_rollup::proposer",
            "reset L1 provider to clear cached nonce state"
        );
        Ok(())
    }

    /// Check if an address has code deployed on L1.
    pub async fn has_code_at(&self, address: Address) -> bool {
        match self.provider.get_code_at(address).await {
            Ok(code) => !code.is_empty(),
            Err(_) => false, // Assume no code on error (will try createProxy)
        }
    }

    /// Return the signer address (used for proof generation and balance checks).
    pub fn signer_address(&self) -> Address {
        self.signer_address
    }

    /// Return the signer for external use (e.g., proof generation).
    pub fn create_signer(&self) -> Result<PrivateKeySigner> {
        Ok(self.signer.clone())
    }

    /// Rebuild the internal provider to point at a different L1 RPC URL.
    ///
    /// Called by the driver when switching between primary and fallback L1
    /// providers, so the proposer's submissions follow the same failover
    /// logic as the rest of the driver.
    pub fn switch_l1_url(&mut self, new_url: &str) -> Result<()> {
        let wallet = EthereumWallet::from(self.signer.clone());

        let provider = ProviderBuilder::new().wallet(wallet).connect_http(
            new_url
                .parse()
                .map_err(|e| eyre::eyre!("invalid URL: {e}"))?,
        );

        self.provider = Box::new(provider);
        info!(
            target: "based_rollup::proposer",
            new_url,
            "proposer switched L1 RPC endpoint"
        );
        Ok(())
    }
}

/// Visible for testing — construct a Proposer with an arbitrary provider.
#[cfg(test)]
#[path = "proposer_tests.rs"]
mod tests;
