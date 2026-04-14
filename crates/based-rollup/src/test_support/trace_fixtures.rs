//! Loaders for the canonical `callTracer` trace fixtures committed under
//! `crates/based-rollup/tests/fixtures/traces/`.
//!
//! Refactor PLAN step 0.6 produces these fixtures and this module exposes
//! them to sibling unit tests under `crates/based-rollup/src/`. Files are
//! embedded at compile time via [`include_str!`] so tests neither require
//! filesystem access nor pay the overhead of reading them at runtime.
//!
//! ## Usage
//!
//! ```ignore
//! use crate::test_support::trace_fixtures::{all_fixtures, FixtureName};
//!
//! for fx in all_fixtures() {
//!     let trace = fx.parse_value();   // serde_json::Value
//!     // ...assert against `trace`
//! }
//! ```
//!
//! ## Adding a fixture
//!
//! 1. Drop a new `.json` file in `tests/fixtures/traces/`.
//! 2. Add a new variant to [`FixtureName`].
//! 3. Add a corresponding entry to [`ALL_FIXTURES`] with the matching
//!    `include_str!` path.
//! 4. Update `tests/fixtures/traces/README.md` so the table stays in sync.

use serde_json::Value;

/// Identifier for each canonical trace fixture. Variants map 1:1 to JSON
/// files in `tests/fixtures/traces/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixtureName {
    DepositSimpleL1ToL2,
    WithdrawalSimpleL2ToL1,
    FlashLoan3CallL1ToL2,
    PingPongDepth2L2ToL1,
    PingPongDepth3L2ToL1,
    TopLevelRevert,
    ChildContinuation,
    MultiCallCallTwice,
}

/// A canonical trace fixture: an identifier + the embedded JSON contents.
#[derive(Debug, Clone, Copy)]
pub struct Fixture {
    pub name: FixtureName,
    /// Human-readable filename without the path prefix, e.g.
    /// `"deposit_simple_l1_to_l2.json"`. Used in test diagnostics.
    pub filename: &'static str,
    /// Embedded JSON contents (compile-time `include_str!`).
    pub contents: &'static str,
}

impl Fixture {
    /// Parse the embedded contents into a `serde_json::Value`. Panics on
    /// malformed JSON because every fixture is committed to the repo and
    /// validated by the round-trip tests.
    pub fn parse_value(&self) -> Value {
        serde_json::from_str(self.contents)
            .unwrap_or_else(|e| panic!("fixture {} is not valid JSON: {}", self.filename, e))
    }
}

/// All 8 canonical fixtures, in stable order.
///
/// The order is documented in `tests/fixtures/traces/README.md`. Tests
/// that loop over this list automatically pick up new fixtures added
/// here without code changes.
pub const ALL_FIXTURES: &[Fixture] = &[
    Fixture {
        name: FixtureName::DepositSimpleL1ToL2,
        filename: "deposit_simple_l1_to_l2.json",
        contents: include_str!("../../tests/fixtures/traces/deposit_simple_l1_to_l2.json"),
    },
    Fixture {
        name: FixtureName::WithdrawalSimpleL2ToL1,
        filename: "withdrawal_simple_l2_to_l1.json",
        contents: include_str!("../../tests/fixtures/traces/withdrawal_simple_l2_to_l1.json"),
    },
    Fixture {
        name: FixtureName::FlashLoan3CallL1ToL2,
        filename: "flash_loan_3_call_l1_to_l2.json",
        contents: include_str!("../../tests/fixtures/traces/flash_loan_3_call_l1_to_l2.json"),
    },
    Fixture {
        name: FixtureName::PingPongDepth2L2ToL1,
        filename: "ping_pong_depth_2_l2_to_l1.json",
        contents: include_str!("../../tests/fixtures/traces/ping_pong_depth_2_l2_to_l1.json"),
    },
    Fixture {
        name: FixtureName::PingPongDepth3L2ToL1,
        filename: "ping_pong_depth_3_l2_to_l1.json",
        contents: include_str!("../../tests/fixtures/traces/ping_pong_depth_3_l2_to_l1.json"),
    },
    Fixture {
        name: FixtureName::TopLevelRevert,
        filename: "top_level_revert.json",
        contents: include_str!("../../tests/fixtures/traces/top_level_revert.json"),
    },
    Fixture {
        name: FixtureName::ChildContinuation,
        filename: "child_continuation.json",
        contents: include_str!("../../tests/fixtures/traces/child_continuation.json"),
    },
    Fixture {
        name: FixtureName::MultiCallCallTwice,
        filename: "multi_call_call_twice.json",
        contents: include_str!("../../tests/fixtures/traces/multi_call_call_twice.json"),
    },
];

/// Iterator-style accessor over [`ALL_FIXTURES`].
pub fn all_fixtures() -> impl Iterator<Item = &'static Fixture> {
    ALL_FIXTURES.iter()
}

/// Lookup a fixture by name. Always returns `Some` because every
/// `FixtureName` variant is wired into [`ALL_FIXTURES`].
pub fn get(name: FixtureName) -> Option<&'static Fixture> {
    ALL_FIXTURES.iter().find(|f| f.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fixture parses to valid JSON whose top-level shape matches
    /// the `callTracer` contract.
    #[test]
    fn all_fixtures_parse_to_valid_call_tracer_shape() {
        let mut count = 0usize;
        for fx in all_fixtures() {
            let v = fx.parse_value();
            // Top-level must be an object with the canonical fields.
            assert!(
                v.is_object(),
                "fixture {} top-level is not an object",
                fx.filename
            );
            for required in &["from", "to", "input", "value", "calls"] {
                assert!(
                    v.get(*required).is_some(),
                    "fixture {} missing required field `{}`",
                    fx.filename,
                    required
                );
            }
            // `calls` must be an array.
            assert!(
                v.get("calls").unwrap().is_array(),
                "fixture {} `calls` is not an array",
                fx.filename
            );
            count += 1;
        }
        assert_eq!(count, 8, "expected 8 canonical fixtures");
    }

    /// Every `FixtureName` variant has a corresponding entry in
    /// [`ALL_FIXTURES`]. Catches enum-add / array-forget mistakes.
    #[test]
    fn every_fixture_name_is_wired_in_all_fixtures() {
        let names = [
            FixtureName::DepositSimpleL1ToL2,
            FixtureName::WithdrawalSimpleL2ToL1,
            FixtureName::FlashLoan3CallL1ToL2,
            FixtureName::PingPongDepth2L2ToL1,
            FixtureName::PingPongDepth3L2ToL1,
            FixtureName::TopLevelRevert,
            FixtureName::ChildContinuation,
            FixtureName::MultiCallCallTwice,
        ];
        for name in names {
            assert!(
                get(name).is_some(),
                "fixture {:?} not in ALL_FIXTURES",
                name
            );
        }
    }

    /// All filenames are unique.
    #[test]
    fn fixture_filenames_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for fx in all_fixtures() {
            assert!(
                seen.insert(fx.filename),
                "duplicate filename: {}",
                fx.filename
            );
        }
    }

    /// The trace fixtures with cross-chain frames embed the canonical
    /// `executeCrossChainCall(address,bytes)` selector `0x9af53259`
    /// somewhere in the tree. The two fixtures that intentionally
    /// don't include a cross-chain hop don't trip this check (we don't
    /// have any such fixture today, but the loop is permissive).
    #[test]
    fn fixtures_with_cross_chain_frames_use_canonical_selector() {
        const SELECTOR_HEX: &str = "9af53259";
        for fx in all_fixtures() {
            // The selector substring must appear at least once anywhere
            // in the JSON contents (top level OR nested).
            assert!(
                fx.contents.contains(SELECTOR_HEX),
                "fixture {} does not contain the canonical selector {}",
                fx.filename,
                SELECTOR_HEX
            );
        }
    }
}
