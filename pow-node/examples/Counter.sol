// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// A deliberately small contract that exercises everything the EVM milestones add:
/// storage (count), events (Incremented), calls that change state, a read-only
/// getter, and a revert with a custom error.
contract Counter {
    uint256 public count;

    event Incremented(address indexed by, uint256 newCount);

    error TooBig();

    function increment() external {
        count += 1;
        emit Incremented(msg.sender, count);
    }

    function set(uint256 value) external {
        if (value > 100) revert TooBig();
        count = value;
    }
}
