// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function approve(address, uint256) external returns (bool);
    function transfer(address, uint256) external returns (bool);
}

interface IAMM {
    function swap(address tokenIn, uint256 amountIn) external returns (uint256);
}

interface IBridge {
    function bridgeTokens(address token, uint256 amount, uint256 rollupId, address dest) external;
}

/// @title L2Executor
/// @notice Deployed on L2. Receives bridged tokens, swaps on L2 AMM,
///         bridges the output back to L1.
///         Called via cross-chain proxy from L1.
contract L2Executor {
    address public amm;
    address public bridge;
    address public tokenA;
    address public tokenB;

    constructor(address _amm, address _bridge, address _tokenA, address _tokenB) {
        amm = _amm;
        bridge = _bridge;
        tokenA = _tokenA;
        tokenB = _tokenB;
    }

    /// @notice Swap and bridge back. The `l1TokenIn` parameter is the L1 address
    ///         of the token (passed from the L1 Aggregator via cross-chain call).
    ///         We map it to the local L2 wrapped token address.
    function swapAndBridgeBack(address l1TokenIn, address recipient) external returns (uint256 amountOut) {
        // Map L1 token address to L2 wrapped token address.
        // The bridge deposit already delivered wrapped tokens to this contract.
        // We don't use l1TokenIn directly — it has no code on L2.
        // Instead, check which of our known L2 tokens has a balance.
        address localTokenIn;
        if (IERC20(tokenA).balanceOf(address(this)) > 0) {
            localTokenIn = tokenA;
        } else if (IERC20(tokenB).balanceOf(address(this)) > 0) {
            localTokenIn = tokenB;
        } else {
            revert("no tokens to swap");
        }

        uint256 balance = IERC20(localTokenIn).balanceOf(address(this));

        IERC20(localTokenIn).approve(amm, balance);
        amountOut = IAMM(amm).swap(localTokenIn, balance);

        address localTokenOut = localTokenIn == tokenA ? tokenB : tokenA;
        IERC20(localTokenOut).approve(bridge, amountOut);
        IBridge(bridge).bridgeTokens(localTokenOut, amountOut, 0, recipient);
    }
}
