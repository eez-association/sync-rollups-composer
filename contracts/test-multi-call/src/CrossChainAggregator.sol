// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function approve(address, uint256) external returns (bool);
    function transfer(address, uint256) external returns (bool);
    function transferFrom(address, address, uint256) external returns (bool);
}

interface IAMM {
    function swap(address tokenIn, uint256 amountIn) external returns (uint256);
}

interface IBridge {
    function bridgeTokens(address token, uint256 amount, uint256 rollupId, address dest) external;
}

interface IL2Executor {
    function swapAndBridgeBack(address tokenIn, address recipient) external returns (uint256);
}

/// @title CrossChainAggregator
/// @notice Splits a swap across L1 and L2 AMM pools.
///         Deployed on L1. In one transaction:
///         1. Swap localAmount on L1 AMM
///         2. Bridge remoteAmount to L2 Executor
///         3. Call L2 Executor via proxy to swap and bridge back
///         4. Return combined output to user
contract CrossChainAggregator {
    address public localAMM;
    address public bridge;
    address public tokenA;
    address public tokenB;
    uint256 public remoteRollupId;
    address public l2Executor;
    address public l2ExecutorProxy;
    address public owner;

    constructor(address _localAMM, address _bridge, address _tokenA, address _tokenB, uint256 _remoteRollupId) {
        localAMM = _localAMM;
        bridge = _bridge;
        tokenA = _tokenA;
        tokenB = _tokenB;
        remoteRollupId = _remoteRollupId;
        owner = msg.sender;
    }

    function setL2Executor(address _executor, address _proxy) external {
        require(msg.sender == owner, "only owner");
        l2Executor = _executor;
        l2ExecutorProxy = _proxy;
    }

    function aggregatedSwap(address tokenIn, uint256 totalAmount, uint256 localAmount) external returns (uint256 totalOut) {
        require(localAmount <= totalAmount, "local > total");
        require(tokenIn == tokenA || tokenIn == tokenB, "invalid token");
        uint256 remoteAmount = totalAmount - localAmount;

        IERC20(tokenIn).transferFrom(msg.sender, address(this), totalAmount);
        address tokenOut = tokenIn == tokenA ? tokenB : tokenA;

        uint256 localOut = 0;
        if (localAmount > 0) {
            IERC20(tokenIn).approve(localAMM, localAmount);
            localOut = IAMM(localAMM).swap(tokenIn, localAmount);
        }

        uint256 remoteOut = 0;
        if (remoteAmount > 0) {
            IERC20(tokenIn).approve(bridge, remoteAmount);
            IBridge(bridge).bridgeTokens(tokenIn, remoteAmount, remoteRollupId, l2Executor);

            uint256 outBefore = IERC20(tokenOut).balanceOf(address(this));
            (bool success, bytes memory result) = l2ExecutorProxy.call(
                abi.encodeCall(IL2Executor.swapAndBridgeBack, (tokenIn, address(this)))
            );
            require(success, "remote swap failed");
            remoteOut = IERC20(tokenOut).balanceOf(address(this)) - outBefore - localOut;
        }

        totalOut = localOut + remoteOut;
        require(totalOut > 0, "zero output");
        IERC20(tokenOut).transfer(msg.sender, totalOut);
    }
}
