#!/usr/bin/env bash
#
# Invariant #23 CI gate — forbid hardcoded Solidity function selectors in
# production Rust code.
#
# Per CLAUDE.md "Lessons Learned — Protocol-First Implementation":
#
#   > NEVER hardcode function selectors. Use typed ABI encoding via `sol!`
#   > macros and `SolCall::abi_encode()`. Hardcoded selectors cause silent
#   > failures and cannot be verified at compile time.
#
# This script scans production Rust files under crates/based-rollup/src/ for
# string literals that look like 4-byte selectors (`"0xdeadbeef"`) or
# uncommented `0x[a-f0-9]{8}` tokens, excluding:
#
# - Lines inside // comments (the selectors documented in comments are fine).
# - Test files (`*_tests.rs`) and test helpers (`test_support/`).
# - The allow-list placeholders used by trace JSON fixtures
#   (`"0xdeadbeef"`, `"0xaabbccdd"`, `"0x11223344"`, `"0x11111111"`,
#   `"0x22222222"`).
# - Zero-fill hex for padded bytes (`0x00000000`, `0x0000000000000000`, etc.).
#
# Exits 1 and prints offending lines if any match is found.

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

# Production files only — everything under crates/based-rollup/src/
# MINUS tests, test_support, and mod test blocks.
mapfile -t FILES < <(
  find crates/based-rollup/src -name '*.rs' \
    -not -name '*_tests.rs' \
    -not -path '*/test_support/*'
)

if [ ${#FILES[@]} -eq 0 ]; then
  echo "no production rust files found" >&2
  exit 1
fi

# grep for 8-hex-char tokens; ignore lines that are // comments or inside
# doc comments; the RE tries to match both `"0x...."` strings and bare
# `0x....` tokens.
violations=$(
  grep -n --extended-regexp \
    '(^|[^/]*)0x[a-f0-9]{8}([^a-f0-9]|$)' \
    "${FILES[@]}" 2>/dev/null \
  | grep -v -E '^[^:]+:[0-9]+:[[:space:]]*//' \
  | grep -v -E '0x0{8,}' \
  | grep -v -E '"0x(deadbeef|aabbccdd|11223344|11111111|22222222)"' \
  | grep -v -E '// .*0x[a-f0-9]{8}' \
  || true
)

if [ -n "$violations" ]; then
  echo "ERROR: hardcoded selectors detected in production Rust code:" >&2
  echo "" >&2
  echo "$violations" >&2
  echo "" >&2
  echo "Per invariant #23 / CLAUDE.md: use typed ABI encoding via the sol! macro." >&2
  echo "If the match is a false positive, extend the allow-list in" >&2
  echo "scripts/refactor/check-no-hardcoded-selectors.sh." >&2
  exit 1
fi

echo "no hardcoded selectors detected in production Rust code"
