#!/usr/bin/env python3
"""Test vectors for the Polygon secret check, from the definitions alone.

secp256k1 from its curve equation and keccak-256 from FIPS 202's permutation with the original Keccak padding (0x01, not SHA-3's
0x06), so nothing here shares code with Solidity, Foundry or the Rust crates. For each secret t it prints:

    t, s = t·Gx mod n, T = t·G (x then y), address(T) = last 20 bytes of keccak256(x ‖ y)

Run: python3 -I scripts/polygon-vectors.py > contracts/polygon/test/vectors.txt
"""

P = 2**256 - 2**32 - 977
N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
GX = 0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798
GY = 0x483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8


def add(a, b):
    if a is None:
        return b
    if b is None:
        return a
    if a[0] == b[0] and (a[1] + b[1]) % P == 0:
        return None
    if a == b:
        lam = 3 * a[0] * a[0] * pow(2 * a[1], -1, P) % P
    else:
        lam = (b[1] - a[1]) * pow(b[0] - a[0], -1, P) % P
    x = (lam * lam - a[0] - b[0]) % P
    return x, (lam * (a[0] - x) - a[1]) % P


def mul(k, point):
    out = None
    while k:
        if k & 1:
            out = add(out, point)
        point = add(point, point)
        k >>= 1
    return out


# Keccak-f[1600], FIPS 202 section 3.
ROUND_CONSTANTS = [
    0x0000000000000001, 0x0000000000008082, 0x800000000000808A, 0x8000000080008000, 0x000000000000808B, 0x0000000080000001,
    0x8000000080008081, 0x8000000000008009, 0x000000000000008A, 0x0000000000000088, 0x0000000080008009, 0x000000008000000A,
    0x000000008000808B, 0x800000000000008B, 0x8000000000008089, 0x8000000000008003, 0x8000000000008002, 0x8000000000000080,
    0x000000000000800A, 0x800000008000000A, 0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
]
ROTATIONS = [[0, 36, 3, 41, 18], [1, 44, 10, 45, 2], [62, 6, 43, 15, 61], [28, 55, 25, 21, 56], [27, 20, 39, 8, 14]]
MASK = 2**64 - 1


def rol(v, n):
    return ((v << n) | (v >> (64 - n))) & MASK if n else v


def keccak_f(a):
    for rc in ROUND_CONSTANTS:
        c = [a[x][0] ^ a[x][1] ^ a[x][2] ^ a[x][3] ^ a[x][4] for x in range(5)]
        d = [c[(x - 1) % 5] ^ rol(c[(x + 1) % 5], 1) for x in range(5)]
        a = [[a[x][y] ^ d[x] for y in range(5)] for x in range(5)]
        b = [[0] * 5 for _ in range(5)]
        for x in range(5):
            for y in range(5):
                b[y][(2 * x + 3 * y) % 5] = rol(a[x][y], ROTATIONS[x][y])
        a = [[b[x][y] ^ ((~b[(x + 1) % 5][y]) & b[(x + 2) % 5][y]) for y in range(5)] for x in range(5)]
        a[0][0] ^= rc
    return a


def keccak256(data: bytes) -> bytes:
    rate = 136
    msg = bytearray(data) + b"\x01"
    msg += b"\x00" * (-len(msg) % rate)
    msg[-1] |= 0x80
    a = [[0] * 5 for _ in range(5)]
    for off in range(0, len(msg), rate):
        block = msg[off:off + rate]
        for i in range(rate // 8):
            a[i % 5][i // 5] ^= int.from_bytes(block[8 * i:8 * i + 8], "little")
        a = keccak_f(a)
    return b"".join(a[i % 5][i // 5].to_bytes(8, "little") for i in range(4))


def vector(t):
    x, y = mul(t, (GX, GY))
    s = t * GX % N
    assert s * pow(GX, -1, N) % N == t
    address = keccak256(x.to_bytes(32, "big") + y.to_bytes(32, "big"))[12:]
    return t, s, x, y, address


def main():
    # Self-checks against published values: keccak256("") and the address of private key 1.
    assert keccak256(b"").hex() == "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
    assert vector(1)[4].hex() == "7e5f4552091a69125d5dfcb7b8c2659029395bdf"
    secrets = [
        1,
        2,
        N - 1,
        # Fixed "random" values, so the file is reproducible: SHA-256 of a label, reduced into range.
        *[int.from_bytes(__import__("hashlib").sha256(f"liq2any polygon vector {i}".encode()).digest(), "big") % (N - 1) + 1
          for i in range(5)],
    ]
    for t in secrets:
        t, s, x, y, address = vector(t)
        print(f"{t:064x} {s:064x} {x:064x}{y:064x} {address.hex()}")


if __name__ == "__main__":
    main()
