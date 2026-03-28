// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IERC20 {
    function transferFrom(address, address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
}

interface IBridge {
    function bridgeTokens(address token, uint256 amount, uint256 rollupId, address dest) external;
}

/// @notice Bridges tokens AND calls a cross-chain proxy in one tx.
///         Tests two L1→L2 cross-chain calls from an intermediate contract.
contract BridgeThenCall {
    function bridgeThenCallProxy(
        address token,
        uint256 bridgeAmount,
        address bridge,
        uint256 rollupId,
        address bridgeDest,
        address proxy,
        bytes calldata proxyCalldata
    ) external {
        IERC20(token).transferFrom(msg.sender, address(this), bridgeAmount);
        IERC20(token).approve(bridge, bridgeAmount);
        IBridge(bridge).bridgeTokens(token, bridgeAmount, rollupId, bridgeDest);

        (bool success, ) = proxy.call(proxyCalldata);
        require(success, "proxy call failed");
    }
}
