// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Mock verifier that always returns true. Used during flash loan
///         development so iterative traceCallMany discovery works without
///         needing a valid ECDSA proof. The signing logic in l1_proxy.rs is
///         kept intact — once this mock is replaced by tmpECDSAVerifier the
///         signatures will be validated for real.
contract MockECDSAVerifier {
    function verify(bytes calldata, bytes32) external pure returns (bool) {
        return true;
    }
}
