// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Test} from "forge-std/Test.sol";
import {Escrow} from "../src/Escrow.sol";
import {SwapVault} from "../src/SwapVault.sol";
import {TokenCall} from "../src/TokenCall.sol";
import {MockUSDT, FeeToken, FalseToken, NoReturnToken, ReentrantToken, Receiver} from "./Mocks.sol";

contract EscrowTest is Test {
    uint256 constant N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;
    uint256 constant WINDOW_BLOCKS = 1_209_600; // 14 days at 1 s
    uint256 constant EXT_BLOCKS = 604_800; // 7 days at 1 s

    MockUSDT token;
    Escrow escrow;
    address taker = makeAddr("taker");
    address maker = makeAddr("maker");
    address stranger = makeAddr("stranger");

    struct Vector {
        bytes32 t;
        bytes32 s;
        address lockAddress;
    }

    Vector[] vectors;

    function setUp() public {
        vm.warp(1_800_000_000);
        vm.roll(50_000_000);
        token = new MockUSDT();
        escrow = new Escrow(address(token), WINDOW_BLOCKS, EXT_BLOCKS);
        while (true) {
            string memory line = vm.readLine("test/vectors.txt");
            if (bytes(line).length == 0) break;
            string[] memory f = vm.split(line, " ");
            vectors.push(
                Vector(
                    vm.parseBytes32(string.concat("0x", f[0])),
                    vm.parseBytes32(string.concat("0x", f[1])),
                    vm.parseAddress(string.concat("0x", f[3]))
                )
            );
        }
        for (uint256 i = 0; i < 3; i++) {
            address who = [taker, maker, stranger][i];
            token.mint(who, 1_000_000e6);
            vm.prank(who);
            token.approve(address(escrow), type(uint256).max);
            vm.deal(who, 100 ether);
        }
    }

    // ---- Helpers ----

    function v0() internal view returns (Vector memory) {
        return vectors[3];
    }

    function lockFrom(address from, address recipient, address lockAddress, bool longLeg) internal returns (bytes32) {
        vm.prank(from);
        return escrow.lock(
            lockAddress,
            recipient,
            100e6,
            uint64(block.timestamp + 3 hours),
            uint64(block.number + 7200),
            longLeg,
            address(0)
        );
    }

    function state(bytes32 id) internal view returns (Escrow.State s) {
        (,, s,,,,,,,,) = escrow.locks(id);
    }

    function deadlines(bytes32 id) internal view returns (uint64 t, uint64 b) {
        (, t,,,,, b,,,,) = escrow.locks(id);
    }

    // ---- The secret check ----

    function test_shared_vectors_reveal() public view {
        assertEq(vectors.length, 8);
        // t = 1 and t = 2: the well-known addresses of private keys 1 and 2.
        assertEq(vectors[0].lockAddress, 0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf);
        assertEq(vectors[1].lockAddress, 0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF);
        for (uint256 i = 0; i < vectors.length; i++) {
            Vector memory v = vectors[i];
            assertEq(
                uint256(v.s),
                mulmod(uint256(v.t), 0x79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798, N)
            );
            assertTrue(escrow.reveals(v.s, v.lockAddress));
            // The private key's own address is address(T), so Foundry's key derivation agrees too.
            assertEq(vm.addr(uint256(v.t)), v.lockAddress);
        }
    }

    function test_wrong_values_do_not_reveal() public view {
        Vector memory v = v0();
        assertFalse(escrow.reveals(bytes32(uint256(v.s) ^ 1), v.lockAddress));
        assertFalse(escrow.reveals(bytes32(0), v.lockAddress));
        assertFalse(escrow.reveals(bytes32(N), v.lockAddress));
        assertFalse(escrow.reveals(bytes32(N + 1), v.lockAddress));
        assertFalse(escrow.reveals(bytes32(type(uint256).max), v.lockAddress));
        // n − s recovers −T, the point v = 28 would give: never accepted for T.
        assertFalse(escrow.reveals(bytes32(N - uint256(v.s)), v.lockAddress));
        // Another swap's secret.
        assertFalse(escrow.reveals(vectors[4].s, v.lockAddress));
    }

    function test_zero_lock_address_never_reveals() public view {
        assertFalse(escrow.reveals(bytes32(0), address(0)));
        assertFalse(escrow.reveals(bytes32(N), address(0)));
    }

    function testFuzz_only_the_secret_reveals(uint256 s) public view {
        Vector memory v = v0();
        vm.assume(s != uint256(v.s));
        assertFalse(escrow.reveals(bytes32(s), v.lockAddress));
    }

    function testFuzz_any_secret_reveals_its_own_address(uint256 t) public view {
        t = bound(t, 1, N - 1);
        uint256 s = mulmod(t, 0x79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798, N);
        assertTrue(escrow.reveals(bytes32(s), vm.addr(t)));
    }

    // ---- Lock checks ----

    function test_lock_refuses_bad_fields() public {
        address a = v0().lockAddress;
        uint64 t = uint64(block.timestamp + 1 hours);
        uint64 b = uint64(block.number + 100);
        vm.startPrank(taker);
        vm.expectRevert(Escrow.BadLock.selector);
        escrow.lock(address(0), maker, 1, t, b, false, address(0));
        vm.expectRevert(Escrow.BadLock.selector);
        escrow.lock(a, address(0), 1, t, b, false, address(0));
        vm.expectRevert(Escrow.BadLock.selector);
        escrow.lock(a, taker, 1, t, b, false, address(0));
        vm.expectRevert(Escrow.BadLock.selector);
        escrow.lock(a, maker, 0, t, b, false, address(0));
        vm.expectRevert(Escrow.BadLock.selector);
        escrow.lock(a, maker, 1, uint64(block.timestamp), b, false, address(0));
        vm.expectRevert(Escrow.BadLock.selector);
        escrow.lock(a, maker, 1, uint64(block.timestamp + 14 days + 1), b, false, address(0));
        vm.expectRevert(Escrow.BadLock.selector);
        escrow.lock(a, maker, 1, t, uint64(block.number), false, address(0));
        vm.expectRevert(Escrow.BadLock.selector);
        escrow.lock(a, maker, 1, t, uint64(block.number + WINDOW_BLOCKS + 1), false, address(0));
        vm.expectRevert(Escrow.BadLock.selector);
        escrow.lock{value: 1}(a, maker, 1, t, b, false, address(0));
        // The limits themselves are allowed.
        escrow.lock(
            a, maker, 1, uint64(block.timestamp + 14 days), uint64(block.number + WINDOW_BLOCKS), false, address(0)
        );
        vm.stopPrank();
    }

    function test_lock_moves_the_coins_into_their_own_vault() public {
        Vector memory v = v0();
        bytes32 id = lockFrom(taker, maker, v.lockAddress, true);
        assertEq(id, escrow.idOf(taker, v.lockAddress));
        address vault = escrow.vaultOf(id);
        assertEq(token.balanceOf(vault), 100e6);
        assertEq(token.balanceOf(address(escrow)), 0);
        assertGt(vault.code.length, 0);
        assertEq(uint256(state(id)), uint256(Escrow.State.Open));
    }

    function test_an_id_is_used_once_even_after_settling() public {
        Vector memory v = v0();
        bytes32 id = lockFrom(taker, maker, v.lockAddress, false);
        escrow.claim(id, v.s);
        vm.expectRevert(Escrow.AlreadyUsed.selector);
        lockFrom(taker, maker, v.lockAddress, false);
    }

    function test_someone_else_locking_the_same_point_gets_another_id() public {
        Vector memory v = v0();
        bytes32 mine = lockFrom(taker, maker, v.lockAddress, true);
        bytes32 theirs = lockFrom(stranger, maker, v.lockAddress, true);
        assertTrue(mine != theirs);
        assertTrue(escrow.vaultOf(mine) != escrow.vaultOf(theirs));
        vm.prank(maker);
        escrow.release(theirs);
        assertEq(uint256(state(mine)), uint256(Escrow.State.Open));
        assertEq(token.balanceOf(escrow.vaultOf(mine)), 100e6);
    }

    function test_a_donation_to_the_vault_address_does_not_break_the_lock() public {
        Vector memory v = v0();
        bytes32 id = escrow.idOf(maker, v.lockAddress);
        address vault = escrow.vaultOf(id);
        vm.prank(stranger);
        token.transfer(vault, 5e6);
        lockFrom(maker, taker, v.lockAddress, false);
        uint256 before = token.balanceOf(taker);
        escrow.claim(id, v.s);
        // Only the lock's own amount moves.
        assertEq(token.balanceOf(taker), before + 100e6);
        assertEq(token.balanceOf(vault), 5e6);
    }

    // ---- Claim ----

    function test_anyone_may_claim_and_only_the_recipient_is_paid() public {
        Vector memory v = v0();
        bytes32 id = lockFrom(maker, taker, v.lockAddress, false);
        uint256 before = token.balanceOf(taker);
        vm.prank(stranger);
        vm.expectEmit(address(escrow));
        emit Escrow.Claimed(id, v.s);
        escrow.claim(id, v.s);
        assertEq(token.balanceOf(taker), before + 100e6);
        assertEq(token.balanceOf(escrow.vaultOf(id)), 0);
        (,,,,,,,,,, bytes32 revealed) = escrow.locks(id);
        assertEq(revealed, v.s);
        assertEq(uint256(state(id)), uint256(Escrow.State.Claimed));
    }

    function test_claim_needs_the_secret_and_works_once() public {
        Vector memory v = v0();
        bytes32 id = lockFrom(maker, taker, v.lockAddress, false);
        vm.expectRevert(Escrow.WrongSecret.selector);
        escrow.claim(id, vectors[4].s);
        vm.expectRevert(Escrow.WrongSecret.selector);
        escrow.claim(id, bytes32(0));
        escrow.claim(id, v.s);
        vm.expectRevert(Escrow.NotOpen.selector);
        escrow.claim(id, v.s);
        vm.expectRevert(Escrow.NotOpen.selector);
        escrow.refund(id);
        vm.prank(taker);
        vm.expectRevert(Escrow.NotOpen.selector);
        escrow.release(id);
    }

    function test_claim_of_an_unknown_id() public {
        vm.expectRevert(Escrow.NotOpen.selector);
        escrow.claim(keccak256("nothing"), v0().s);
    }

    // ---- The clock rule ----

    /// Every combination of the two clocks around their deadlines, for both legs: exactly one of claim and refund is open,
    /// and claim follows the rule of docs/design.md section 2.4.
    function test_clock_rule_grid() public {
        for (uint256 leg = 0; leg < 2; leg++) {
            Vector memory v = vectors[2 + leg];
            uint64 dt = uint64(block.timestamp + 1000);
            uint64 db = uint64(block.number + 500);
            vm.prank(maker);
            bytes32 id = escrow.lock(v.lockAddress, taker, 1e6, dt, db, leg == 1, address(0));
            uint256 t0 = block.timestamp;
            uint256 b0 = block.number;
            int256[4] memory offsets = [int256(-1), 0, 1, 100000];
            for (uint256 i = 0; i < 4; i++) {
                for (uint256 j = 0; j < 4; j++) {
                    uint256 t = uint256(int256(uint256(dt)) + offsets[i]);
                    uint256 b = uint256(int256(uint256(db)) + offsets[j]);
                    vm.warp(t);
                    vm.roll(b);
                    bool timeOpen = t < dt;
                    bool blocksOpen = b < db;
                    bool expected = leg == 1 ? (timeOpen || blocksOpen) : (timeOpen && blocksOpen);
                    assertEq(escrow.canClaim(id), expected, "canClaim");
                    assertEq(escrow.canRefund(id), !expected, "canRefund");
                }
            }
            vm.warp(t0);
            vm.roll(b0);
        }
    }

    /// A Polygon halt: time jumps, blocks don't. The short leg's claim closes; the long leg's stays open.
    function test_halt_closes_the_short_leg_and_keeps_the_long_leg() public {
        bytes32 shortId = lockFrom(maker, taker, vectors[2].lockAddress, false);
        bytes32 longId = lockFrom(taker, maker, vectors[3].lockAddress, true);
        vm.warp(block.timestamp + 4 hours);
        vm.roll(block.number + 10);
        assertFalse(escrow.canClaim(shortId));
        vm.expectRevert(Escrow.ClaimClosed.selector);
        escrow.claim(shortId, vectors[2].s);
        escrow.refund(shortId);
        assertTrue(escrow.canClaim(longId));
        vm.expectRevert(Escrow.RefundNotOpen.selector);
        escrow.refund(longId);
        escrow.claim(longId, vectors[3].s);
    }

    /// A slow clock: blocks go on, time lags. The short leg closes on its block deadline.
    function test_slow_clock_closes_the_short_leg_on_blocks() public {
        bytes32 shortId = lockFrom(maker, taker, vectors[2].lockAddress, false);
        vm.warp(block.timestamp + 1 hours);
        vm.roll(block.number + 7200);
        assertFalse(escrow.canClaim(shortId));
        escrow.refund(shortId);
    }

    // ---- Refund and release ----

    function test_refund_pays_the_initiator_and_anyone_may_send_it() public {
        bytes32 id = lockFrom(maker, taker, v0().lockAddress, false);
        vm.expectRevert(Escrow.RefundNotOpen.selector);
        escrow.refund(id);
        vm.warp(block.timestamp + 3 hours);
        uint256 before = token.balanceOf(maker);
        vm.prank(stranger);
        escrow.refund(id);
        assertEq(token.balanceOf(maker), before + 100e6);
        assertEq(uint256(state(id)), uint256(Escrow.State.Refunded));
        vm.expectRevert(Escrow.NotOpen.selector);
        escrow.refund(id);
        vm.expectRevert(Escrow.NotOpen.selector);
        escrow.claim(id, v0().s);
    }

    function test_release_only_by_the_recipient() public {
        bytes32 id = lockFrom(taker, maker, v0().lockAddress, true);
        vm.prank(taker);
        vm.expectRevert(Escrow.NotRecipient.selector);
        escrow.release(id);
        vm.prank(stranger);
        vm.expectRevert(Escrow.NotRecipient.selector);
        escrow.release(id);
        uint256 before = token.balanceOf(taker);
        vm.prank(maker);
        escrow.release(id);
        assertEq(token.balanceOf(taker), before + 100e6);
        vm.prank(maker);
        vm.expectRevert(Escrow.NotOpen.selector);
        escrow.release(id);
    }

    // ---- Extend ----

    function test_extend_rules() public {
        bytes32 longId = lockFrom(taker, maker, vectors[3].lockAddress, true);
        bytes32 shortId = lockFrom(maker, taker, vectors[2].lockAddress, false);
        (uint64 t, uint64 b) = deadlines(longId);
        // Not the initiator, not a stranger.
        vm.prank(taker);
        vm.expectRevert(Escrow.NotRecipient.selector);
        escrow.extend(longId, t + 1, b + 1);
        // Never a short leg.
        vm.prank(taker);
        vm.expectRevert(Escrow.ExtensionNotAllowed.selector);
        escrow.extend(shortId, t + 1, b + 1);
        vm.startPrank(maker);
        // Each deadline strictly later, and no more than the limits.
        vm.expectRevert(Escrow.ExtensionNotAllowed.selector);
        escrow.extend(longId, t, b + 1);
        vm.expectRevert(Escrow.ExtensionNotAllowed.selector);
        escrow.extend(longId, t + 1, b);
        vm.expectRevert(Escrow.ExtensionNotAllowed.selector);
        escrow.extend(longId, t + 7 days + 1, b + 1);
        vm.expectRevert(Escrow.ExtensionNotAllowed.selector);
        escrow.extend(longId, t + 1, uint64(b + EXT_BLOCKS + 1));
        escrow.extend(longId, t + 7 days, uint64(b + EXT_BLOCKS));
        (uint64 t2, uint64 b2) = deadlines(longId);
        assertEq(t2, t + 7 days);
        assertEq(b2, b + EXT_BLOCKS);
        // Once only.
        vm.expectRevert(Escrow.ExtensionNotAllowed.selector);
        escrow.extend(longId, t2 + 1, b2 + 1);
        vm.stopPrank();
    }

    function test_extend_not_after_the_claim_has_closed() public {
        bytes32 id = lockFrom(taker, maker, v0().lockAddress, true);
        (uint64 t, uint64 b) = deadlines(id);
        vm.warp(t);
        vm.roll(b);
        vm.prank(maker);
        vm.expectRevert(Escrow.ExtensionNotAllowed.selector);
        escrow.extend(id, t + 1, b + 1);
    }

    // ---- Gas drop ----

    function test_gas_drop_is_forwarded_at_lock() public {
        address gasTo = makeAddr("gasTo");
        vm.prank(maker);
        bytes32 id = escrow.lock{value: 0.2 ether}(
            v0().lockAddress, taker, 1e6, uint64(block.timestamp + 1 hours), uint64(block.number + 100), false, gasTo
        );
        assertEq(gasTo.balance, 0.2 ether);
        assertEq(address(escrow).balance, 0);
        (,,,,,,,, uint96 gasDrop,,) = escrow.locks(id);
        assertEq(gasDrop, 0.2 ether);
    }

    function test_a_refused_gas_drop_undoes_the_lock() public {
        Receiver r = new Receiver();
        r.setRefuse(true);
        vm.prank(maker);
        vm.expectRevert(Escrow.GasDropFailed.selector);
        escrow.lock{value: 1}(
            v0().lockAddress,
            taker,
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 100),
            false,
            address(r)
        );
        assertEq(uint256(state(escrow.idOf(maker, v0().lockAddress))), uint256(Escrow.State.None));
    }

    function test_a_gas_drop_receiver_cannot_reenter() public {
        Receiver r = new Receiver();
        Vector memory v = v0();
        bytes32 id = escrow.idOf(maker, v.lockAddress);
        r.arm(address(escrow), abi.encodeCall(Escrow.claim, (id, v.s)));
        vm.prank(maker);
        escrow.lock{value: 1}(
            v.lockAddress, taker, 1e6, uint64(block.timestamp + 1 hours), uint64(block.number + 100), false, address(r)
        );
        assertTrue(r.reentryFailed());
        assertEq(uint256(state(id)), uint256(Escrow.State.Open));
    }

    // ---- Vaults ----

    function test_only_the_escrow_can_pay_from_a_vault() public {
        bytes32 id = lockFrom(taker, maker, v0().lockAddress, true);
        SwapVault vault = SwapVault(escrow.vaultOf(id));
        vm.prank(stranger);
        vm.expectRevert(SwapVault.OnlyEscrow.selector);
        vault.pay(stranger, 1);
        // The implementation itself holds nothing and obeys the same rule.
        SwapVault implementation = SwapVault(escrow.vaultImplementation());
        vm.prank(stranger);
        vm.expectRevert(SwapVault.OnlyEscrow.selector);
        implementation.pay(stranger, 0);
    }

    // ---- Tether's blocklist ----

    function test_a_blocked_escrow_stops_new_locks_but_not_open_swaps() public {
        Vector memory v = v0();
        bytes32 id = lockFrom(maker, taker, v.lockAddress, false);
        token.block_(address(escrow), true);
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        lockFrom(maker, taker, vectors[4].lockAddress, false);
        escrow.claim(id, v.s);
        assertEq(uint256(state(id)), uint256(Escrow.State.Claimed));
    }

    function test_a_blocked_vault_stops_that_swap_only() public {
        bytes32 a = lockFrom(maker, taker, vectors[3].lockAddress, false);
        bytes32 b = lockFrom(maker, taker, vectors[4].lockAddress, false);
        token.block_(escrow.vaultOf(a), true);
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        escrow.claim(a, vectors[3].s);
        assertEq(uint256(state(a)), uint256(Escrow.State.Open));
        escrow.claim(b, vectors[4].s);
        // Unblocked later, the swap completes.
        token.block_(escrow.vaultOf(a), false);
        escrow.claim(a, vectors[3].s);
    }

    function test_a_blocked_recipient_is_still_paid() public {
        bytes32 id = lockFrom(maker, taker, v0().lockAddress, false);
        token.block_(taker, true);
        escrow.claim(id, v0().s);
        assertEq(uint256(state(id)), uint256(Escrow.State.Claimed));
    }

    function test_a_blocked_initiator_cannot_lock() public {
        token.block_(taker, true);
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        lockFrom(taker, maker, v0().lockAddress, true);
    }

    // ---- Unusual tokens ----

    function test_fee_on_transfer_token_is_refused() public {
        FeeToken fee = new FeeToken();
        Escrow e = new Escrow(address(fee), WINDOW_BLOCKS, EXT_BLOCKS);
        fee.mint(taker, 10e6);
        vm.startPrank(taker);
        fee.approve(address(e), type(uint256).max);
        vm.expectRevert(Escrow.WrongAmount.selector);
        e.lock(
            v0().lockAddress,
            maker,
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 100),
            true,
            address(0)
        );
        vm.stopPrank();
    }

    function test_a_token_returning_false_fails_loudly() public {
        FalseToken f = new FalseToken();
        Escrow e = new Escrow(address(f), WINDOW_BLOCKS, EXT_BLOCKS);
        f.mint(maker, 10e6);
        vm.startPrank(maker);
        f.approve(address(e), type(uint256).max);
        bytes32 id = e.lock(
            v0().lockAddress,
            taker,
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 100),
            false,
            address(0)
        );
        vm.stopPrank();
        f.setFail(true);
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        e.claim(id, v0().s);
        (,, Escrow.State s,,,,,,,,) = e.locks(id);
        assertEq(uint256(s), uint256(Escrow.State.Open));
    }

    function test_a_token_reverting_with_no_data_fails_loudly() public {
        FalseToken f = new FalseToken();
        Escrow e = new Escrow(address(f), WINDOW_BLOCKS, EXT_BLOCKS);
        f.mint(maker, 10e6);
        vm.startPrank(maker);
        f.approve(address(e), type(uint256).max);
        bytes32 id = e.lock(
            v0().lockAddress,
            taker,
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 100),
            false,
            address(0)
        );
        vm.stopPrank();
        f.setRevertEmpty(true);
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        e.claim(id, v0().s);
        (,, Escrow.State s,,,,,,,,) = e.locks(id);
        assertEq(uint256(s), uint256(Escrow.State.Open));
    }

    function test_a_token_returning_nothing_works() public {
        NoReturnToken nr = new NoReturnToken();
        Escrow e = new Escrow(address(nr), WINDOW_BLOCKS, EXT_BLOCKS);
        nr.mint(maker, 10e6);
        vm.startPrank(maker);
        nr.approve(address(e), type(uint256).max);
        bytes32 id = e.lock(
            v0().lockAddress,
            taker,
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 100),
            false,
            address(0)
        );
        vm.stopPrank();
        e.claim(id, v0().s);
        assertEq(nr.balanceOf(taker), 1e6);
    }

    function test_a_reentrant_token_cannot_claim_or_refund_twice() public {
        ReentrantToken r = new ReentrantToken();
        Escrow e = new Escrow(address(r), WINDOW_BLOCKS, EXT_BLOCKS);
        r.mint(maker, 10e6);
        vm.startPrank(maker);
        r.approve(address(e), type(uint256).max);
        bytes32 id = e.lock(
            v0().lockAddress,
            taker,
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 100),
            false,
            address(0)
        );
        vm.stopPrank();
        // During the claim's payout, the token tries to claim again and to refund.
        r.arm(address(e), abi.encodeCall(Escrow.claim, (id, v0().s)));
        e.claim(id, v0().s);
        assertTrue(r.reverted());
        assertEq(r.balanceOf(taker), 1e6);
        assertEq(r.balanceOf(e.vaultOf(id)), 0);
    }

    function test_constructor_refuses_bad_parameters() public {
        vm.expectRevert(Escrow.BadLock.selector);
        new Escrow(address(0xdead), WINDOW_BLOCKS, EXT_BLOCKS);
        vm.expectRevert(Escrow.BadLock.selector);
        new Escrow(address(token), 0, EXT_BLOCKS);
        vm.expectRevert(Escrow.BadLock.selector);
        new Escrow(address(token), WINDOW_BLOCKS, 0);
        vm.expectRevert(Escrow.BadLock.selector);
        new Escrow(address(token), uint256(type(uint64).max) + 1, EXT_BLOCKS);
    }

    // ---- lockWithPermit ----

    function permitSig(uint256 key, address owner, uint256 value, uint256 deadline)
        internal
        view
        returns (uint8 v, bytes32 r, bytes32 s)
    {
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                token.DOMAIN_SEPARATOR(),
                keccak256(
                    abi.encode(token.PERMIT_TYPEHASH(), owner, address(escrow), value, token.nonces(owner), deadline)
                )
            )
        );
        return vm.sign(key, digest);
    }

    function test_lock_with_permit() public {
        (address owner, uint256 key) = makeAddrAndKey("permit owner");
        token.mint(owner, 10e6);
        uint256 deadline = block.timestamp + 10 minutes;
        (uint8 v, bytes32 r, bytes32 s) = permitSig(key, owner, 1e6, deadline);
        vm.prank(owner);
        bytes32 id = escrow.lockWithPermit(
            v0().lockAddress,
            maker,
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 100),
            true,
            address(0),
            deadline,
            v,
            r,
            s
        );
        assertEq(token.balanceOf(escrow.vaultOf(id)), 1e6);
    }

    function test_a_front_run_permit_does_not_block_the_lock() public {
        (address owner, uint256 key) = makeAddrAndKey("permit owner");
        token.mint(owner, 10e6);
        uint256 deadline = block.timestamp + 10 minutes;
        (uint8 v, bytes32 r, bytes32 s) = permitSig(key, owner, 1e6, deadline);
        vm.prank(stranger);
        token.permit(owner, address(escrow), 1e6, deadline, v, r, s);
        vm.prank(owner);
        bytes32 id = escrow.lockWithPermit(
            v0().lockAddress,
            maker,
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 100),
            true,
            address(0),
            deadline,
            v,
            r,
            s
        );
        assertEq(token.balanceOf(escrow.vaultOf(id)), 1e6);
    }

    function test_permit_that_never_expires_is_refused() public {
        vm.prank(taker);
        vm.expectRevert(Escrow.BadLock.selector);
        escrow.lockWithPermit(
            v0().lockAddress,
            maker,
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 100),
            true,
            address(0),
            0,
            27,
            bytes32(0),
            bytes32(0)
        );
    }

    function test_a_bad_permit_without_allowance_fails() public {
        (address owner,) = makeAddrAndKey("permit owner");
        token.mint(owner, 10e6);
        vm.prank(owner);
        vm.expectRevert(TokenCall.TokenCallFailed.selector);
        escrow.lockWithPermit(
            v0().lockAddress,
            maker,
            1e6,
            uint64(block.timestamp + 1 hours),
            uint64(block.number + 100),
            true,
            address(0),
            block.timestamp + 1,
            27,
            bytes32(uint256(1)),
            bytes32(uint256(2))
        );
    }

    // ---- Gas, recorded ----

    function test_gas_report_shapes() public {
        Vector memory v = v0();
        vm.prank(maker);
        uint256 g = gasleft();
        bytes32 id = escrow.lock(
            v.lockAddress,
            taker,
            100e6,
            uint64(block.timestamp + 3 hours),
            uint64(block.number + 7200),
            false,
            address(0)
        );
        emit log_named_uint("lock (execution only)", g - gasleft());
        g = gasleft();
        escrow.claim(id, v.s);
        emit log_named_uint("claim (execution only)", g - gasleft());
    }
}
