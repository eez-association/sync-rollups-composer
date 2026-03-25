// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract Counter {
    uint256 public counter;

    struct Increment {
        address sender;
        uint256 value;
    }

    Increment[] public increments;

    function increment() external returns (uint256) {
        counter++;
        increments.push(Increment(msg.sender, counter));
        return counter;
    }

    function getIncrements() external view returns (Increment[] memory) {
        return increments;
    }
}
