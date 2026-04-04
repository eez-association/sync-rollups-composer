// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title SimpleStorage — deployed on L2, stores a uint256 value.
contract SimpleStorage {
    uint256 public value;

    function store(uint256 v) external {
        value = v;
    }
}
