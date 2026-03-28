// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IERC20 {
    function transferFrom(address, address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
}

interface IBridge {
    function bridgeTokens(address token, uint256 amount, uint256 rollupId, address dest) external;
}

/// @notice Minimal wrapper: takes tokens from user, bridges them. Nothing else.
///         Used to verify that bridge-via-contract works.
contract BridgeWrapper {
    function wrapAndBridge(address token, uint256 amount, address bridge, uint256 rollupId, address dest) external {
        IERC20(token).transferFrom(msg.sender, address(this), amount);
        IERC20(token).approve(bridge, amount);
        IBridge(bridge).bridgeTokens(token, amount, rollupId, dest);
    }
}
