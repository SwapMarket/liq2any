// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

/// A plain ERC-20 with USDT0's blocklist rules on Polygon (docs/assumptions.md section 1.2): a blocked address can't send, and a
/// blocked caller can't use `transferFrom`, but a blocked address still receives. Also a classic permit.
contract MockUSDT {
    string public constant name = "USDT0";
    uint8 public constant decimals = 6;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    mapping(address => bool) public isBlocked;
    mapping(address => uint256) public nonces;
    bytes32 public immutable DOMAIN_SEPARATOR;
    bytes32 public constant PERMIT_TYPEHASH =
        keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)");

    constructor() {
        DOMAIN_SEPARATOR = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,address verifyingContract,bytes32 salt)"),
                keccak256(bytes(name)),
                keccak256("1"),
                address(this),
                bytes32(block.chainid)
            )
        );
    }

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function block_(address who, bool blocked) external {
        isBlocked[who] = blocked;
    }

    function burnAll(address who) external {
        require(isBlocked[who], "not blocked");
        balanceOf[who] = 0;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external virtual returns (bool) {
        _move(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external virtual returns (bool) {
        require(!isBlocked[msg.sender], "Blocked: msg.sender is blocked");
        require(allowance[from][msg.sender] >= amount, "allowance");
        allowance[from][msg.sender] -= amount;
        _move(from, to, amount);
        return true;
    }

    function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s)
        external
    {
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                DOMAIN_SEPARATOR,
                keccak256(abi.encode(PERMIT_TYPEHASH, owner, spender, value, nonces[owner]++, deadline))
            )
        );
        require(owner != address(0) && owner == ecrecover(digest, v, r, s), "USDT0: INVALID-PERMIT");
        require(deadline == 0 || block.timestamp <= deadline, "USDT0: PERMIT-EXPIRED");
        allowance[owner][spender] = value;
    }

    function _move(address from, address to, uint256 amount) internal virtual {
        require(!isBlocked[from], "USDT0: from is blocked");
        require(balanceOf[from] >= amount, "balance");
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
    }
}

/// Keeps 1 unit of every transfer.
contract FeeToken is MockUSDT {
    function _move(address from, address to, uint256 amount) internal override {
        super._move(from, to, amount);
        if (amount > 0) balanceOf[to] -= 1;
    }
}

/// Returns false instead of reverting, or reverts with no data at all.
contract FalseToken is MockUSDT {
    bool public failTransfers;
    bool public revertEmpty;

    function setFail(bool fail) external {
        failTransfers = fail;
    }

    function setRevertEmpty(bool on) external {
        revertEmpty = on;
    }

    function transfer(address to, uint256 amount) external override returns (bool) {
        if (revertEmpty) {
            assembly {
                revert(0, 0)
            }
        }
        if (failTransfers) return false;
        _move(msg.sender, to, amount);
        return true;
    }
}

/// Returns nothing, like Ethereum's USDT.
contract NoReturnToken is MockUSDT {
    function transfer(address to, uint256 amount) external override returns (bool) {
        _move(msg.sender, to, amount);
        assembly {
            return(0, 0)
        }
    }

    function transferFrom(address from, address to, uint256 amount) external override returns (bool) {
        require(allowance[from][msg.sender] >= amount, "allowance");
        allowance[from][msg.sender] -= amount;
        _move(from, to, amount);
        assembly {
            return(0, 0)
        }
    }
}

/// Calls back into a target during transfers, with calldata set by the test.
contract ReentrantToken is MockUSDT {
    address public target;
    bytes public payload;
    bool public reverted;

    function arm(address target_, bytes calldata payload_) external {
        target = target_;
        payload = payload_;
    }

    function _move(address from, address to, uint256 amount) internal override {
        if (target != address(0)) {
            address t = target;
            target = address(0);
            (bool ok,) = t.call(payload);
            reverted = !ok;
        }
        super._move(from, to, amount);
    }
}

/// A gas-drop receiver that calls back into a target, or refuses POL.
contract Receiver {
    address public target;
    bytes public payload;
    bool public refuse;
    bool public reentryFailed;

    function arm(address target_, bytes calldata payload_) external {
        target = target_;
        payload = payload_;
    }

    function setRefuse(bool refuse_) external {
        refuse = refuse_;
    }

    receive() external payable {
        require(!refuse, "refused");
        if (target != address(0)) {
            (bool ok,) = target.call(payload);
            reentryFailed = !ok;
        }
    }
}
