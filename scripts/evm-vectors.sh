#!/usr/bin/env bash
# Test vectors for shared/chain/src/evm.rs, made by Foundry's `cast`, an independent implementation. Offline: nothing is sent.
#
#   scripts/evm-vectors.sh > shared/chain/tests/evm-vectors.txt
#
# Lines are `kind|fields…|expected`. The key is the well-known example key from the web3 documentation, never used for funds.
set -euo pipefail

KEY=0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318
ESCROW=0xD1C669097D4EDb41B5A5b3541aFB5c22b7Bd6f6F
LOCK_SIG="lock(address,address,uint128,uint64,uint64,bool,address)"

echo "address|$KEY|$(cast wallet address --private-key "$KEY")"
for a in 0xc2132d05d31c914a87c6611c10748aeb04b58e8f 0x4dff9b5b0143e642a3f63a5bcf2d1c328e600bf8 0x0000000000000000000000000000000000000001; do
  echo "checksum|$a|$(cast to-check-sum-address "$a")"
done

# calldata|signature|args (comma-separated)|expected
lock_args=(0x73D8819B31105eebf456c261eAaE5e02b04ad207 0x000000000000000000000000000000000000aa01 25000000 1795400000 53560000 false 0x000000000000000000000000000000000000aa02)
echo "calldata|$LOCK_SIG|$(IFS=,; echo "${lock_args[*]}")|$(cast calldata "$LOCK_SIG" "${lock_args[@]}")"
claim_args=(0x4e8abf76d6919b2b7c9a1cdf9d581bc0b3922cf6a2912fc2e50b9c1b3d8f6de5 0xa0e045a5cf74be8b84aadd75cf57365e70c48752bdbc592aae092b86540afd45)
echo "calldata|claim(bytes32,bytes32)|$(IFS=,; echo "${claim_args[*]}")|$(cast calldata "claim(bytes32,bytes32)" "${claim_args[@]}")"
echo "calldata|refund(bytes32)|${claim_args[0]}|$(cast calldata "refund(bytes32)" "${claim_args[0]}")"
echo "calldata|extend(bytes32,uint64,uint64)|${claim_args[0]},1795400000,53560000|$(cast calldata "extend(bytes32,uint64,uint64)" "${claim_args[0]}" 1795400000 53560000)"
echo "calldata|lock(address,address,uint128,uint64,uint64,bool,address)|0x0000000000000000000000000000000000000001,0x0000000000000000000000000000000000000002,340282366920938463463374607431768211455,18446744073709551615,1,true,0x0000000000000000000000000000000000000000|$(cast calldata "$LOCK_SIG" 0x0000000000000000000000000000000000000001 0x0000000000000000000000000000000000000002 340282366920938463463374607431768211455 18446744073709551615 1 true 0x0000000000000000000000000000000000000000)"

# tx|chain|nonce|priority fee|max fee|gas|to|value|calldata hex|expected raw
tx() {
  local chain=$1 nonce=$2 prio=$3 max=$4 gas=$5 to=$6 value=$7 data=$8
  local raw
  if [ -n "$data" ]; then
    raw=$(cast mktx --private-key "$KEY" --chain "$chain" --nonce "$nonce" --gas-limit "$gas" --gas-price "$max" \
      --priority-gas-price "$prio" --value "$value" "$to" "$data")
  else
    raw=$(cast mktx --private-key "$KEY" --chain "$chain" --nonce "$nonce" --gas-limit "$gas" --gas-price "$max" \
      --priority-gas-price "$prio" --value "$value" "$to")
  fi
  echo "tx|$chain|$nonce|$prio|$max|$gas|$to|$value|$data|$raw"
}
claim_data=$(cast calldata "claim(bytes32,bytes32)" "${claim_args[@]}")
lock_data=$(cast calldata "$LOCK_SIG" "${lock_args[@]}")
tx 80002 7 30000000000 60000000000 300000 "$ESCROW" 0 "$claim_data"
tx 137 0 30000000000 350000000000 400000 "$ESCROW" 1000000000000000 "$lock_data"
tx 80002 0 0 1 21000 0x000000000000000000000000000000000000aa02 0 ""
tx 137 4294967295 2000000000000 9000000000000 30000000 "$ESCROW" 79228162514264337593543950335 "$claim_data"
tx 31337 127 1 128 50000 0x000000000000000000000000000000000000aa02 127 0x00
