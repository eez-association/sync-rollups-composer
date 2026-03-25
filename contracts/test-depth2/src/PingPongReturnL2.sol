// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title PingPongReturnL2
/// @notice Like PingPongL2, but all functions return values to validate cross-chain
///         return data propagation (regression test for issue #242).
///
///   maxRounds=1: L2->L1 terminal, start() captures L1 return value
///   maxRounds=2: L2->L1, L1->L2 return, L2->L1 terminal
///   maxRounds=N: N L2->L1 calls + (N-1) L1->L2 returns
///
/// Key difference from PingPongL2: ping/pong/start all return uint256 and the
/// contract stores the last return value received from L1 for verification.
contract PingPongReturnL2 {
    uint256 public pingCount;
    uint256 public lastReturnValue;

    address public pingPongL1Proxy;
    address public pingPongL1;
    address public immutable owner;

    constructor() {
        owner = msg.sender;
    }

    function setup(address _pingPongL1Proxy, address _pingPongL1) external {
        require(msg.sender == owner, "PingPongReturnL2: not owner");
        require(_pingPongL1Proxy != address(0), "PingPongReturnL2: zero proxy");
        require(_pingPongL1 != address(0), "PingPongReturnL2: zero l1");
        pingPongL1Proxy = _pingPongL1Proxy;
        pingPongL1 = _pingPongL1;
    }

    /// @notice Entry point. Returns the L1 return value from the first ping.
    function start(uint256 maxRounds) external returns (uint256) {
        require(pingPongL1Proxy != address(0), "PingPongReturnL2: not set up");
        require(maxRounds >= 1, "PingPongReturnL2: maxRounds must be >= 1");
        pingCount++;
        (bool success, bytes memory data) = pingPongL1Proxy.call(
            abi.encodeCall(IPingPongReturnL1.ping, (1, maxRounds))
        );
        require(success, "PingPongReturnL2: L2->L1 ping failed");
        uint256 returnValue = abi.decode(data, (uint256));
        lastReturnValue = returnValue;
        return returnValue;
    }

    /// @notice Called via L1->L2 return call. Returns the L1 return value.
    function pong(uint256 round, uint256 maxRounds) external returns (uint256) {
        require(pingPongL1Proxy != address(0), "PingPongReturnL2: not set up");
        pingCount++;
        (bool success, bytes memory data) = pingPongL1Proxy.call(
            abi.encodeCall(IPingPongReturnL1.ping, (round, maxRounds))
        );
        require(success, "PingPongReturnL2: L2->L1 ping failed");
        uint256 returnValue = abi.decode(data, (uint256));
        lastReturnValue = returnValue;
        return returnValue;
    }
}

interface IPingPongReturnL1 {
    function ping(uint256 round, uint256 maxRounds) external returns (uint256);
}
