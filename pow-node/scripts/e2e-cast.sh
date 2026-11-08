#!/usr/bin/env bash
# End-to-end check of the EVM integration, driven by Foundry's `cast`.
#
# Starts a fresh `breeja --dev` node, then uses only standard tooling to:
#   transfer ETH, deploy examples/Counter.sol, read its code and storage,
#   estimate gas, call it, read its events, and check that a reverting call is
#   reported by eth_call and mined (status 0, sender charged, state unchanged).
#
# Every step asserts its result. The script exits non-zero on the first failure
# and always stops the node it started.
#
# Requirements: cargo, cast (Foundry), jq.
# Usage: scripts/e2e-cast.sh            (from pow-node/)
#        PORT=8600 scripts/e2e-cast.sh  (use another port)

set -euo pipefail

cd "$(dirname "$0")/.."

PORT="${PORT:-8545}"
export ETH_RPC_URL="http://127.0.0.1:${PORT}"
# Anvil/Hardhat dev account #0 and #1. Public keys: never hold real funds.
PK=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
ALICE=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
BOB=0x70997970C51812dc3A010C7d01b50e0d17dc79C8
TIMEOUT="${TIMEOUT:-120}"
LOG="$(mktemp -t breeja-e2e.XXXXXX.log)"

for tool in cargo cast jq; do
  command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 2; }
done

pass=0
step() { printf '\n== %s\n' "$*"; }
ok()   { pass=$((pass + 1)); printf '   PASS  %s\n' "$*"; }
fail() { printf '   FAIL  %s\n' "$*" >&2; echo "--- node log (last 40 lines) ---" >&2; tail -40 "$LOG" >&2; exit 1; }
expect_eq() { [ "$1" = "$2" ] && ok "$3 ($1)" || fail "$3: expected '$2', got '$1'"; }

step "build the node (release)"
cargo build --release --quiet
ok "cargo build --release"

if curl -s -o /dev/null "$ETH_RPC_URL"; then
  fail "something is already listening on port $PORT; set PORT to a free one"
fi

step "start breeja --dev on port $PORT"
RUST_LOG=info ./target/release/breeja --dev --rpc-address "127.0.0.1:${PORT}" >"$LOG" 2>&1 &
NODE_PID=$!
trap 'kill "$NODE_PID" 2>/dev/null || true; wait "$NODE_PID" 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do cast chain-id >/dev/null 2>&1 && break; sleep 0.2; done
expect_eq "$(cast chain-id)" "1" "eth_chainId"
expect_eq "$(cast gas-price)" "1000000000" "eth_gasPrice (fixed 1 gwei)"

# A plain transfer, exactly as the README shows it.
step "M2: transfer 1 ETH with cast send"
bob_before=$(cast balance "$BOB")
receipt=$(timeout "$TIMEOUT" cast send --legacy --private-key "$PK" --value 1ether "$BOB" --json)
expect_eq "$(jq -r .status <<<"$receipt")" "0x1" "transfer status"
expect_eq "$(cast to-dec "$(jq -r .gasUsed <<<"$receipt")")" "21000" "transfer gas is exactly 21000"
expect_eq "$(cast estimate "$BOB" --value 1ether)" "21000" "eth_estimateGas for a plain transfer"
bob_after=$(cast balance "$BOB")
expect_eq "$(python3 -c "print($bob_after - $bob_before)")" \
  "1000000000000000000" "recipient balance rose by 1 ETH"
block=$(jq -r .blockNumber <<<"$receipt")
expect_eq "$(cast block "$block" --json | jq -r .sha3Uncles)" \
  "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347" "cast can read the block"

# Deployment of the real Solidity contract (solc 0.8.24, Shanghai, uses PUSH0).
step "M3: deploy examples/Counter.sol with cast send --create"
receipt=$(timeout "$TIMEOUT" cast send --legacy --private-key "$PK" --create "$(cat examples/Counter.bin)" --json)
expect_eq "$(jq -r .status <<<"$receipt")" "0x1" "deployment status"
COUNTER=$(jq -r .contractAddress <<<"$receipt")
[ "$COUNTER" != "null" ] && ok "receipt has contractAddress $COUNTER" || fail "no contractAddress"
expect_eq "$(jq -r .to <<<"$receipt")" "null" "a creation has no 'to'"

step "M5: eth_getCode returns the runtime bytecode"
code=$(cast code "$COUNTER")
[ "$code" = "0x$(tr -d '\n' < examples/Counter.runtime.bin)" ] \
  && ok "deployed code == Counter.runtime.bin ($(( (${#code} - 2) / 2 )) bytes)" \
  || fail "deployed code differs from Counter.runtime.bin"
expect_eq "$(cast code "$BOB")" "0x" "an account without code"

step "M4: eth_call reads state without changing it"
expect_eq "$(cast call "$COUNTER" 'count()(uint256)')" "0" "count() before any increment"

step "M4: eth_estimateGas searches for the gas a call needs"
estimate=$(cast estimate "$COUNTER" 'increment()')
[ "$estimate" -gt 21000 ] && ok "estimate for increment() = $estimate (> 21000)" || fail "estimate too low: $estimate"

step "M3: a state-changing call emits an event"
receipt=$(timeout "$TIMEOUT" cast send --legacy --private-key "$PK" "$COUNTER" 'increment()' --json)
expect_eq "$(jq -r .status <<<"$receipt")" "0x1" "increment() status"
used=$(cast to-dec "$(jq -r .gasUsed <<<"$receipt")")
[ "$used" -le "$estimate" ] && ok "gas used $used <= estimate $estimate" || fail "estimate $estimate below gas used $used"
expect_eq "$(jq -r '.logs | length' <<<"$receipt")" "1" "one log in the receipt"
[ "$(jq -r .logsBloom <<<"$receipt")" != "0x$(printf '0%.0s' $(seq 1 512))" ] \
  && ok "receipt logsBloom is non-zero" || fail "receipt logsBloom is all zero"
expect_eq "$(cast call "$COUNTER" 'count()(uint256)')" "1" "count() after increment"
expect_eq "$(cast storage "$COUNTER" 0)" \
  "0x0000000000000000000000000000000000000000000000000000000000000001" "eth_getStorageAt slot 0"

step "M5: eth_getLogs finds the event by address and topic"
timeout "$TIMEOUT" cast send --legacy --private-key "$PK" "$COUNTER" 'increment()' >/dev/null
logs=$(cast logs --from-block 0 --address "$COUNTER" 'Incremented(address indexed,uint256)' --json)
expect_eq "$(jq length <<<"$logs")" "2" "two Incremented events"
expect_eq "$(jq -r '.[1].topics[1]' <<<"$logs")" \
  "0x000000000000000000000000$(tr 'A-F' 'a-f' <<<"${ALICE#0x}")" "indexed 'by' topic is the sender"
expect_eq "$(cast to-dec "$(jq -r '.[1].data' <<<"$logs")")" "2" "event data newCount = 2"
other=$(cast logs --from-block 0 --address "$BOB" --json)
expect_eq "$(jq length <<<"$other")" "0" "no logs for an address that emitted none"

step "M4: a revert is reported by eth_call as error code 3 with the revert data"
if out=$(cast call "$COUNTER" 'set(uint256)' 500 2>&1); then fail "set(500) should revert"; fi
grep -q 'error code 3' <<<"$out" && ok "error code 3" || fail "unexpected error: $out"
grep -q '0x8aa0e18c' <<<"$out" && ok "revert data is TooBig() (0x8aa0e18c)" || fail "unexpected data: $out"

step "M3: a reverting transaction is mined, charged, and changes nothing"
nonce_before=$(cast nonce "$ALICE")
# The gas limit is given because cast would otherwise refuse to send a call
# whose estimate reverts: that refusal is the point of eth_estimateGas.
receipt=$(timeout "$TIMEOUT" cast send --legacy --private-key "$PK" --gas-limit 100000 "$COUNTER" 'set(uint256)' 500 --json)
expect_eq "$(jq -r .status <<<"$receipt")" "0x0" "status 0 (failed)"
used=$(cast to-dec "$(jq -r .gasUsed <<<"$receipt")")
[ "$used" -gt 21000 ] && ok "sender was charged $used gas" || fail "gas used $used"
expect_eq "$(jq -r '.logs | length' <<<"$receipt")" "0" "a failed transaction has no logs"
expect_eq "$(cast nonce "$ALICE")" "$((nonce_before + 1))" "sender nonce still advanced"
expect_eq "$(cast call "$COUNTER" 'count()(uint256)')" "2" "count unchanged by the revert"

step "M6: block bookkeeping"
blk=$(cast block "$(jq -r .blockNumber <<<"$receipt")" --json)
[ "$(cast to-dec "$(jq -r .gasUsed <<<"$blk")")" -le "$(cast to-dec "$(jq -r .gasLimit <<<"$blk")")" ] \
  && ok "block gasUsed <= gasLimit" || fail "block over its gas limit"
expect_eq "$(cast tx "$(jq -r .transactionHash <<<"$receipt")" from)" "$ALICE" "cast tx reads the transaction"

if grep -qE ' (WARN|ERROR) ' "$LOG"; then fail "node logged a warning or error"; fi
ok "node log has no warnings or errors"

printf '\nALL %d CHECKS PASSED\n' "$pass"
