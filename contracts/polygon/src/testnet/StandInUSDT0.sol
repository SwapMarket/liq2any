// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

/// TEST NETWORKS ONLY. USDT0 has no testnet (docs/assumptions.md section 8), so Amoy uses this worthless stand-in. It copies what
/// the escrow, the maker and the page rely on in Polygon's real USDT0 (section 1):
/// - 6 decimals;
/// - a classic permit over Polygon's salt domain, with no chainId field, where a deadline of 0 never expires;
/// - Tether's blocklist rules: a blocked address can't send, a blocked caller can't use `transferFrom`, a blocked address still
///   receives, and its funds can be destroyed;
/// - no transfers to the token itself.
/// Anyone may mint from the faucet, up to `FAUCET_MAX` per call. The deployer alone blocks, unblocks and destroys.
contract StandInUSDT0 {
    string public constant name = "Test USDT0 (Amoy stand-in, worthless)";
    string public constant symbol = "tUSDT0";
    uint8 public constant decimals = 6;
    uint256 public constant FAUCET_MAX = 1_000e6;
    bytes32 public constant PERMIT_TYPEHASH =
        keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)");

    address public immutable blocker;
    bytes32 public immutable DOMAIN_SEPARATOR;
    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    mapping(address => uint256) public nonces;
    mapping(address => bool) public isBlocked;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
    event BlockPlaced(address indexed user);
    event BlockReleased(address indexed user);
    event DestroyedBlockedFunds(address indexed user, uint256 balance);

    constructor() {
        blocker = msg.sender;
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

    function faucet(uint256 amount) external {
        require(amount <= FAUCET_MAX, "tUSDT0: over the faucet's limit");
        totalSupply += amount;
        balanceOf[msg.sender] += amount;
        emit Transfer(address(0), msg.sender, amount);
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        _approve(msg.sender, spender, amount);
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _move(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        require(!isBlocked[msg.sender], "Blocked: msg.sender is blocked");
        uint256 allowed = allowance[from][msg.sender];
        require(allowed >= amount, "ERC20: transfer amount exceeds allowance");
        _move(from, to, amount);
        _approve(from, msg.sender, allowed - amount);
        return true;
    }

    function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s)
        external
    {
        require(owner != address(0), "USDT0: HOLDER-ZERO");
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                DOMAIN_SEPARATOR,
                keccak256(abi.encode(PERMIT_TYPEHASH, owner, spender, value, nonces[owner]++, deadline))
            )
        );
        require(owner == ecrecover(digest, v, r, s), "USDT0: INVALID-PERMIT");
        require(deadline == 0 || block.timestamp <= deadline, "USDT0: PERMIT-EXPIRED");
        _approve(owner, spender, value);
    }

    function addToBlockedList(address user) external {
        require(msg.sender == blocker, "USDT0: INSUFFICIENT_PERMISSIONS");
        isBlocked[user] = true;
        emit BlockPlaced(user);
    }

    function removeFromBlockedList(address user) external {
        require(msg.sender == blocker, "USDT0: INSUFFICIENT_PERMISSIONS");
        isBlocked[user] = false;
        emit BlockReleased(user);
    }

    function destroyBlockedFunds(address user) external {
        require(msg.sender == blocker, "USDT0: INSUFFICIENT_PERMISSIONS");
        require(isBlocked[user], "USDT0: user is not blocked");
        uint256 funds = balanceOf[user];
        balanceOf[user] = 0;
        totalSupply -= funds;
        emit Transfer(user, address(0), funds);
        emit DestroyedBlockedFunds(user, funds);
    }

    function _approve(address owner, address spender, uint256 amount) private {
        allowance[owner][spender] = amount;
        emit Approval(owner, spender, amount);
    }

    function _move(address from, address to, uint256 amount) private {
        require(!isBlocked[from], "USDT0: from is blocked");
        require(to != address(0), "ERC20: transfer to the zero address");
        require(to != address(this), "USDT0: transfer to the contract address");
        require(balanceOf[from] >= amount, "ERC20: transfer amount exceeds balance");
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
    }
}
