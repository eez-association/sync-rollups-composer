//! Composer RPC — intercepts `eth_sendRawTransaction` to detect and prepare
//! cross-chain calls before forwarding the user's transaction.
//!
//! Two direction-specific modules:
//! - `l1_to_l2`: L1 RPC proxy (intercepts L1 txs targeting L2)
//! - `l2_to_l1`: L2 RPC proxy (intercepts L2 txs targeting L1)
//!
//! Shared utilities in `common` and generic trace-based detection in `trace`.

pub mod common;
pub mod l1_to_l2;
pub mod l2_to_l1;
pub mod trace;
