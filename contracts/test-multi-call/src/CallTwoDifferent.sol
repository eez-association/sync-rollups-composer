// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract CallTwoDifferent {
    function callBothCounters(address counterA, address counterB) external returns (uint256 a, uint256 b) {
        (bool ok1, bytes memory ret1) = counterA.call(abi.encodeWithSignature("increment()"));
        require(ok1, "first call failed");
        a = abi.decode(ret1, (uint256));

        (bool ok2, bytes memory ret2) = counterB.call(abi.encodeWithSignature("increment()"));
        require(ok2, "second call failed");
        b = abi.decode(ret2, (uint256));
    }
}
