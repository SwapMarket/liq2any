// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Test} from "forge-std/Test.sol";
import {Escrow} from "../src/Escrow.sol";
import {MakerVault, IEscrow} from "../src/MakerVault.sol";
import {TokenCall} from "../src/TokenCall.sol";
import {MockUSDT, NoReturnToken, Receiver} from "./Mocks.sol";

/// The maker vault of docs/design.md section 2.6 (decision P9): what each key can and can't do, the cap, and the swaps run
/// through it in both directions.
contract MakerVaultTest is Test {
    uint256 constant WINDOW_BLOCKS = 1_209_600;
    uint256 constant EXT_BLOCKS = 604_800;
    uint64 constant WINDOW = 1 days;
    uint128 constant CAP = 1000e6;

    MockUSDT token;
    Escrow escrow;
    MakerVault vault;
    address owner = makeAddr("owner");
    address operator = makeAddr("operator");
    address taker = makeAddr("taker");
    address thief = makeAddr("thief");

    bytes32[] secrets;
    address[] lockAddresses;

    function setUp() public {
        vm.warp(1_800_000_000);
        vm.roll(50_000_000);
        token = new MockUSDT();
        escrow = new Escrow(address(token), WINDOW_BLOCKS, EXT_BLOCKS);
        vault = new MakerVault(IEscrow(address(escrow)), owner, operator, WINDOW, CAP);
        token.mint(address(vault), 10_000e6);
        token.mint(taker, 10_000e6);
        vm.prank(taker);
        token.approve(address(escrow), type(uint256).max);
        vm.deal(operator, 10 ether);
        while (true) {
            string memory line = vm.readLine("test/vectors.txt");
            if (bytes(line).length == 0) break;
            string[] memory f = vm.split(line, " ");
            secrets.push(vm.parseBytes32(string.concat("0x", f[1])));
            lockAddresses.push(vm.parseAddress(string.concat("0x", f[3])));
        }
    }

    // ---- Helpers ----

    function vaultLock(uint256 v, uint128 amount) internal returns (bytes32) {
        vm.prank(operator);
        return vault.lock(
            lockAddresses[v], taker, amount, uint64(block.timestamp + 1 hours), uint64(block.number + 3600), address(0)
        );
    }

    function lockOf(bytes32 id)
        internal
        view
        returns (address initiator, Escrow.State state, bool longLeg, address recipient, uint128 amount)
    {
        (initiator,, state, longLeg,, recipient,,,, amount,) = escrow.locks(id);
    }

    // ---- Deployment ----

    function test_constructor_refuses_bad_settings() public {
        vm.expectRevert(MakerVault.BadSetting.selector);
        new MakerVault(IEscrow(makeAddr("no code")), owner, operator, WINDOW, CAP);
        vm.expectRevert(MakerVault.BadSetting.selector);
        new MakerVault(IEscrow(address(escrow)), address(0), operator, WINDOW, CAP);
        vm.expectRevert(MakerVault.BadSetting.selector);
        new MakerVault(IEscrow(address(escrow)), owner, operator, 0, CAP);
        assertEq(vault.token(), address(token), "the escrow's own token");
        assertEq(vault.owner(), owner);
        assertEq(vault.operator(), operator);
    }

    // ---- L→P: the maker's lock comes from the vault ----

    function test_maker_lock_claimed_by_taker() public {
        bytes32 id = vaultLock(0, 100e6);
        assertEq(id, escrow.idOf(address(vault), lockAddresses[0]), "the vault is the initiator");
        (address initiator, Escrow.State state, bool longLeg, address recipient, uint128 amount) = lockOf(id);
        assertEq(initiator, address(vault));
        assertTrue(state == Escrow.State.Open);
        assertFalse(longLeg, "the maker's lock is the short leg");
        assertEq(recipient, taker);
        assertEq(amount, 100e6);
        assertEq(token.balanceOf(address(vault)), 9_900e6);
        assertEq(token.allowance(address(vault), address(escrow)), 0, "no allowance left over");
        uint256 before = token.balanceOf(taker);
        escrow.claim(id, secrets[0]);
        assertEq(token.balanceOf(taker) - before, 100e6);
    }

    function test_refund_returns_to_the_vault() public {
        bytes32 id = vaultLock(0, 100e6);
        vm.warp(block.timestamp + 1 hours);
        escrow.refund(id);
        assertEq(token.balanceOf(address(vault)), 10_000e6);
        assertEq(token.balanceOf(operator), 0, "nothing reaches the hot key");
    }

    function test_gas_drop_passes_through() public {
        uint256 before = taker.balance;
        vm.prank(operator);
        bytes32 id = vault.lock{value: 0.01 ether}(
            lockAddresses[0], taker, 100e6, uint64(block.timestamp + 1 hours), uint64(block.number + 3600), taker
        );
        assertEq(taker.balance - before, 0.01 ether);
        assertEq(address(vault).balance, 0);
        (,,,,,,,, uint96 gasDrop,,) = escrow.locks(id);
        assertEq(gasDrop, 0.01 ether);
    }

    function test_escrow_rules_still_apply() public {
        vm.startPrank(operator);
        vm.expectRevert(Escrow.BadLock.selector);
        vault.lock(
            lockAddresses[0],
            address(vault),
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 9),
            address(0)
        );
        vm.expectRevert(Escrow.BadLock.selector);
        vault.lock(lockAddresses[0], taker, 1e6, uint64(block.timestamp), uint64(block.number + 9), address(0));
        vm.stopPrank();
        vaultLock(0, 1e6);
        vm.expectRevert(Escrow.AlreadyUsed.selector);
        vaultLock(0, 1e6);
        assertEq(vault.windowUsed(), 1e6, "a refused lock uses none of the cap");
    }

    function test_a_token_that_returns_nothing() public {
        NoReturnToken quiet = new NoReturnToken();
        Escrow e = new Escrow(address(quiet), WINDOW_BLOCKS, EXT_BLOCKS);
        MakerVault v = new MakerVault(IEscrow(address(e)), owner, operator, WINDOW, CAP);
        quiet.mint(address(v), 100e6);
        vm.prank(operator);
        v.lock(lockAddresses[0], taker, 100e6, uint64(block.timestamp + 1 hours), uint64(block.number + 9), address(0));
        assertEq(quiet.balanceOf(address(v)), 0);
    }

    // ---- P→L: the taker locks to the vault ----

    function takerLock(uint256 v) internal returns (bytes32) {
        vm.prank(taker);
        return escrow.lock(
            lockAddresses[v],
            address(vault),
            50e6,
            uint64(block.timestamp + 6 hours),
            uint64(block.number + 21600),
            true,
            address(0)
        );
    }

    function test_taker_lock_claimed_into_the_vault() public {
        bytes32 id = takerLock(1);
        escrow.claim(id, secrets[1]);
        assertEq(token.balanceOf(address(vault)), 10_050e6, "claims pay the vault, not the hot key");
        assertEq(vault.windowUsed(), 0, "coins coming in use none of the cap");
    }

    function test_operator_releases_and_extends_through_the_vault() public {
        bytes32 id = takerLock(1);
        (, uint64 t,,,,, uint64 b,,,,) = escrow.locks(id);
        vm.prank(operator);
        vault.extend(id, t + 1 hours, b + 3600);
        (, uint64 t2,,,,, uint64 b2,,,,) = escrow.locks(id);
        assertEq(t2, t + 1 hours);
        assertEq(b2, b + 3600);

        uint256 before = token.balanceOf(taker);
        vm.prank(operator);
        vault.release(id);
        assertEq(token.balanceOf(taker) - before, 50e6, "released back to the taker");
    }

    // ---- Who may do what ----

    function test_only_the_operator_locks_releases_and_extends() public {
        bytes32 id = takerLock(1);
        address[2] memory others = [owner, thief];
        for (uint256 i = 0; i < 2; i++) {
            vm.startPrank(others[i]);
            vm.expectRevert(MakerVault.NotOperator.selector);
            vault.lock(
                lockAddresses[0], others[i], 1e6, uint64(block.timestamp + 1), uint64(block.number + 1), address(0)
            );
            vm.expectRevert(MakerVault.NotOperator.selector);
            vault.release(id);
            vm.expectRevert(MakerVault.NotOperator.selector);
            vault.extend(id, uint64(block.timestamp + 7 hours), uint64(block.number + 30000));
            vm.stopPrank();
        }
    }

    function test_only_the_owner_withdraws_and_sets_the_operator() public {
        address[2] memory others = [operator, thief];
        for (uint256 i = 0; i < 2; i++) {
            vm.startPrank(others[i]);
            vm.expectRevert(MakerVault.NotOwner.selector);
            vault.withdraw(others[i], 1);
            vm.expectRevert(MakerVault.NotOwner.selector);
            vault.setOperator(others[i], WINDOW, type(uint128).max);
            vm.stopPrank();
        }
        vm.startPrank(owner);
        vault.withdraw(owner, 4_000e6);
        vm.expectRevert(MakerVault.BadSetting.selector);
        vault.withdraw(address(0), 1);
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        vault.withdraw(owner, 6_000e6 + 1);
        vm.stopPrank();
        assertEq(token.balanceOf(owner), 4_000e6);
    }

    function test_rotating_the_operator_shuts_the_old_one_out() public {
        address fresh = makeAddr("fresh");
        vm.prank(owner);
        vault.setOperator(fresh, WINDOW, CAP);
        vm.expectRevert(MakerVault.NotOperator.selector);
        vaultLock(0, 1e6);
        vm.prank(fresh);
        vault.lock(
            lockAddresses[0], taker, 1e6, uint64(block.timestamp + 1 hours), uint64(block.number + 9), address(0)
        );

        vm.prank(owner);
        vault.setOperator(address(0), WINDOW, CAP);
        vm.prank(fresh);
        vm.expectRevert(MakerVault.NotOperator.selector);
        vault.lock(
            lockAddresses[1], taker, 1e6, uint64(block.timestamp + 1 hours), uint64(block.number + 9), address(0)
        );
        vm.prank(owner);
        vm.expectRevert(MakerVault.BadSetting.selector);
        vault.setOperator(fresh, 0, CAP);
    }

    // ---- The cap: what a stolen hot key can take ----

    function test_a_stolen_hot_key_takes_at_most_the_cap_per_window() public {
        // The thief locks to an address of its own, with a lock address whose secret it knows, and claims.
        vm.startPrank(operator);
        bytes32 id = vault.lock(
            lockAddresses[0], thief, CAP, uint64(block.timestamp + 1 hours), uint64(block.number + 9), address(0)
        );
        vm.expectRevert(MakerVault.OverCap.selector);
        vault.lock(lockAddresses[1], thief, 1, uint64(block.timestamp + 1 hours), uint64(block.number + 9), address(0));
        vm.stopPrank();
        escrow.claim(id, secrets[0]);
        assertEq(token.balanceOf(thief), CAP);

        // One second before the window ends, still nothing; at its end, one more cap.
        vm.warp(block.timestamp + WINDOW - 1);
        vm.expectRevert(MakerVault.OverCap.selector);
        vaultLock(1, 1);
        vm.warp(block.timestamp + 1);
        vaultLock(1, CAP);
        assertEq(vault.windowStart(), block.timestamp, "the next window starts at the first lock after the last");
    }

    function test_refunds_do_not_give_the_cap_back() public {
        bytes32 id = vaultLock(0, CAP);
        vm.warp(block.timestamp + 1 hours);
        escrow.refund(id);
        vm.expectRevert(MakerVault.OverCap.selector);
        vaultLock(1, 1);
    }

    function test_a_cap_lowered_below_use_refuses_cleanly() public {
        vaultLock(0, 600e6);
        vm.prank(owner);
        vault.setOperator(operator, WINDOW, 500e6);
        vm.expectRevert(MakerVault.OverCap.selector);
        vaultLock(1, 1);
        vm.prank(owner);
        vault.setOperator(operator, WINDOW, 0);
        vm.expectRevert(MakerVault.OverCap.selector);
        vaultLock(1, 1);
    }

    function test_a_gas_drop_receiver_cannot_lock_again() public {
        Receiver r = new Receiver();
        r.arm(
            address(vault),
            abi.encodeCall(
                MakerVault.lock,
                (
                    lockAddresses[1],
                    address(r),
                    1e6,
                    uint64(block.timestamp + 1 hours),
                    uint64(block.number + 9),
                    address(0)
                )
            )
        );
        vm.prank(operator);
        vault.lock{value: 1}(
            lockAddresses[0], address(r), 1e6, uint64(block.timestamp + 1 hours), uint64(block.number + 9), address(r)
        );
        assertTrue(r.reentryFailed());
        assertEq(vault.windowUsed(), 1e6);
    }

    /// Whatever the operator tries and whenever, a lock goes through exactly when this model of the window allows it, so what
    /// leaves the vault within one window is never more than the cap.
    function testFuzz_locks_follow_the_window(uint128[8] memory amounts, uint32[8] memory gaps) public {
        uint256 start;
        uint256 used;
        for (uint256 i = 0; i < 8; i++) {
            vm.warp(block.timestamp + (gaps[i] % (2 * WINDOW)));
            uint128 amount = uint128(bound(amounts[i], 1, CAP + 1));
            (uint256 s, uint256 u) = block.timestamp >= start + WINDOW ? (block.timestamp, 0) : (start, used);
            bool fits = u + amount <= CAP;
            if (!fits) vm.expectRevert(MakerVault.OverCap.selector);
            vm.prank(operator);
            vault.lock(
                lockAddresses[i], taker, amount, uint64(block.timestamp + 1 hours), uint64(block.number + 1), address(0)
            );
            if (fits) (start, used) = (s, u + amount);
            assertEq(vault.windowStart(), start);
            assertEq(vault.windowUsed(), used);
            assertLe(used, CAP);
        }
    }
}
