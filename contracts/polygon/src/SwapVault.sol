// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {TokenCall} from "./TokenCall.sol";

/// Holds one swap's coins. `Escrow` deploys a minimal proxy of this per swap (docs/design.md section 2.1), so a Tether block on
/// one address freezes one swap at most. Its only function lets `Escrow` pay out. The proxy delegates here, so `escrow` and
/// `token` are this implementation's immutables, the same for every vault.
contract SwapVault {
    address private immutable escrow;
    address private immutable token;

    error OnlyEscrow();

    constructor(address token_) {
        escrow = msg.sender;
        token = token_;
    }

    function pay(address to, uint256 amount) external {
        if (msg.sender != escrow) revert OnlyEscrow();
        TokenCall.transfer(token, to, amount);
    }
}
