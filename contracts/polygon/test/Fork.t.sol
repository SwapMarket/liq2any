// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Test} from "forge-std/Test.sol";
import {Escrow} from "../src/Escrow.sol";
import {TokenCall} from "../src/TokenCall.sol";

interface IUSDT0 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
    function nonces(address) external view returns (uint256);
    function DOMAIN_SEPARATOR() external view returns (bytes32);
    function isBlocked(address) external view returns (bool);
    function addToBlockedList(address) external;
    function destroyBlockedFunds(address) external;
}

/// Against the real USDT0 on a fork of Polygon mainnet. Skipped unless POLYGON_RPC is set to an endpoint that serves recent
/// state, such as https://polygon.drpc.org (publicnode refuses state older than a few minutes, which breaks a fork).
contract EscrowForkTest is Test {
    IUSDT0 constant USDT0 = IUSDT0(0xc2132D05D31c914a87C6611C10748AEb04B58e8F);
    /// USDT0's 3-of-5 Safe: proxy owner, DEFAULT_ADMIN_ROLE and BLOCK_ROLE (docs/assumptions.md section 1.2).
    address constant TETHER_SAFE = 0x4DFF9b5b0143E642a3F63a5bcf2d1C328e600bf8;
    /// Aave's aPolUSDT, a large holder, to fund the test accounts.
    address constant WHALE = 0x6ab707Aca953eDAeFBc4fD23bA73294241490620;
    uint256 constant GX = 0x79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798;
    uint256 constant N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;

    Escrow escrow;
    address taker;
    uint256 takerKey;
    address maker = makeAddr("maker");

    function setUp() public {
        string memory rpc = vm.envOr("POLYGON_RPC", string(""));
        if (bytes(rpc).length == 0) {
            vm.skip(true);
            return;
        }
        vm.createSelectFork(rpc);
        require(block.chainid == 137, "not Polygon mainnet");
        // 14 days and 7 days at 1.5 s per block.
        escrow = new Escrow(address(USDT0), 806_400, 403_200);
        (taker, takerKey) = makeAddrAndKey("taker");
        vm.startPrank(WHALE);
        USDT0.transfer(taker, 1_000e6);
        USDT0.transfer(maker, 1_000e6);
        vm.stopPrank();
        vm.prank(maker);
        USDT0.approve(address(escrow), type(uint256).max);
        vm.prank(taker);
        USDT0.approve(address(escrow), type(uint256).max);
    }

    function secret(uint256 i) internal pure returns (uint256 t, bytes32 s) {
        t = uint256(keccak256(abi.encode("liq2any fork", i))) % (N - 1) + 1;
        s = bytes32(mulmod(t, GX, N));
    }

    function makerLock(uint256 i) internal returns (bytes32 id, bytes32 s) {
        uint256 t;
        (t, s) = secret(i);
        vm.prank(maker);
        id = escrow.lock(
            vm.addr(t), taker, 100e6, uint64(block.timestamp + 3 hours), uint64(block.number + 7200), false, address(0)
        );
    }

    function test_fork_lock_and_claim_with_real_usdt0() public {
        (bytes32 id, bytes32 s) = makerLock(1);
        assertEq(USDT0.balanceOf(escrow.vaultOf(id)), 100e6);
        uint256 before = USDT0.balanceOf(taker);
        escrow.claim(id, s);
        assertEq(USDT0.balanceOf(taker), before + 100e6);
        assertEq(USDT0.balanceOf(escrow.vaultOf(id)), 0);
    }

    function test_fork_refund_with_real_usdt0() public {
        (bytes32 id,) = makerLock(2);
        vm.warp(block.timestamp + 3 hours);
        uint256 before = USDT0.balanceOf(maker);
        escrow.refund(id);
        assertEq(USDT0.balanceOf(maker), before + 100e6);
    }

    /// A permit over Polygon's salt domain (no chainId field), as a wallet signs it.
    function test_fork_lock_with_permit_over_the_salt_domain() public {
        (address owner, uint256 key) = makeAddrAndKey("permit owner");
        vm.prank(WHALE);
        USDT0.transfer(owner, 50e6);
        bytes32 saltDomain = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,address verifyingContract,bytes32 salt)"),
                keccak256("USDT0"),
                keccak256("1"),
                address(USDT0),
                bytes32(uint256(137))
            )
        );
        assertEq(saltDomain, USDT0.DOMAIN_SEPARATOR());
        uint256 deadline = block.timestamp + 10 minutes;
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                saltDomain,
                keccak256(
                    abi.encode(
                        keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)"),
                        owner,
                        address(escrow),
                        50e6,
                        USDT0.nonces(owner),
                        deadline
                    )
                )
            )
        );
        (uint8 v, bytes32 r, bytes32 sig) = vm.sign(key, digest);
        (uint256 t,) = secret(3);
        vm.prank(owner);
        bytes32 id = escrow.lockWithPermit(
            vm.addr(t),
            maker,
            50e6,
            uint64(block.timestamp + 8 hours),
            uint64(block.number + 19200),
            true,
            address(0),
            deadline,
            v,
            r,
            sig
        );
        assertEq(USDT0.balanceOf(escrow.vaultOf(id)), 50e6);
    }

    /// Tether blocks one swap's vault: that swap stops, the others don't.
    function test_fork_tether_blocks_one_vault() public {
        (bytes32 a, bytes32 sa) = makerLock(4);
        (bytes32 b, bytes32 sb) = makerLock(5);
        address vault = escrow.vaultOf(a);
        vm.prank(TETHER_SAFE);
        USDT0.addToBlockedList(vault);
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        escrow.claim(a, sa);
        escrow.claim(b, sb);
    }

    /// Tether blocks the escrow itself: new locks stop, open swaps still settle, because each vault sends its own coins.
    function test_fork_tether_blocks_the_escrow() public {
        (bytes32 a, bytes32 sa) = makerLock(6);
        vm.prank(TETHER_SAFE);
        USDT0.addToBlockedList(address(escrow));
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        makerLock(7);
        uint256 before = USDT0.balanceOf(taker);
        escrow.claim(a, sa);
        assertEq(USDT0.balanceOf(taker), before + 100e6);
    }

    /// Tether burns a blocked vault's coins: that swap's coins are gone, and its claim and refund both fail.
    function test_fork_tether_burns_a_blocked_vault() public {
        (bytes32 a, bytes32 sa) = makerLock(8);
        address vault = escrow.vaultOf(a);
        vm.startPrank(TETHER_SAFE);
        USDT0.addToBlockedList(vault);
        USDT0.destroyBlockedFunds(vault);
        vm.stopPrank();
        assertEq(USDT0.balanceOf(vault), 0);
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        escrow.claim(a, sa);
    }

    /// A blocked recipient is still paid, so the page and the maker check `isBlocked` before locking.
    function test_fork_a_blocked_recipient_is_still_paid() public {
        (bytes32 a, bytes32 sa) = makerLock(9);
        vm.prank(TETHER_SAFE);
        USDT0.addToBlockedList(taker);
        uint256 before = USDT0.balanceOf(taker);
        escrow.claim(a, sa);
        assertEq(USDT0.balanceOf(taker), before + 100e6);
        assertTrue(USDT0.isBlocked(taker));
    }
}
