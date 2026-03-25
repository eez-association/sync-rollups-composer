// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title PingPongL2
/// @notice Configurable-depth cross-chain ping-pong contract deployed on L2.
///
/// The `maxRounds` parameter controls how many L2->L1 calls happen.
/// Each round is one L2->L1 call. Between rounds, L1 makes a return call to L2.
///
///   maxRounds=1: L2->L1 (terminal)                         -- 1 cross-chain hop
///   maxRounds=2: L2->L1, L1->L2 return, L2->L1 terminal   -- 3 hops
///   maxRounds=N: N L2->L1 calls + (N-1) L1->L2 returns    -- 2N-1 hops
///
/// Call sequence for maxRounds=3:
///   1. User calls start(3)                      (L2)
///      -> L2->L1: PingPongL1.ping(1, 3)
///   2. L1 ping(1,3): round < maxRounds
///      -> L1->L2 return: PingPongL2.pong(2, 3)
///   3. L2 pong(2, 3)
///      -> L2->L1: PingPongL1.ping(2, 3)
///   4. L1 ping(2,3): round < maxRounds
///      -> L1->L2 return: PingPongL2.pong(3, 3)
///   5. L2 pong(3, 3)
///      -> L2->L1: PingPongL1.ping(3, 3)
///   6. L1 ping(3,3): round == maxRounds, terminal
///      -> done = true
///
/// Addresses are set post-deployment via setup() to break the circular dependency.
contract PingPongL2 {
    /// @notice How many times this contract has executed (start + pong calls).
    uint256 public pingCount;

    /// @notice L1 proxy on L2 -- the CrossChainProxy representing PingPongL1 from L1.
    ///         Computed by CrossChainManagerL2.computeCrossChainProxyAddress(pingPongL1, rollupId=0).
    address public pingPongL1Proxy;

    /// @notice Actual address of PingPongL1 on L1.
    address public pingPongL1;

    /// @notice Owner -- only address allowed to call setup().
    address public immutable owner;

    constructor() {
        owner = msg.sender;
    }

    /// @notice Set proxy addresses after both contracts are deployed.
    /// @param _pingPongL1Proxy L1 proxy address on L2 (CrossChainProxy for PingPongL1).
    /// @param _pingPongL1      Actual PingPongL1 address on L1.
    function setup(address _pingPongL1Proxy, address _pingPongL1) external {
        require(msg.sender == owner, "PingPongL2: not owner");
        require(_pingPongL1Proxy != address(0), "PingPongL2: zero proxy");
        require(_pingPongL1 != address(0), "PingPongL2: zero l1");
        pingPongL1Proxy = _pingPongL1Proxy;
        pingPongL1 = _pingPongL1;
    }

    /// @notice Entry point: starts the ping-pong chain with configurable depth.
    /// @param maxRounds Number of L2->L1 calls to make (must be >= 1).
    ///        Total cross-chain hops = 2*maxRounds - 1.
    function start(uint256 maxRounds) external {
        require(pingPongL1Proxy != address(0), "PingPongL2: not set up");
        require(maxRounds >= 1, "PingPongL2: maxRounds must be >= 1");
        pingCount++;
        // Round 1: L2->L1 call to PingPongL1.ping(1, maxRounds)
        (bool success,) = pingPongL1Proxy.call(
            abi.encodeCall(IPingPongL1.ping, (1, maxRounds))
        );
        require(success, "PingPongL2: L2->L1 ping failed");
    }

    /// @notice Called via L1->L2 return call (scope navigation).
    ///         Makes the next L2->L1 call in the ping-pong chain.
    /// @param round The round number for the NEXT L2->L1 call.
    /// @param maxRounds Total number of rounds (passed through unchanged).
    function pong(uint256 round, uint256 maxRounds) external {
        require(pingPongL1Proxy != address(0), "PingPongL2: not set up");
        pingCount++;
        // L2->L1 call: PingPongL1.ping(round, maxRounds)
        (bool success,) = pingPongL1Proxy.call(
            abi.encodeCall(IPingPongL1.ping, (round, maxRounds))
        );
        require(success, "PingPongL2: L2->L1 ping failed");
    }
}

/// @notice Minimal interface for cross-chain calls targeting PingPongL1.
interface IPingPongL1 {
    function ping(uint256 round, uint256 maxRounds) external;
}
