// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {TokenCall} from "./TokenCall.sol";

interface IEscrow {
    function token() external view returns (address);
    function lock(
        address lockAddress,
        address recipient,
        uint128 amount,
        uint64 deadlineTime,
        uint64 deadlineBlock,
        bool longLeg,
        address gasTo
    ) external payable returns (bytes32);
    function release(bytes32 id) external;
    function extend(bytes32 id, uint64 newTime, uint64 newBlock) external;
}

/// A maker's float on Polygon, as liq2sol's maker vault (its D6; here decision P9, docs/design.md section 2.6).
///
/// The vault is the maker's Polygon address in both directions: the initiator of its locks, so refunds return here, and the
/// recipient of takers' locks, so claims pay here. A cold owner alone withdraws. A hot operator locks, at most `windowCap` per
/// `windowSecs`, and passes `release` and `extend` on to the escrow, which accepts them only from a lock's recipient.
///
/// The operator can still lock up to the cap to a recipient it controls and claim it, so the cap, not the key separation,
/// limits a stolen hot key. The vault is one address Tether can block, so the maker keeps its balance near operating needs.
contract MakerVault {
    using TokenCall for address;

    IEscrow public immutable escrow;
    address public immutable token;
    address public immutable owner;

    address public operator;
    uint64 public windowSecs;
    uint128 public windowCap;
    uint64 public windowStart;
    uint128 public windowUsed;

    event OperatorSet(address operator, uint64 windowSecs, uint128 windowCap);
    event Withdrawn(address to, uint256 amount);

    error NotOwner();
    error NotOperator();
    error BadSetting();
    error OverCap();

    constructor(IEscrow escrow_, address owner_, address operator_, uint64 windowSecs_, uint128 windowCap_) {
        if (address(escrow_).code.length == 0 || owner_ == address(0)) revert BadSetting();
        escrow = escrow_;
        token = escrow_.token();
        owner = owner_;
        _setOperator(operator_, windowSecs_, windowCap_);
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier onlyOperator() {
        if (msg.sender != operator) revert NotOperator();
        _;
    }

    // ---- Owner (cold) ----

    function withdraw(address to, uint256 amount) external onlyOwner {
        if (to == address(0)) revert BadSetting();
        emit Withdrawn(to, amount);
        token.transfer(to, amount);
    }

    /// Rotate the hot key or change the cap. A zero operator stops all locking.
    function setOperator(address operator_, uint64 windowSecs_, uint128 windowCap_) external onlyOwner {
        _setOperator(operator_, windowSecs_, windowCap_);
    }

    function _setOperator(address operator_, uint64 windowSecs_, uint128 windowCap_) private {
        if (windowSecs_ == 0) revert BadSetting();
        operator = operator_;
        windowSecs = windowSecs_;
        windowCap = windowCap_;
        emit OperatorSet(operator_, windowSecs_, windowCap_);
    }

    // ---- Operator (hot) ----

    /// `Escrow.lock` from the vault's coins, within the cap. POL sent is the gas drop, passed on as it is.
    function lock(
        address lockAddress,
        address recipient,
        uint128 amount,
        uint64 deadlineTime,
        uint64 deadlineBlock,
        address gasTo
    ) external payable onlyOperator returns (bytes32) {
        (windowStart, windowUsed) = afterLock(amount);
        // Exactly this lock's amount, so no allowance is left over, and a token that refuses changing a nonzero allowance
        // never sees one.
        token.approve(address(escrow), amount);
        // The maker's own lock is always the short leg (docs/design.md section 2.4).
        return escrow.lock{value: msg.value}(lockAddress, recipient, amount, deadlineTime, deadlineBlock, false, gasTo);
    }

    function release(bytes32 id) external onlyOperator {
        escrow.release(id);
    }

    function extend(bytes32 id, uint64 newTime, uint64 newBlock) external onlyOperator {
        escrow.extend(id, newTime, newBlock);
    }

    // ---- Views ----

    /// The window's (start, amount used) after locking `amount` now. Reverts if over the cap. A window ends `windowSecs` after
    /// it starts, and the next starts at the first lock after that. Refunds don't give the cap back.
    function afterLock(uint128 amount) public view returns (uint64 start, uint128 used) {
        if (block.timestamp >= uint256(windowStart) + windowSecs) {
            (start, used) = (uint64(block.timestamp), 0);
        } else {
            (start, used) = (windowStart, windowUsed);
        }
        if (used > windowCap || amount > windowCap - used) revert OverCap();
        used += amount;
    }
}
