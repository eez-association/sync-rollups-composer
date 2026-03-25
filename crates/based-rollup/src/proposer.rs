//! L1 block submission via the Rollups contract.
//!
//! The proposer posts sealed L2 blocks to L1 by calling `postBatch()` with
//! execution entries (immediate block entries + optional cross-chain deferred
//! entries), block calldata, and a proof.

use crate::config::RollupConfig;
use crate::cross_chain::CrossChainExecutionEntry;
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
    pub clean_state_root: B256,
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
    async fn sign_proof(
        &self,
        entries: &[CrossChainExecutionEntry],
        call_data: &Bytes,
    ) -> Result<Bytes> {
        let vk = self.verification_key().await?;
        let (latest_number, latest_hash, latest_timestamp) = self.latest_l1_block().await?;
        let target_block = latest_number + 1;

        // Predict the next block's timestamp.
        // On L1 with fixed block time (e.g., reth --dev.block-time=12s):
        //   next_ts = latest_ts + block_time
        // On L1 without fixed block time:
        //   next_ts = max(latest_ts + 1, now)
        // Use the L1 slot time from config (defaults to 12s).
        let predicted_timestamp = latest_timestamp + self.config.block_time;

        let entry_hashes = crate::cross_chain::compute_entry_hashes(entries, vk);
        let public_inputs_hash = crate::cross_chain::compute_public_inputs_hash(
            &entry_hashes,
            call_data,
            latest_hash,
            predicted_timestamp,
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
            target_block,
            parent_hash = %latest_hash,
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
                last_post,
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
        let proof = self.sign_proof(&all_entries, &call_data).await?;

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
    pub async fn send_l1_tx_with_nonce(
        &self,
        to: Address,
        input: Bytes,
        value: U256,
        nonce: u64,
        gas_limit: u64,
    ) -> Result<B256> {
        // Setting nonce explicitly causes NonceFiller to skip (FillerControlFlow::Finished).
        // Setting gas explicitly avoids gas estimation failures that corrupt nonce state.
        let tx = alloy_rpc_types::TransactionRequest::default()
            .to(to)
            .input(input.into())
            .value(value)
            .nonce(nonce)
            .gas_limit(gas_limit);

        let pending = self.provider.send_transaction(tx).await.map_err(|err| {
            warn!(target: "based_rollup::proposer", %err, nonce, "failed to send L1 tx");
            eyre::eyre!(err)
        })?;

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
    pub fn reset_nonce(&mut self) -> Result<()> {
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
