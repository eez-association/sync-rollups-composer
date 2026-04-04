// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title Counter — deployed on L1, increments and returns new value.
contract Counter {
    uint256 public counter;

    function increment() external returns (uint256) {
        counter++;
        return counter;
    }
}
