// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IBridge {
    function bridgeEther(uint256 rollupId, address recipient) external payable;
}

/// @notice Deployed on L2 at the same CREATE address as RevertOnReceive on L1.
/// Accepts ETH deposits and can initiate withdrawals via Bridge.bridgeEther(0, recipient).
contract WithdrawalSender {
    /// @notice Initiate a withdrawal by calling Bridge.bridgeEther(0, address(this))
    function triggerWithdrawal(address bridge, uint256 amount) external {
        IBridge(bridge).bridgeEther{value: amount}(0, address(this));
    }

    /// @notice Accept ETH deposits (needed to fund the contract on L2)
    receive() external payable {}
}
