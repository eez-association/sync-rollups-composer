// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title DualCaller
/// @notice Deployed on L1. Makes two calls:
///         1. Reads from L2 SimpleStorage via proxy (cross-chain L1→L2)
///         2. Increments L1 Counter (same-chain)
///
///         This tests mixing cross-chain and same-chain calls in one execution.
contract DualCaller {
    function readAndIncrement(
        address l2StorageProxy,
        address l1Counter
    ) external returns (uint256 storedValue, uint256 counterValue) {
        // 1. Read from L2 Storage via proxy (cross-chain L1→L2)
        (bool ok1, bytes memory ret1) = l2StorageProxy.call(
            abi.encodeWithSignature("value()")
        );
        require(ok1, "read from L2 storage failed");
        storedValue = abi.decode(ret1, (uint256));

        // 2. Increment L1 Counter (same-chain, no proxy)
        (bool ok2, bytes memory ret2) = l1Counter.call(
            abi.encodeWithSignature("increment()")
        );
        require(ok2, "L1 counter increment failed");
        counterValue = abi.decode(ret2, (uint256));
    }
}
