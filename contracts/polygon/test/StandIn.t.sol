// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Test} from "forge-std/Test.sol";
import {Escrow} from "../src/Escrow.sol";
import {TokenCall} from "../src/TokenCall.sol";
import {StandInUSDT0} from "../src/testnet/StandInUSDT0.sol";

/// The Amoy stand-in behaves like Polygon's USDT0 wherever the escrow, the maker or the page rely on it.
contract StandInTest is Test {
    uint256 constant GX = 0x79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798;
    uint256 constant N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;

    StandInUSDT0 token;
    Escrow escrow;
    address blocker = makeAddr("blocker");
    address alice;
    uint256 aliceKey;
    address bob = makeAddr("bob");

    function setUp() public {
        vm.chainId(80002);
        vm.prank(blocker);
        token = new StandInUSDT0();
        escrow = new Escrow(address(token), 1_209_600, 604_800);
        (alice, aliceKey) = makeAddrAndKey("alice");
        vm.prank(alice);
        token.faucet(1_000e6);
    }

    function test_faucet_limit() public {
        vm.prank(bob);
        vm.expectRevert("tUSDT0: over the faucet's limit");
        token.faucet(1_000e6 + 1);
        assertEq(token.totalSupply(), 1_000e6);
    }

    function test_salt_domain_like_polygon() public view {
        bytes32 expected = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,address verifyingContract,bytes32 salt)"),
                keccak256(bytes(token.name())),
                keccak256("1"),
                address(token),
                bytes32(uint256(80002))
            )
        );
        assertEq(token.DOMAIN_SEPARATOR(), expected);
        assertEq(token.decimals(), 6);
    }

    function test_permit_then_lock_through_the_escrow() public {
        uint256 deadline = block.timestamp + 600;
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                token.DOMAIN_SEPARATOR(),
                keccak256(
                    abi.encode(token.PERMIT_TYPEHASH(), alice, address(escrow), 10e6, token.nonces(alice), deadline)
                )
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(aliceKey, digest);
        uint256 t = 42;
        vm.prank(alice);
        bytes32 id = escrow.lockWithPermit(
            vm.addr(t),
            bob,
            10e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 3600),
            true,
            address(0),
            deadline,
            v,
            r,
            s
        );
        escrow.claim(id, bytes32(mulmod(t, GX, N)));
        assertEq(token.balanceOf(bob), 10e6);
    }

    function test_blocklist_rules_like_usdt0() public {
        vm.prank(bob);
        vm.expectRevert("USDT0: INSUFFICIENT_PERMISSIONS");
        token.addToBlockedList(alice);
        vm.prank(blocker);
        token.addToBlockedList(alice);
        // A blocked address can't send, but still receives.
        vm.prank(alice);
        vm.expectRevert("USDT0: from is blocked");
        token.transfer(bob, 1);
        vm.prank(bob);
        token.faucet(5e6);
        vm.prank(bob);
        token.transfer(alice, 5e6);
        assertEq(token.balanceOf(alice), 1_005e6);
        // A blocked caller can't use transferFrom.
        vm.prank(bob);
        token.approve(alice, 1);
        vm.prank(alice);
        vm.expectRevert("Blocked: msg.sender is blocked");
        token.transferFrom(bob, alice, 1);
        // Its funds can be destroyed.
        vm.prank(blocker);
        token.destroyBlockedFunds(alice);
        assertEq(token.balanceOf(alice), 0);
        assertEq(token.totalSupply(), 0);
    }

    function test_a_blocked_escrow_cannot_take_new_locks() public {
        vm.prank(blocker);
        token.addToBlockedList(address(escrow));
        vm.startPrank(alice);
        token.approve(address(escrow), 1e6);
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        escrow.lock(
            vm.addr(7), bob, 1e6, uint64(block.timestamp + 1 hours), uint64(block.number + 3600), true, address(0)
        );
        vm.stopPrank();
    }

    function test_no_transfers_to_the_token_or_to_zero() public {
        vm.startPrank(alice);
        vm.expectRevert("USDT0: transfer to the contract address");
        token.transfer(address(token), 1);
        vm.expectRevert("ERC20: transfer to the zero address");
        token.transfer(address(0), 1);
        vm.stopPrank();
    }
}
