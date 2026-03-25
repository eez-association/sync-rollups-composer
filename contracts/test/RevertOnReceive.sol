// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @notice Deployed on L1 at a deterministic CREATE address.
/// Reverts when receiving ETH, causing the withdrawal trigger
/// (proxy.executeOnBehalf{value:X}(addr, "")) to fail.
contract RevertOnReceive {
    receive() external payable {
        revert("no ETH accepted");
    }

    fallback() external payable {
        revert("no ETH accepted");
    }
}
