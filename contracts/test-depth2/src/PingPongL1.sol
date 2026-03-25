// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title PingPongL1
/// @notice Configurable-depth cross-chain ping-pong contract deployed on L1.
///
/// The `maxRounds` parameter controls how many L2->L1 calls happen.
/// Each round is one L2->L1 call. Between rounds, L1 makes a return call to L2.
///
///   maxRounds=1: L2->L1 (terminal)                         -- 1 cross-chain hop
///   maxRounds=2: L2->L1, L1->L2 return, L2->L1 terminal   -- 3 hops
///   maxRounds=N: N L2->L1 calls + (N-1) L1->L2 returns    -- 2N-1 hops
///
/// Call sequence for maxRounds=3 (from L1's perspective):
///   1. ping(1, 3): round < maxRounds -> L1->L2 return: pong(2, 3)
///   2. ping(2, 3): round < maxRounds -> L1->L2 return: pong(3, 3)
///   3. ping(3, 3): round == maxRounds -> done = true (terminal)
///
/// The L2 proxy address (representing PingPongL2 on L1) is set post-deployment
/// via setup() to break the circular address dependency.
contract PingPongL1 {
    /// @notice Counts each L2→L1 ping received (incremented by ping(), NOT by any pong function).
    uint256 public pongCount;

    /// @notice Set to true when the last round completes (terminal).
    bool public done;

    /// @notice L2 proxy on L1 -- the CrossChainProxy representing PingPongL2 from L2.
    ///         Computed by Rollups.computeCrossChainProxyAddress(pingPongL2, rollupId=1).
    address public pingPongL2Proxy;

    /// @notice Owner -- only address allowed to call setup().
    address public immutable owner;

    constructor() {
        owner = msg.sender;
    }

    /// @notice Set proxy address after both contracts are deployed.
    /// @param _pingPongL2Proxy L2 proxy address on L1 (CrossChainProxy for PingPongL2).
    function setup(address _pingPongL2Proxy) external {
        require(msg.sender == owner, "PingPongL1: not owner");
        require(_pingPongL2Proxy != address(0), "PingPongL1: zero proxy");
        pingPongL2Proxy = _pingPongL2Proxy;
    }

    /// @notice Called via L2->L1 cross-chain call.
    ///         If this is not the last round, makes a return L1->L2 call
    ///         so L2 can initiate the next round.
    ///         If this IS the last round, sets done=true (terminal).
    /// @param round Current round number (1-indexed).
    /// @param maxRounds Total number of rounds.
    function ping(uint256 round, uint256 maxRounds) external {
        require(pingPongL2Proxy != address(0), "PingPongL1: not set up");
        pongCount++;
        if (round < maxRounds) {
            // Not the last round: make L1->L2 return call with next round number
            (bool success,) = pingPongL2Proxy.call(
                abi.encodeCall(IPingPongL2.pong, (round + 1, maxRounds))
            );
            require(success, "PingPongL1: L1->L2 pong failed");
        } else {
            // Last round: terminal, no more cross-chain calls
            done = true;
        }
    }
}

/// @notice Minimal interface for cross-chain calls targeting PingPongL2.
interface IPingPongL2 {
    function pong(uint256 round, uint256 maxRounds) external;
}
