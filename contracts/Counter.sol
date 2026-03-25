// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title SimpleCounter — demo contract for cross-chain composability
/// @notice Deployed on L2 by the crosschain-tx-sender script
contract Counter {
    uint256 public count;

    function getCount() external view returns (uint256) {
        return count;
    }

    function increment() external {
        count += 1;
    }
}
