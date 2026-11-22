// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Test} from "forge-std/Test.sol";
import {Escrow} from "../src/Escrow.sol";
import {MockUSDT} from "./Mocks.sol";

/// Random sequences of locks, claims, refunds, releases, extensions and clock moves, by three actors.
contract Handler is Test {
    uint256 constant N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;
    uint256 constant GX = 0x79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798;

    Escrow public escrow;
    MockUSDT public token;
    address[3] public actors;
    bytes32[] public ids;
    mapping(bytes32 => uint256) public secretOf;
    mapping(bytes32 => Escrow.State) public seen;
    uint256 public settledThenChanged;
    uint256 public claims;
    uint256 public refunds;
    uint256 public releases;
    uint256 public extends;

    constructor(Escrow escrow_, MockUSDT token_, address[3] memory actors_) {
        escrow = escrow_;
        token = token_;
        actors = actors_;
    }

    function idCount() external view returns (uint256) {
        return ids.length;
    }

    function lock(
        uint256 actorSeed,
        uint256 recipientSeed,
        uint256 t,
        uint96 amount,
        uint32 dt,
        uint32 db,
        bool longLeg
    ) external {
        address from = actors[actorSeed % 3];
        address to = actors[recipientSeed % 3];
        if (from == to) to = actors[(recipientSeed + 1) % 3];
        t = bound(t, 1, N - 1);
        amount = uint96(bound(amount, 1, 1_000e6));
        uint64 deadlineTime = uint64(block.timestamp + bound(dt, 1, 14 days));
        uint64 deadlineBlock = uint64(block.number + bound(db, 1, 1_209_600));
        vm.prank(from);
        try escrow.lock(vm.addr(t), to, amount, deadlineTime, deadlineBlock, longLeg, address(0)) returns (bytes32 id) {
            ids.push(id);
            secretOf[id] = t;
            seen[id] = Escrow.State.Open;
        } catch {}
    }

    function claim(uint256 i, address caller, bool wrong) external {
        if (ids.length == 0) return;
        bytes32 id = ids[i % ids.length];
        uint256 s = mulmod(secretOf[id], GX, N);
        if (wrong) s = s ^ 1;
        vm.prank(caller);
        try escrow.claim(id, bytes32(s)) {
            claims++;
        } catch {}
        _record(id);
    }

    function refund(uint256 i, address caller) external {
        if (ids.length == 0) return;
        bytes32 id = ids[i % ids.length];
        vm.prank(caller);
        try escrow.refund(id) {
            refunds++;
        } catch {}
        _record(id);
    }

    function release(uint256 i, uint256 actorSeed) external {
        if (ids.length == 0) return;
        bytes32 id = ids[i % ids.length];
        vm.prank(actors[actorSeed % 3]);
        try escrow.release(id) {
            releases++;
        } catch {}
        _record(id);
    }

    function extend(uint256 i, uint256 actorSeed, uint32 dt, uint32 db) external {
        if (ids.length == 0) return;
        bytes32 id = ids[i % ids.length];
        (, uint64 t,,,, address recipient, uint64 b,,,,) = escrow.locks(id);
        // Mostly the recipient, asking for a later deadline, so valid extensions happen; sometimes anyone, with anything.
        bool valid = actorSeed % 4 != 0;
        vm.prank(valid ? recipient : actors[actorSeed % 3]);
        try escrow.extend(
            id,
            uint64(t + (valid ? bound(dt, 1, 7 days) : bound(dt, 0, 8 days))),
            uint64(b + (valid ? bound(db, 1, 604_800) : bound(db, 0, 700_000)))
        ) {
            extends++;
        } catch {}
        _record(id);
    }

    function warp(uint32 seconds_) external {
        vm.warp(block.timestamp + bound(seconds_, 0, 10 days));
    }

    function roll(uint32 blocks) external {
        vm.roll(block.number + bound(blocks, 0, 500_000));
    }

    function _record(bytes32 id) private {
        (,, Escrow.State s,,,,,,,,) = escrow.locks(id);
        if (seen[id] != Escrow.State.Open && s != seen[id]) settledThenChanged++;
        seen[id] = s;
    }
}

contract EscrowInvariants is Test {
    Escrow escrow;
    MockUSDT token;
    Handler handler;
    address[3] actors;
    uint256 constant START = 1_000_000e6;

    function setUp() public {
        vm.warp(1_800_000_000);
        vm.roll(50_000_000);
        token = new MockUSDT();
        escrow = new Escrow(address(token), 1_209_600, 604_800);
        actors = [makeAddr("a"), makeAddr("b"), makeAddr("c")];
        for (uint256 i = 0; i < 3; i++) {
            token.mint(actors[i], START);
            vm.prank(actors[i]);
            token.approve(address(escrow), type(uint256).max);
        }
        handler = new Handler(escrow, token, actors);
        targetContract(address(handler));
    }

    /// No token is created or lost: the actors and the vaults of open locks hold everything.
    function invariant_tokens_are_conserved() public view {
        uint256 total;
        for (uint256 i = 0; i < 3; i++) {
            total += token.balanceOf(actors[i]);
        }
        uint256 n = handler.idCount();
        for (uint256 i = 0; i < n; i++) {
            total += token.balanceOf(escrow.vaultOf(handler.ids(i)));
        }
        assertEq(total, 3 * START);
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    /// An open lock's vault holds exactly its amount, and a settled lock's vault holds nothing.
    function invariant_each_vault_holds_its_lock() public view {
        uint256 n = handler.idCount();
        for (uint256 i = 0; i < n; i++) {
            bytes32 id = handler.ids(i);
            (,, Escrow.State s,,,,,,, uint128 amount,) = escrow.locks(id);
            assertEq(token.balanceOf(escrow.vaultOf(id)), s == Escrow.State.Open ? amount : 0);
        }
    }

    /// While open, exactly one of claim and refund is possible; once settled, neither.
    function invariant_claim_and_refund_are_exact_opposites() public view {
        uint256 n = handler.idCount();
        for (uint256 i = 0; i < n; i++) {
            bytes32 id = handler.ids(i);
            (,, Escrow.State s,,,,,,,,) = escrow.locks(id);
            if (s == Escrow.State.Open) assertTrue(escrow.canClaim(id) != escrow.canRefund(id));
            else assertFalse(escrow.canClaim(id) || escrow.canRefund(id));
        }
    }

    /// A settled lock never changes again.
    function invariant_settled_is_final() public view {
        assertEq(handler.settledThenChanged(), 0);
    }

    /// Records how much each run did, so the suite can show it isn't only exercising reverts.
    function afterInvariant() public {
        emit log_named_string(
            "run did",
            string.concat(
                vm.toString(handler.idCount()),
                " locks, ",
                vm.toString(handler.claims()),
                " claims, ",
                vm.toString(handler.refunds()),
                " refunds, ",
                vm.toString(handler.releases()),
                " releases, ",
                vm.toString(handler.extends()),
                " extends"
            )
        );
    }

    /// A claimed lock stores the value that opens it.
    function invariant_claims_store_their_secret() public view {
        uint256 n = handler.idCount();
        for (uint256 i = 0; i < n; i++) {
            bytes32 id = handler.ids(i);
            (,, Escrow.State s,,,,, address lockAddress,,, bytes32 revealed) = escrow.locks(id);
            if (s == Escrow.State.Claimed) assertTrue(escrow.reveals(revealed, lockAddress));
            else assertEq(revealed, bytes32(0));
        }
    }
}
