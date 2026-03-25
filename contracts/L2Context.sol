// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title L2Context - Per-block L1 parent context set by builder-signed transaction
/// @notice Called by the authorized caller (builder) as the first transaction in every L2 block.
///         Stores the L1 parent block info that L2 contracts can read. L2 block number and
///         timestamp are available natively via block.number and block.timestamp.
contract L2Context {
    /// @notice The authorized caller that can call setContext.
    address public immutable AUTHORIZED_CALLER;

    /// @notice Per-block context storing the L1 parent block metadata.
    struct BlockContext {
        uint256 l1ParentBlockNumber;
        bytes32 l1ParentBlockHash;
    }

    /// @notice Context for each L2 block number.
    /// @dev This mapping grows unboundedly with chain height. Each block adds 2 storage
    ///      slots (~64 bytes). This is an accepted tradeoff for historical L1 context
    ///      lookups on L2. A ring buffer or pruning mechanism could be added if state
    ///      growth becomes a concern.
    mapping(uint256 => BlockContext) public contexts;

    /// @notice The most recently set context.
    BlockContext public latest;

    constructor(address _authorizedCaller) {
        AUTHORIZED_CALLER = _authorizedCaller;
    }

    /// @notice Called by the builder as the first transaction in every L2 block.
    /// @param l1ParentBlockNumber The L1 parent block number (L1 head when this L2 block is built).
    /// @param l1ParentBlockHash   The L1 parent block hash.
    function setContext(
        uint256 l1ParentBlockNumber,
        bytes32 l1ParentBlockHash
    ) external {
        require(msg.sender == AUTHORIZED_CALLER, "only authorized");
        require(contexts[block.number].l1ParentBlockHash == bytes32(0), "context already set");

        BlockContext memory ctx = BlockContext({
            l1ParentBlockNumber: l1ParentBlockNumber,
            l1ParentBlockHash: l1ParentBlockHash
        });

        contexts[block.number] = ctx;
        latest = ctx;
    }
}
