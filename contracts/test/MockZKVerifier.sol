// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "../sync-rollups/src/IZKVerifier.sol";

/// @notice Mock ZK verifier that accepts all proofs (development only).
contract MockZKVerifier is IZKVerifier {
    bool public shouldVerify = true;

    function setVerifyResult(bool _shouldVerify) external {
        shouldVerify = _shouldVerify;
    }

    function verify(bytes calldata, bytes32) external view override returns (bool) {
        return shouldVerify;
    }
}
