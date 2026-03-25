// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title ConditionalCallTwice
/// @notice Calls two different L2 counter proxies, then conditionally reverts
///         based on the second counter's return value.
///
///         This tests cross-chain atomicity: if the L1 execution reverts after
///         both cross-chain calls have been made, do the L2 state changes
///         (counter increments) also get rolled back?
///
///         Test cases:
///           - revertThreshold = 100: Counter B returns a small value (< 100),
///             no revert → both counters persist.
///           - revertThreshold = 1: Counter B returns >= 1 on first call,
///             triggers revert → both counters should roll back.
contract ConditionalCallTwice {
    function callBothConditional(
        address counterA,
        address counterB,
        uint256 revertThreshold
    ) external returns (uint256 a, uint256 b) {
        // Call counter A (cross-chain to L2)
        (bool ok1, bytes memory ret1) = counterA.call(
            abi.encodeWithSignature("increment()")
        );
        require(ok1, "first call failed");
        a = abi.decode(ret1, (uint256));

        // Call counter B (cross-chain to L2)
        (bool ok2, bytes memory ret2) = counterB.call(
            abi.encodeWithSignature("increment()")
        );
        require(ok2, "second call failed");
        b = abi.decode(ret2, (uint256));

        // Conditionally revert: if counter B's new value >= threshold, revert all
        require(b < revertThreshold, "conditional revert: counterB >= threshold");
    }
}
