// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

interface ICounter {
    function increment() external returns (uint256);
}

interface ISimpleStorage {
    function store(uint256 v) external;
}

/// @title Orchestrator — deployed on L2. Calls L1 Counter proxy, stores
///        result in SimpleStorage, reverts if result is even.
contract Orchestrator {
    function executeAndStore(address counterProxy, address storage_) external returns (uint256) {
        uint256 result = ICounter(counterProxy).increment();
        ISimpleStorage(storage_).store(result);
        require(result % 2 != 0, "result is even, reverting");
        return result;
    }
}
