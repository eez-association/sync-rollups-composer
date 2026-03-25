// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title PingPongReturnL1
/// @notice Like PingPongL1, but ping() returns uint256 (pongCount) to validate
///         L2->L1 return data propagation (regression test for issue #242).
///
///   ping(round, maxRounds):
///     - Increments pongCount
///     - If round < maxRounds: makes L1->L2 return call to pong(round+1, maxRounds)
///     - If round == maxRounds: sets done=true (terminal)
///     - Returns pongCount
///
/// The return value flows back to L2 through the RESULT execution entry.
/// Before the #242 fix, this return data was lost (L2 caller got empty bytes).
contract PingPongReturnL1 {
    uint256 public pongCount;
    bool public done;
    uint256 public lastReturnValue;

    address public pingPongL2Proxy;
    address public immutable owner;

    constructor() {
        owner = msg.sender;
    }

    function setup(address _pingPongL2Proxy) external {
        require(msg.sender == owner, "PingPongReturnL1: not owner");
        require(_pingPongL2Proxy != address(0), "PingPongReturnL1: zero proxy");
        pingPongL2Proxy = _pingPongL2Proxy;
    }

    /// @notice Called via L2->L1 cross-chain call. Returns pongCount.
    function ping(uint256 round, uint256 maxRounds) external returns (uint256) {
        require(pingPongL2Proxy != address(0), "PingPongReturnL1: not set up");
        pongCount++;
        if (round < maxRounds) {
            (bool success, bytes memory data) = pingPongL2Proxy.call(
                abi.encodeCall(IPingPongReturnL2.pong, (round + 1, maxRounds))
            );
            require(success, "PingPongReturnL1: L1->L2 pong failed");
            lastReturnValue = abi.decode(data, (uint256));
        } else {
            done = true;
        }
        return pongCount;
    }
}

interface IPingPongReturnL2 {
    function pong(uint256 round, uint256 maxRounds) external returns (uint256);
}
