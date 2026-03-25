// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract CallTwice {
    function callCounterTwice(address counter) external returns (uint256 first, uint256 second) {
        (bool ok1, bytes memory ret1) = counter.call(abi.encodeWithSignature("increment()"));
        require(ok1, "first call failed");
        first = abi.decode(ret1, (uint256));

        (bool ok2, bytes memory ret2) = counter.call(abi.encodeWithSignature("increment()"));
        require(ok2, "second call failed");
        second = abi.decode(ret2, (uint256));
    }
}
