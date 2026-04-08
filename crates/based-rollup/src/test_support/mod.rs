//! Test support utilities for the based-rollup crate.
//!
//! This module exists to host test-only DSL and helpers that need to be
//! reachable from sibling unit tests under `crates/based-rollup/src/`,
//! which cannot import code from `crates/based-rollup/tests/` (those are
//! integration tests with their own crate roots).
//!
//! The module is gated behind `#[cfg(any(test, feature = "test-utils"))]`
//! so it does NOT contribute to release builds and is only visible to
//! unit tests and to dependent crates that opt in via the `test-utils`
//! feature.
//!
//! Members:
//! - [`mirror_case`] — neutral DSL for L1↔L2 mirror tests (refactor PLAN
//!   step 0.5; closes invariant #18 of `docs/refactor/INVARIANT_MAP.md`).
//! - [`trace_fixtures`] — embedded canonical `callTracer` JSON fixtures
//!   (refactor PLAN step 0.6) used by composer_rpc tests today and by
//!   Phase 5 fuzz harnesses later.

pub mod mirror_case;
pub mod trace_fixtures;
