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

    function swapAndBridgeBack(address tokenIn, address recipient) external returns (uint256 amountOut) {
        uint256 balance = IERC20(tokenIn).balanceOf(address(this));
        require(balance > 0, "no tokens to swap");

        IERC20(tokenIn).approve(amm, balance);
        amountOut = IAMM(amm).swap(tokenIn, balance);

        address tokenOut = tokenIn == tokenA ? tokenB : tokenA;
        IERC20(tokenOut).approve(bridge, amountOut);
        IBridge(bridge).bridgeTokens(tokenOut, amountOut, 0, recipient);
    }
}
