// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function decimals() external view returns (uint8);
    function symbol() external view returns (string memory);
}

/// @title SimpleAMM
/// @notice Minimal constant-product AMM (x*y=k) for two ERC20 tokens.
///         Supports addLiquidity, removeLiquidity, and swap.
///         No LP tokens — single liquidity provider model for simplicity.
contract SimpleAMM {
    IERC20 public immutable tokenA;
    IERC20 public immutable tokenB;
    string public poolName;

    uint256 public reserveA;
    uint256 public reserveB;
    address public liquidityProvider;

    event LiquidityAdded(address indexed provider, uint256 amountA, uint256 amountB);
    event LiquidityRemoved(address indexed provider, uint256 amountA, uint256 amountB);
    event Swap(address indexed user, address tokenIn, uint256 amountIn, address tokenOut, uint256 amountOut);

    constructor(address _tokenA, address _tokenB) {
        tokenA = IERC20(_tokenA);
        tokenB = IERC20(_tokenB);
        poolName = string(abi.encodePacked(
            IERC20(_tokenA).symbol(), "/", IERC20(_tokenB).symbol()
        ));
    }

    function addLiquidity(uint256 amountA, uint256 amountB) external {
        require(amountA > 0 && amountB > 0, "zero amount");
        tokenA.transferFrom(msg.sender, address(this), amountA);
        tokenB.transferFrom(msg.sender, address(this), amountB);
        reserveA += amountA;
        reserveB += amountB;
        liquidityProvider = msg.sender;
        emit LiquidityAdded(msg.sender, amountA, amountB);
    }

    function removeLiquidity() external {
        require(msg.sender == liquidityProvider, "not provider");
        uint256 a = reserveA;
        uint256 b = reserveB;
        reserveA = 0;
        reserveB = 0;
        tokenA.transfer(msg.sender, a);
        tokenB.transfer(msg.sender, b);
        emit LiquidityRemoved(msg.sender, a, b);
    }

    /// @notice Swap tokenIn for tokenOut using constant product formula.
    ///         0.3% fee applied to input.
    function swap(address tokenIn, uint256 amountIn) external returns (uint256 amountOut) {
        require(amountIn > 0, "zero input");
        require(tokenIn == address(tokenA) || tokenIn == address(tokenB), "invalid token");

        bool isAtoB = tokenIn == address(tokenA);
        (uint256 resIn, uint256 resOut) = isAtoB ? (reserveA, reserveB) : (reserveB, reserveA);

        // Transfer in
        IERC20(tokenIn).transferFrom(msg.sender, address(this), amountIn);

        // Constant product with 0.3% fee
        uint256 amountInWithFee = amountIn * 997;
        amountOut = (amountInWithFee * resOut) / (resIn * 1000 + amountInWithFee);
        require(amountOut > 0, "insufficient output");

        // Transfer out
        if (isAtoB) {
            reserveA += amountIn;
            reserveB -= amountOut;
            tokenB.transfer(msg.sender, amountOut);
        } else {
            reserveB += amountIn;
            reserveA -= amountOut;
            tokenA.transfer(msg.sender, amountOut);
        }

        emit Swap(msg.sender, tokenIn, amountIn, isAtoB ? address(tokenB) : address(tokenA), amountOut);
    }

    /// @notice Get quote for a swap (view only, no state change)
    function getAmountOut(address tokenIn, uint256 amountIn) external view returns (uint256) {
        require(tokenIn == address(tokenA) || tokenIn == address(tokenB), "invalid token");
        bool isAtoB = tokenIn == address(tokenA);
        (uint256 resIn, uint256 resOut) = isAtoB ? (reserveA, reserveB) : (reserveB, reserveA);
        if (resIn == 0 || resOut == 0 || amountIn == 0) return 0;
        uint256 amountInWithFee = amountIn * 997;
        return (amountInWithFee * resOut) / (resIn * 1000 + amountInWithFee);
    }

    /// @notice Get current price of tokenA in terms of tokenB
    function price() external view returns (uint256) {
        if (reserveA == 0) return 0;
        return (reserveB * 1e18) / reserveA;
    }
}
