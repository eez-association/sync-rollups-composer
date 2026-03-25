// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface ISimpleStorage {
    function store(uint256 v) external;
}

/// @title Orchestrator
/// @notice Deployed on L2. Calls an L1 Counter proxy (cross-chain L2→L1),
///         stores the result in L2 SimpleStorage, then conditionally reverts
///         if the result is even.
///
///         When the revert triggers, both the L1 counter increment AND the
///         L2 storage write should be rolled back (cross-chain atomicity).
contract Orchestrator {
    function executeAndStore(
        address l1CounterProxy,
        address storageContract
    ) external returns (uint256 result) {
        // Step 1: Call L1 Counter via proxy (cross-chain L2→L1)
        (bool ok, bytes memory ret) = l1CounterProxy.call(
            abi.encodeWithSignature("increment()")
        );
        require(ok, "L1 counter call failed");
        result = abi.decode(ret, (uint256));

        // Step 2: Store result in L2 SimpleStorage
        ISimpleStorage(storageContract).store(result);

        // Step 3: Conditionally revert if result is even
        require(result % 2 != 0, "result is even, reverting");
    }
}
