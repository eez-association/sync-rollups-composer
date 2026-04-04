// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

interface ISimpleStorage {
    function value() external view returns (uint256);
}

interface ICounter {
    function increment() external returns (uint256);
}

/// @title DualCaller — deployed on L1. Reads L2 Storage proxy and
///        increments L1 Counter in one transaction.
contract DualCaller {
    function readAndIncrement(address storageProxy, address counter) external returns (uint256 storageVal, uint256 counterVal) {
        storageVal = ISimpleStorage(storageProxy).value();
        counterVal = ICounter(counter).increment();
    }
}
