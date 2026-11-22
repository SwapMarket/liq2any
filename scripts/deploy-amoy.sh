#!/usr/bin/env bash
# Deploys the stand-in token and the escrow to Polygon Amoy, TEST NETWORK ONLY, with the key in contracts/polygon/.keys/.
# Refuses any other chain. Writes the addresses to contracts/polygon/deployments/amoy.json.
#
#   scripts/deploy-amoy.sh [rpc]        (default https://polygon-amoy-bor-rpc.publicnode.com)
#
# TOKEN=0x… reuses a stand-in token deployed earlier, after checking that its code is this build's.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/contracts/polygon"
RPC="${1:-https://polygon-amoy-bor-rpc.publicnode.com}"
KEYFILE="$DIR/.keys/amoy-deployer.json"
OUT="$DIR/deployments/amoy.json"

[ "$(cast chain-id --rpc-url "$RPC")" = "80002" ] || { echo "Not Amoy (chain 80002). Refusing." >&2; exit 1; }
[ -f "$KEYFILE" ] || { echo "No key at $KEYFILE." >&2; exit 1; }
[ -e "$OUT" ] && { echo "$OUT exists: Amoy is already deployed. Move it away to deploy again." >&2; exit 1; }

KEY="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[0]["private_key"])' "$KEYFILE")"
FROM="$(cast wallet address --private-key "$KEY")"
echo "Deployer $FROM, balance $(cast from-wei "$(cast balance "$FROM" --rpc-url "$RPC")") POL"

# deploy <contract> [constructor args…]. The arguments go last: `--constructor-args` takes everything after it, and once swallowed
# `--rpc-url` and the key, which sent forge to localhost instead.
deploy() {
  local contract="$1"
  shift
  local out
  out="$(cd "$DIR" && forge create "$contract" --rpc-url "$RPC" --private-key "$KEY" --broadcast --json ${1:+--constructor-args "$@"})"
  python3 -I -c 'import json,sys; d=json.loads(sys.argv[1]); print(d["deployedTo"], d["transactionHash"])' "$out"
}

# Every differing byte of deployed code must be a zero in the build: an immutable, filled in at deployment.
same_code() {
  local built
  built="$(cd "$DIR" && forge inspect "$2" deployedBytecode)"
  python3 -I -c '
import sys
a, b = bytes.fromhex(sys.argv[1][2:]), bytes.fromhex(sys.argv[2][2:])
sys.exit(0 if len(a) == len(b) and all(x == y or y == 0 for x, y in zip(a, b)) else 1)' "$(cast code "$1" --rpc-url "$RPC")" "$built"
}

if [ -n "${TOKEN:-}" ]; then
  same_code "$TOKEN" src/testnet/StandInUSDT0.sol:StandInUSDT0 || { echo "$TOKEN isn't this build's stand-in." >&2; exit 1; }
  TOKEN_TX="(deployed earlier)"
else
  read -r TOKEN TOKEN_TX <<<"$(deploy src/testnet/StandInUSDT0.sol:StandInUSDT0)"
  [ -n "$TOKEN" ] || { echo "The token deployment failed." >&2; exit 1; }
fi
echo "Stand-in token $TOKEN ($TOKEN_TX)"
# 14 days and 7 days at Amoy's 1.0 s blocks.
read -r ESCROW ESCROW_TX <<<"$(deploy src/Escrow.sol:Escrow "$TOKEN" 1209600 604800)"
[ -n "$ESCROW" ] || { echo "The escrow deployment failed." >&2; exit 1; }
echo "Escrow $ESCROW ($ESCROW_TX)"
same_code "$ESCROW" src/Escrow.sol:Escrow || { echo "Deployed escrow code differs from the build." >&2; exit 1; }
[ "$(cast call "$ESCROW" 'token()(address)' --rpc-url "$RPC" | tr A-F a-f)" = "$(echo "$TOKEN" | tr A-F a-f)" ] || { echo "Escrow names the wrong token." >&2; exit 1; }
echo "Deployed code matches the build, and the escrow names the stand-in."

BLOCK="$(cast receipt "$ESCROW_TX" blockNumber --rpc-url "$RPC")"
COMMIT="$(git -C "$ROOT" rev-parse HEAD)"
mkdir -p "$(dirname "$OUT")"
cat > "$OUT" <<EOF
{
  "network": "amoy",
  "chain_id": 80002,
  "token": "$TOKEN",
  "token_tx": "$TOKEN_TX",
  "escrow": "$ESCROW",
  "escrow_tx": "$ESCROW_TX",
  "escrow_block": $BLOCK,
  "max_window_blocks": 1209600,
  "max_extension_blocks": 604800,
  "deployer": "$FROM",
  "source_commit": "$COMMIT"
}
EOF
echo "Wrote $OUT"
