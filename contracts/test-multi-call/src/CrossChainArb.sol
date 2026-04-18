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

interface IL2Executor {
    function swapAndBridgeBack(address tokenIn, address recipient) external returns (uint256);
}

/// @title CrossChainArb
/// @notice Atomic cross-chain arbitrage between L1 and L2 AMM pools.
///         Holds its own working capital (WETH). Bot off-chain decides direction
///         and size. Each arb is atomic — reverts if profit < minProfit.
///
///         Two directions:
///         - arbSellOnL2: bridge WETH to L2, sell WETH→USDC on L2, bridge USDC back,
///                        buy WETH→USDC on L1. Profitable when L2 WETH price > L1 WETH price.
///         - arbSellOnL1: swap WETH→USDC on L1, bridge USDC to L2, L2 swaps USDC→WETH,
///                        bridge WETH back. Profitable when L1 WETH price > L2 WETH price.
contract CrossChainArb {
    address public weth;
    address public usdc;
    address public l1Amm;
    address public bridge;
    uint256 public rollupId;
    address public l2Executor;
    address public l2ExecutorProxy;
    address public owner;

    event ArbExecuted(uint8 direction, uint256 amountIn, uint256 profit);

    constructor(
        address _weth,
        address _usdc,
        address _l1Amm,
        address _bridge,
        uint256 _rollupId
    ) {
        weth = _weth;
        usdc = _usdc;
        l1Amm = _l1Amm;
        bridge = _bridge;
        rollupId = _rollupId;
        owner = msg.sender;
    }

    function setL2Executor(address _executor, address _proxy) external {
        require(msg.sender == owner, "only owner");
        l2Executor = _executor;
        l2ExecutorProxy = _proxy;
    }

    /// @notice Sell WETH on L2 (expensive), buy WETH on L1 (cheap).
    ///         Profitable when L2 price > L1 price.
    function arbSellOnL2(uint256 amountIn, uint256 minProfit) external returns (uint256 profit) {
        require(msg.sender == owner, "only owner");
        uint256 balBefore = IERC20(weth).balanceOf(address(this));
        require(balBefore >= amountIn, "insufficient WETH");

        // 1. Bridge WETH to L2 Executor
        IERC20(weth).approve(bridge, amountIn);
        IBridge(bridge).bridgeTokens(weth, amountIn, rollupId, l2Executor);

        // 2. Call L2 Executor via proxy: swaps WETH→USDC on L2, bridges USDC back here
        uint256 usdcBefore = IERC20(usdc).balanceOf(address(this));
        (bool ok,) = l2ExecutorProxy.call(
            abi.encodeCall(IL2Executor.swapAndBridgeBack, (weth, address(this)))
        );
        require(ok, "L2 executor call failed");
        uint256 usdcReceived = IERC20(usdc).balanceOf(address(this)) - usdcBefore;
        require(usdcReceived > 0, "no USDC received");

        // 3. Swap USDC → WETH on L1 AMM
        IERC20(usdc).approve(l1Amm, usdcReceived);
        IAMM(l1Amm).swap(usdc, usdcReceived);

        // 4. Profit check
        uint256 balAfter = IERC20(weth).balanceOf(address(this));
        require(balAfter >= balBefore + minProfit, "unprofitable");
        profit = balAfter - balBefore;
        emit ArbExecuted(0, amountIn, profit);
    }

    /// @notice Sell WETH on L1 (expensive), buy WETH on L2 (cheap).
    ///         Profitable when L1 price > L2 price.
    function arbSellOnL1(uint256 amountIn, uint256 minProfit) external returns (uint256 profit) {
        require(msg.sender == owner, "only owner");
        uint256 balBefore = IERC20(weth).balanceOf(address(this));
        require(balBefore >= amountIn, "insufficient WETH");

        // 1. Swap WETH → USDC on L1 AMM
        IERC20(weth).approve(l1Amm, amountIn);
        uint256 usdcOut = IAMM(l1Amm).swap(weth, amountIn);

        // 2. Bridge USDC to L2 Executor
        IERC20(usdc).approve(bridge, usdcOut);
        IBridge(bridge).bridgeTokens(usdc, usdcOut, rollupId, l2Executor);

        // 3. Call L2 Executor via proxy: swaps USDC→WETH on L2, bridges WETH back here
        (bool ok,) = l2ExecutorProxy.call(
            abi.encodeCall(IL2Executor.swapAndBridgeBack, (usdc, address(this)))
        );
        require(ok, "L2 executor call failed");

        // 4. Profit check
        uint256 balAfter = IERC20(weth).balanceOf(address(this));
        require(balAfter >= balBefore + minProfit, "unprofitable");
        profit = balAfter - balBefore;
        emit ArbExecuted(1, amountIn, profit);
    }

    /// @notice Owner withdraw — for recovering funds.
    function withdraw(address token, uint256 amount) external {
        require(msg.sender == owner, "only owner");
        IERC20(token).transfer(owner, amount);
    }
}
