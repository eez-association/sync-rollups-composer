//! L1 block submission via the Rollups contract.
//!
//! The proposer posts sealed L2 blocks to L1 by calling `postBatch()` with
//! execution entries (immediate block entries + optional cross-chain deferred
//! entries), block calldata, and a proof.

use crate::config::RollupConfig;
use crate::cross_chain::{CleanStateRoot, CrossChainExecutionEntry};
use alloy_network::EthereumWallet;
use alloy_primitives::Address;
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
    /// Type-erased provider with wallet fillers.
    provider: Box<dyn Provider + Send + Sync>,
    /// The signer for ECDSA proof generation.
    signer: PrivateKeySigner,
    /// The signer address (for balance checks).
    signer_address: Address,
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

        Ok(Self {
            config,
            provider: Box::new(provider),
            signer,
            signer_address,
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
    async fn build_proof_context(
        &self,
        entries: &[CrossChainExecutionEntry],
    ) -> Result<ProofContext> {
        let vk = self.verification_key().await?;
        let (latest_number, latest_hash, latest_timestamp) = self.latest_l1_block().await?;
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
    /// blocks (5s). The previous value of 7 (14s) was too tight for real
    /// 5-second Gnosis/Chiado block times under any RPC latency.
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

    /// Maximum estimated calldata gas per batch. Set conservatively below
    /// typical L1 block gas limits (~30M) to leave room for intrinsic gas.
    const MAX_CALLDATA_GAS: u64 = 12_000_000;

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
        let proof_ctx = self.build_proof_context(&all_entries).await?;
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
            .input(calldata.into());

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

        let pending = self.provider.send_transaction(tx).await.map_err(|err| {
            warn!(target: "based_rollup::proposer", %err, "failed to submit to L1");
            eyre::eyre!(err)
        })?;

        let tx_hash = *pending.tx_hash();
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

    /// Wait for an L1 transaction to be mined and confirmed.
    /// Called after [`send_to_l1`] and after forwarding queued L1 txs.
    pub async fn wait_for_l1_receipt(&self, tx_hash: B256) -> Result<u64> {
        let (_tx_hash, l1_block) = self.wait_for_receipt(tx_hash, "postBatch").await?;
        Ok(l1_block)
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
