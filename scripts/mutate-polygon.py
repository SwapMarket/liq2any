#!/usr/bin/env python3
"""Mutation checks for the Polygon escrow: break each rule in turn and require the test suite to fail.

Each mutation is an exact text replacement in a contract source. The script applies it, runs `forge test` (unit and invariant
suites; the fork suite skips without POLYGON_RPC), restores the file, and reports any mutation that survived. Run from the
repository root, on a clean tree:

    python3 -I scripts/mutate-polygon.py
"""

import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent / "contracts" / "polygon"
ESCROW = "src/Escrow.sol"
VAULT = "src/SwapVault.sol"
CALL = "src/TokenCall.sol"

# Mutations that can't change behaviour, with the reason. They are run and reported, but don't fail the script.
EQUIVALENT = {
    "if (vault == address(0)) revert VaultFailed();":
        "CREATE2 fails only if the vault exists, which AlreadyUsed stops first, or out of gas, which leaves 1/64 of the gas: "
        "too little for the transfer that follows. Kept as a free safety net.",
}

MUTATIONS = [
    # The secret check.
    (ESCROW, "return lockAddress != address(0) && ecrecover(", "return ecrecover("),
    (ESCROW, "ecrecover(bytes32(0), 27, GX, s)", "ecrecover(bytes32(0), 28, GX, s)"),
    (ESCROW, "if (!reveals(s, l.lockAddress)) revert WrongSecret();", ""),
    # Lock fields.
    (ESCROW, "lockAddress == address(0) || recipient", "recipient"),
    (ESCROW, "recipient == address(0) || recipient == msg.sender", "recipient == msg.sender"),
    (ESCROW, "|| recipient == msg.sender || amount == 0", "|| amount == 0"),
    (ESCROW, "|| amount == 0\n", "\n"),
    (ESCROW, "deadlineTime <= block.timestamp || ", ""),
    (ESCROW, "|| deadlineTime > block.timestamp + MAX_WINDOW", ""),
    (ESCROW, "deadlineBlock <= block.number || ", ""),
    (ESCROW, "|| deadlineBlock > block.number + maxWindowBlocks", ""),
    (ESCROW, "|| (msg.value != 0 && gasTo == address(0))", ""),
    (ESCROW, "if (l.state != State.None) revert AlreadyUsed();", ""),
    (ESCROW, "if (vault == address(0)) revert VaultFailed();", ""),
    (ESCROW, "if (token.balanceOf(vault) - before != amount) revert WrongAmount();", ""),
    (ESCROW, "if (!ok) revert GasDropFailed();", ""),
    (ESCROW, "return keccak256(abi.encode(initiator, lockAddress));", "return keccak256(abi.encode(lockAddress));"),
    # The clock rule.
    (ESCROW, "(timeOpen || blocksOpen) : (timeOpen && blocksOpen)", "(timeOpen && blocksOpen) : (timeOpen || blocksOpen)"),
    (ESCROW, "(timeOpen || blocksOpen) :", "(timeOpen) :"),
    (ESCROW, ": (timeOpen && blocksOpen)", ": (timeOpen)"),
    (ESCROW, ": (timeOpen && blocksOpen)", ": (blocksOpen)"),
    (ESCROW, "bool timeOpen = block.timestamp < l.deadlineTime;", "bool timeOpen = block.timestamp <= l.deadlineTime;"),
    (ESCROW, "bool blocksOpen = block.number < l.deadlineBlock;", "bool blocksOpen = block.number <= l.deadlineBlock;"),
    # Settling.
    (ESCROW, "if (!canClaim(id)) revert ClaimClosed();", ""),
    (ESCROW, "if (canClaim(id)) revert RefundNotOpen();", ""),
    (ESCROW, "l.revealed = s;", ""),
    (ESCROW, "l.state = State.Claimed;", ""),
    (ESCROW, "        l.state = State.Refunded;\n        emit Refunded(id);", "        emit Refunded(id);"),
    (ESCROW, "        l.state = State.Refunded;\n        emit Released(id);", "        emit Released(id);"),
    (ESCROW, "if (msg.sender != l.recipient) revert NotRecipient();\n        l.state", "l.state"),
    (ESCROW, "SwapVault(vaultOf(id)).pay(l.recipient, l.amount);", "SwapVault(vaultOf(id)).pay(msg.sender, l.amount);"),
    (ESCROW, "emit Refunded(id);\n        SwapVault(vaultOf(id)).pay(l.initiator, l.amount);",
     "emit Refunded(id);\n        SwapVault(vaultOf(id)).pay(l.recipient, l.amount);"),
    # Extend.
    (ESCROW, "if (msg.sender != l.recipient) revert NotRecipient();\n        if (\n            !l.longLeg", "if (\n            !l.longLeg"),
    (ESCROW, "!l.longLeg || l.extended || ", "l.extended || "),
    (ESCROW, "|| l.extended || !canClaim(id)", "|| !canClaim(id)"),
    (ESCROW, "|| !canClaim(id) || newTime", "|| newTime"),
    (ESCROW, "|| newTime <= l.deadlineTime", ""),
    (ESCROW, "|| newTime > uint256(l.deadlineTime) + MAX_EXTENSION", ""),
    (ESCROW, "|| newBlock <= l.deadlineBlock", ""),
    (ESCROW, "|| newBlock > uint256(l.deadlineBlock) + maxExtensionBlocks", ""),
    (ESCROW, "l.extended = true;", ""),
    # Permits and the guard.
    (ESCROW, "if (permitDeadline == 0) revert BadLock();", ""),
    (ESCROW, "if (entered != 1) revert Reentered();", ""),
    # Constructor.
    (ESCROW, "if (token_.code.length == 0 || maxWindowBlocks_ == 0", "if (maxWindowBlocks_ == 0"),
    # Vault and token calls.
    (VAULT, "if (msg.sender != escrow) revert OnlyEscrow();", ""),
    (CALL, "if (!ok) revert TokenCallFailed();\n        if (ret.length", "if (ret.length"),
    (CALL, "if (ret.length != 0 && (ret.length != 32 || abi.decode(ret, (uint256)) != 1)) revert TokenCallFailed();", ""),
]


def tests_pass() -> bool:
    result = subprocess.run(["forge", "test"], cwd=ROOT, capture_output=True, text=True)
    return result.returncode == 0


def main() -> int:
    if not tests_pass():
        print("The unmutated suite fails; fix that first.")
        return 1
    survivors = []
    for index, (path, old, new) in enumerate(MUTATIONS, 1):
        file = ROOT / path
        original = file.read_text()
        if original.count(old) != 1:
            print(f"{index:2}. NOT APPLIED (text found {original.count(old)} times): {old[:70]!r}")
            survivors.append((path, old, "not applied"))
            continue
        file.write_text(original.replace(old, new))
        try:
            passed = tests_pass()
        finally:
            file.write_text(original)
        if passed and old in EQUIVALENT:
            print(f"{index:2}. equivalent (expected): {path}: {old.strip()[:80]!r}\n      {EQUIVALENT[old]}")
            continue
        status = "SURVIVED" if passed else "caught"
        print(f"{index:2}. {status}: {path}: {old.strip()[:80]!r}")
        if passed:
            survivors.append((path, old, "survived"))
    print(f"\n{len(MUTATIONS) - len(survivors)} of {len(MUTATIONS)} mutations caught or known equivalent.")
    return 1 if survivors else 0


if __name__ == "__main__":
    sys.exit(main())
