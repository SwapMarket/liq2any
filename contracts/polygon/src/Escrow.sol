// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {TokenCall} from "./TokenCall.sol";
import {SwapVault} from "./SwapVault.sol";

interface IERC20Permit {
    function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s)
        external;
}

/// liq2any's Polygon escrow. Specification: docs/design.md section 2. Immutable: no owner, no upgrade, no pause, no fee.
///
/// A lock pays out to whoever reveals `s = t·Gx mod n` for the point `T = t·G` whose address it holds, or returns to its
/// initiator after its deadline. Each swap's coins sit in their own vault.
contract Escrow {
    using TokenCall for address;

    enum State {
        None,
        Open,
        Claimed,
        Refunded
    }

    /// Packed into four slots, plus `revealed` once claimed.
    struct Lock {
        address initiator;
        uint64 deadlineTime;
        State state;
        bool longLeg;
        bool extended;
        address recipient;
        uint64 deadlineBlock;
        address lockAddress;
        uint96 gasDrop;
        uint128 amount;
        bytes32 revealed;
    }

    /// The x coordinate of the secp256k1 generator, used as `r` in the recovery below.
    bytes32 internal constant GX = 0x79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798;
    /// Furthest a lock's time deadline may be set, and furthest `extend` may move it.
    uint256 public constant MAX_WINDOW = 14 days;
    uint256 public constant MAX_EXTENSION = 7 days;

    address public immutable token;
    address public immutable vaultImplementation;
    /// The block counterparts of the two limits above, set for the network's block time at deployment.
    uint256 public immutable maxWindowBlocks;
    uint256 public immutable maxExtensionBlocks;
    bytes32 private immutable vaultCodeHash;

    mapping(bytes32 => Lock) public locks;
    uint256 private entered = 1;

    event Locked(
        bytes32 indexed id,
        address indexed initiator,
        address indexed recipient,
        address lockAddress,
        uint256 amount,
        uint64 deadlineTime,
        uint64 deadlineBlock,
        bool longLeg,
        address gasTo,
        uint256 gasDrop
    );
    event Claimed(bytes32 indexed id, bytes32 revealed);
    event Refunded(bytes32 indexed id);
    event Released(bytes32 indexed id);
    event Extended(bytes32 indexed id, uint64 deadlineTime, uint64 deadlineBlock);

    error BadLock();
    error AlreadyUsed();
    error NotOpen();
    error ClaimClosed();
    error WrongSecret();
    error RefundNotOpen();
    error NotRecipient();
    error ExtensionNotAllowed();
    error WrongAmount();
    error GasDropFailed();
    error Reentered();
    error VaultFailed();

    modifier guarded() {
        if (entered != 1) revert Reentered();
        entered = 2;
        _;
        entered = 1;
    }

    constructor(address token_, uint256 maxWindowBlocks_, uint256 maxExtensionBlocks_) {
        if (token_.code.length == 0 || maxWindowBlocks_ == 0 || maxExtensionBlocks_ == 0) revert BadLock();
        if (maxWindowBlocks_ > type(uint64).max || maxExtensionBlocks_ > type(uint64).max) revert BadLock();
        token = token_;
        maxWindowBlocks = maxWindowBlocks_;
        maxExtensionBlocks = maxExtensionBlocks_;
        address implementation = address(new SwapVault(token_));
        vaultImplementation = implementation;
        vaultCodeHash = keccak256(_proxyCode(implementation));
    }

    // ---- Views ----

    /// Whether `s` reveals the secret behind `lockAddress`, the address of `T`: `s = t·Gx` where `t·G = T`.
    ///
    /// `ecrecover` computes `r⁻¹(s·R − z·G)`. With `z = 0`, `r = Gx` and `v = 27` (so `R = G`, whose y is even) that is
    /// `(s / Gx)·G = t·G`, and it returns that point's address. A zero or out-of-range `s` fails to recover and returns zero,
    /// which never matches, since no lock has a zero `lockAddress`.
    function reveals(bytes32 s, address lockAddress) public pure returns (bool) {
        return lockAddress != address(0) && ecrecover(bytes32(0), 27, GX, s) == lockAddress;
    }

    function idOf(address initiator, address lockAddress) public pure returns (bytes32) {
        return keccak256(abi.encode(initiator, lockAddress));
    }

    function vaultOf(bytes32 id) public view returns (address) {
        return address(uint160(uint256(keccak256(abi.encodePacked(bytes1(0xff), address(this), id, vaultCodeHash)))));
    }

    /// The clock rule of docs/design.md section 2.4. A short leg's claim needs both clocks before their deadlines, a long leg's
    /// either one.
    function canClaim(bytes32 id) public view returns (bool) {
        Lock storage l = locks[id];
        if (l.state != State.Open) return false;
        bool timeOpen = block.timestamp < l.deadlineTime;
        bool blocksOpen = block.number < l.deadlineBlock;
        return l.longLeg ? (timeOpen || blocksOpen) : (timeOpen && blocksOpen);
    }

    /// The exact opposite of `canClaim` while open.
    function canRefund(bytes32 id) public view returns (bool) {
        return locks[id].state == State.Open && !canClaim(id);
    }

    // ---- Locking ----

    /// Lock `amount` from the caller. Any POL sent is forwarded to `gasTo` at once: the maker's gas drop to the taker.
    function lock(
        address lockAddress,
        address recipient,
        uint128 amount,
        uint64 deadlineTime,
        uint64 deadlineBlock,
        bool longLeg,
        address gasTo
    ) external payable guarded returns (bytes32 id) {
        return _lock(lockAddress, recipient, amount, deadlineTime, deadlineBlock, longLeg, gasTo);
    }

    /// `lock`, approving with a `permit` first. A permit someone else already used doesn't stop the lock, as long as the
    /// allowance is there. A permit that never expires is refused, so a page can't ask for one.
    function lockWithPermit(
        address lockAddress,
        address recipient,
        uint128 amount,
        uint64 deadlineTime,
        uint64 deadlineBlock,
        bool longLeg,
        address gasTo,
        uint256 permitDeadline,
        uint8 v,
        bytes32 r,
        bytes32 s
    ) external payable guarded returns (bytes32 id) {
        if (permitDeadline == 0) revert BadLock();
        try IERC20Permit(token).permit(msg.sender, address(this), amount, permitDeadline, v, r, s) {} catch {}
        return _lock(lockAddress, recipient, amount, deadlineTime, deadlineBlock, longLeg, gasTo);
    }

    function _lock(
        address lockAddress,
        address recipient,
        uint128 amount,
        uint64 deadlineTime,
        uint64 deadlineBlock,
        bool longLeg,
        address gasTo
    ) private returns (bytes32 id) {
        if (
            lockAddress == address(0) || recipient == address(0) || recipient == msg.sender || amount == 0
                || deadlineTime <= block.timestamp || deadlineTime > block.timestamp + MAX_WINDOW
                || deadlineBlock <= block.number || deadlineBlock > block.number + maxWindowBlocks
                || msg.value > type(uint96).max || (msg.value != 0 && gasTo == address(0))
        ) revert BadLock();
        id = idOf(msg.sender, lockAddress);
        Lock storage l = locks[id];
        if (l.state != State.None) revert AlreadyUsed();
        l.initiator = msg.sender;
        l.deadlineTime = deadlineTime;
        l.state = State.Open;
        l.longLeg = longLeg;
        l.recipient = recipient;
        l.deadlineBlock = deadlineBlock;
        l.lockAddress = lockAddress;
        l.gasDrop = uint96(msg.value);
        l.amount = amount;

        address vault = _deployVault(id);
        // The vault's address can be known in advance, so it may already hold a donation: count only what this lock adds.
        uint256 before = token.balanceOf(vault);
        token.transferFrom(msg.sender, vault, amount);
        if (token.balanceOf(vault) - before != amount) revert WrongAmount();

        emit Locked(
            id, msg.sender, recipient, lockAddress, amount, deadlineTime, deadlineBlock, longLeg, gasTo, msg.value
        );
        if (msg.value != 0) {
            (bool ok,) = gasTo.call{value: msg.value}("");
            if (!ok) revert GasDropFailed();
        }
    }

    // ---- Settling ----

    /// Pay the recipient. Anyone holding `s` may call this.
    function claim(bytes32 id, bytes32 s) external guarded {
        Lock storage l = locks[id];
        if (l.state != State.Open) revert NotOpen();
        if (!canClaim(id)) revert ClaimClosed();
        if (!reveals(s, l.lockAddress)) revert WrongSecret();
        l.revealed = s;
        l.state = State.Claimed;
        emit Claimed(id, s);
        SwapVault(vaultOf(id)).pay(l.recipient, l.amount);
    }

    /// Return the coins to the initiator once the claim has closed. Anyone may call this.
    function refund(bytes32 id) external guarded {
        Lock storage l = locks[id];
        if (l.state != State.Open) revert NotOpen();
        if (canClaim(id)) revert RefundNotOpen();
        l.state = State.Refunded;
        emit Refunded(id);
        SwapVault(vaultOf(id)).pay(l.initiator, l.amount);
    }

    /// The recipient gives the swap up and returns the coins at once: the early refund.
    function release(bytes32 id) external guarded {
        Lock storage l = locks[id];
        if (l.state != State.Open) revert NotOpen();
        if (msg.sender != l.recipient) revert NotRecipient();
        l.state = State.Refunded;
        emit Released(id);
        SwapVault(vaultOf(id)).pay(l.initiator, l.amount);
    }

    /// The recipient of a long leg moves both deadlines later, once, for a Liquid halt (liq2sol's D1).
    function extend(bytes32 id, uint64 newTime, uint64 newBlock) external guarded {
        Lock storage l = locks[id];
        if (msg.sender != l.recipient) revert NotRecipient();
        if (
            !l.longLeg || l.extended || !canClaim(id) || newTime <= l.deadlineTime
                || newTime > uint256(l.deadlineTime) + MAX_EXTENSION || newBlock <= l.deadlineBlock
                || newBlock > uint256(l.deadlineBlock) + maxExtensionBlocks
        ) revert ExtensionNotAllowed();
        l.deadlineTime = newTime;
        l.deadlineBlock = newBlock;
        l.extended = true;
        emit Extended(id, newTime, newBlock);
    }

    // ---- Vaults ----

    function _deployVault(bytes32 id) private returns (address vault) {
        bytes memory code = _proxyCode(vaultImplementation);
        assembly {
            vault := create2(0, add(code, 0x20), mload(code), id)
        }
        if (vault == address(0)) revert VaultFailed();
    }

    /// EIP-1167 minimal proxy creation code for `implementation`.
    function _proxyCode(address implementation) private pure returns (bytes memory) {
        return abi.encodePacked(
            hex"3d602d80600a3d3981f3363d3d373d3d3d363d73", implementation, hex"5af43d82803e903d91602b57fd5bf3"
        );
    }
}
