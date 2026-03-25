## Summary

<!-- Brief description of what this PR does and why -->

## Changes

-

## Related Issues

<!-- Link issues this PR addresses: Closes #123, Fixes #456 -->

## Testing

- [ ] Unit tests pass (`cargo test --workspace --lib`)
- [ ] E2E tests pass (`cargo test --workspace --test e2e_anvil`)
- [ ] Clippy clean (`cargo clippy --workspace --all-features`)
- [ ] Formatted (`cargo +nightly fmt --all --check`)

## Review Checklist

- [ ] No `unwrap()` in production code without justification
- [ ] No unchecked arithmetic on untrusted inputs
- [ ] Error paths logged appropriately
- [ ] No hardcoded secrets or keys
