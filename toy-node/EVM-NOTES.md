# EVM-NOTES: giving Breeja an EVM

> DRAFT. Rewrite every answer in your own words before you submit. Anything you
> cannot explain out loud should be cut or reworked, because this is what you defend.

## 1. Baseline (Milestone 0)

Branch `feat/evm`, cut from upstream `main` (`e981e2b`).

- `cargo test --workspace`: **48 passed, 0 failed** (13 node unit, 8 mining
  integration, 14 primitives, 13 JSON-RPC).
- `cargo clippy --workspace --all-targets`: no warnings.
- REVM `43` added to `crates/node/Cargo.toml`; `cargo build` is clean. No
  behaviour change.
- `cargo run --release -- --dev`: genesis sealed with 4 pre-funded accounts at
  10,000 ETH each, JSON-RPC on `127.0.0.1:8545` with 11 methods, miner at
  difficulty 3 retargeting toward a 2 second block time. Blocks sealed with
  `txs=0` (no transactions sent). `eth_blockNumber` returned `0x15b` (347) after
  a few minutes and rose between calls.

## 2. What the code looks like today (Part 0 reading)

| File | What matters for the EVM |
| --- | --- |
| `primitives/src/node.rs` | `Account` has `nonce`, `balance`, `storage_root`, `code_hash`, but the last two are always `B256::ZERO` (from `Default`). `Transaction`, `PendingTransaction` and `TransactionReceipt` all have a non-optional `to: Address`, so a contract creation has nowhere to live yet. Receipts have no `logs`, no `contract_address`, and a `status` that is always `1`. |
| `node/src/executor.rs` | `apply_transfer` (lines 29-56) is the whole state transition. It increments the nonce itself (line 50) and credits the miner's fee itself (line 53). Both are things REVM does on its own (Pitfalls 1 and 3). `execute_block` skips a failing transaction (lines 74-78, 128-130). |
| `node/src/miner.rs` | Loop: snapshot under a read lock (54-61), execute and mine with no lock held (66-100), then take the write lock only to commit (133-143). `header.gas_limit` is fixed at `BLOCK_GAS_LIMIT`. |
| `node/src/pool.rs` | Three checks assume there is no EVM (see Q1). Admission reserves `gas * gas_price` (150-158). |
| `rpc/src/handlers.rs` | `call_handler` (240-258) returns `0x` for a no-calldata call and an error otherwise. `estimate_gas_handler` (265-295) returns the constant `21000`. Both refuse anything with calldata through `no_evm(...)`. |

Other things I noticed that will matter later:

- `Block` and `BlockHeader` have no `mix_hash` field, so `prevrandao` has to be
  the parent hash (Pitfall 7). The block's own hash does not exist when the EVM
  runs: it depends on the state root that execution produces.
- `state_root_of` (node.rs:292-302) hashes sorted addresses plus the RLP of each
  `Account`. It never sees code or storage contents, so Milestone 6 must change it.
- `peek_mempool_batch` (node.rs:473-476) caps a block at
  `BLOCK_GAS_LIMIT / TRANSFER_GAS = 149` transactions. That assumes every
  transaction costs 21,000 gas.
- `Account::default()` gives `code_hash = B256::ZERO`, but an account with no code
  has `code_hash = KECCAK_EMPTY` in REVM. Decide deliberately how `Database::basic`
  reports an empty account.
- The mined `txs=` log field uses `batch.len()` (miner.rs:148), the number *attempted*,
  not the number included. Once failed transactions can be skipped this can be wrong.

## 3. Part 0 questions

**Q1. Which `pool.rs` checks must change?**

1. `TxKind::Create` is rejected (pool.rs:63-69). *Remove it.* A creation is now
   valid; the transaction's `to` must become `Option<Address>`.
2. Non-empty calldata is rejected (pool.rs:72-78). *Remove it.* Calldata is how a
   contract is called.
3. `gas < TRANSFER_GAS` is rejected (pool.rs:81-83). It does not mention the EVM
   but assumes every transaction is a plain transfer costing 21,000. *Relax it into
   a real intrinsic-gas calculation:* 21,000 base, plus a per-byte calldata cost,
   plus 32,000 more for a creation. A flat 21,000 floor would wrongly admit a
   transaction with calldata whose gas limit cannot even cover its own intrinsic cost.

**Q2. Is skipping failed transactions still right once contracts exist?**

No. Skipping was correct when the only failures were *invalid* transactions (wrong
nonce, cannot afford the up-front reservation), which were races worth dropping.
A contract call can be perfectly valid and still `REVERT` or halt (out of gas)
partway. Real Ethereum *includes* it, charges the sender for the gas burned, and
advances the nonce, and the receipt carries `status: 0x0`. If Breeja kept skipping
those, the miner would run the contract code and be paid nothing for it. So the
rule becomes: skip only what is invalid; include and charge what fails during
execution.

**Q3. Why reserve `gas_limit × gas_price` at admission but charge `TRANSFER_GAS`?**

Admission must guarantee the sender can afford the *worst case*, so it reserves the
full limit (pool.rs:150, and `get_pending_balance` at node.rs:436). Settlement then
charges only the gas actually used and refunds the difference. Today the difference
is a fixed `gas_limit - 21000` and easy to overlook; once gas is variable it is real
and depends on what execution burns. A revert still charges the gas used up to the
revert; a halt consumes the whole limit.

**Q4. What does a full state snapshot per block cost once accounts have storage?**

`commit_block` pushes `self.accounts.clone()` into `state_history` for every block
(node.rs:506), which is why historical `eth_getBalance` works. Today that clones a
handful of 4-field accounts. With storage and code it clones every account's whole
`HashMap<U256, U256>` on every block, so memory grows roughly with
(number of blocks) x (total state size) rather than just the number of accounts.
The miner also clones the whole state at the top of every loop iteration
(`accounts_snapshot`, miner.rs:54-61), including when it restarts because a new
transaction arrived. Mitigations to weigh: share code through a `code_hash` to
bytecode map, store per-block diffs instead of whole snapshots, or keep the
snapshots and state this as a known limitation of a teaching chain.

## 4. Milestone 1: teaching state about code and storage

**What changed (no behaviour change; the miner and executor are untouched):**

- `Account` (primitives) gained `storage: HashMap<U256, U256>`. Its RLP encoding is
  still exactly the four Ethereum fields (`nonce`, `balance`, `storage_root`,
  `code_hash`), through a small `AccountRlp` helper, so state roots are unchanged.
  Storage is carried alongside the account rather than embedded in the encoding,
  because Ethereum commits to storage through `storage_root`. Computing
  `storage_root` from `storage` is Milestone 6.
- `BlockState` gained a chain-wide `code: HashMap<B256, Bytes>` keyed by
  `keccak256(code)`, and accessors `get_code`, `get_code_by_hash`, `get_storage`,
  `get_storage_at` (historical, using the existing per-block snapshots),
  `code_snapshot` and `insert_code`.
- New `crates/node/src/evm.rs` with `BreejaDb`, a snapshot of accounts, code and
  recent block hashes, implementing REVM's `Database` and `DatabaseCommit`.
  `BreejaDb::from_state` copies state out so the lock can be released before
  executing, the same pattern as the miner's `accounts_snapshot()`.

**Tests:** 61 pass (was 48): +3 in primitives (code is content-addressed, storage
history, storage is outside the four-field encoding) and +10 in `evm.rs`
(`basic` for a funded and an unknown account, code and storage reads, `block_hash`,
`commit` of balance/nonce/code/storage, untouched accounts ignored, zero write
removes a slot, selfdestruct removes the account, snapshot from state).
Clippy: no warnings.

**The end-to-end proof.** The unit tests above build REVM's change records by hand,
so on their own they only show that `BreejaDb` follows the traits. The test
`a_real_evm_deploys_and_calls_a_contract_through_the_database` closes that gap: a
real REVM deploys the brief's 11-byte contract and calls it, with every read and
write going through `BreejaDb`. It asserts that:

- the deploy returns a contract address, and the runtime code (not the initcode)
  lands in the shared code map under its hash;
- the call writes `42` to slot 0 and emits exactly one log, and the slot is
  committed;
- gas matches the brief's measurements on REVM 43: **55,522 to deploy, 43,487 to
  call**;
- REVM itself paid the miner exactly `gas used x gas price` with nothing credited by
  hand (this is Pitfall 1 observed directly), and total supply is unchanged.

## 5. Milestone 2: transfers through REVM (parity)

**What changed:** `apply_transfer` is gone. `executor::execute_transaction` builds a
`TxEnv` (legacy call), a `BlockEnv` from a new `BlockContext`, and runs REVM over a
`BreejaDb`; the resulting state is committed back with `DatabaseCommit`. The miner
now fixes the block timestamp *before* execution (the `TIMESTAMP` opcode can read
it), snapshots with `BreejaDb::from_state`, and passes a `BlockContext`.

- Invalid transaction (bad nonce, cannot afford the reservation) -> `Err`, skipped,
  leaves no trace. A transaction that executes and fails is not skipped (matters
  from M3; for plain transfers nothing can fail after validation).
- Receipt `status` comes from the EVM outcome; gas from `tx_gas_used()`.
- Pitfall canaries as tests: nonce is exactly 1 after one transfer (REVM bumps it,
  we do not), and the miner holds exactly one fee (REVM pays it, we do not).

**Tests:** 62 pass (was 61). The parity test shows a plain transfer through REVM
costs exactly 21,000 gas. The supply-conservation canary in `tests/mining.rs` is
unchanged and passes.

## 6. Milestone 3: contracts

**What changed:**

- `to` is `Option<Address>` on `PendingTransaction`, `Transaction` and
  `TransactionReceipt` (`None` = creation); pending and mined transactions now carry
  `input`. JSON-RPC returns `to: null` for a creation.
- `pool.rs`: the three "no EVM" checks are gone (Part 0 Q1). In their place: init
  code is capped at 49,152 bytes (EIP-3860), and the gas limit must cover
  `intrinsic_gas` = 21,000 + 4/16 per zero/non-zero calldata byte (+ 32,000 and
  2 per init-code word for a creation).
- Receipts are real: `status` 0/1, `contract_address`, `logs` (each with a
  block-wide `logIndex`). A failed transaction's receipt carries no logs, since a
  revert undoes them.
- `execute_transaction` maps `to: None` to `TxKind::Create`; `execute_block` returns
  the code map and `commit_block` merges it, so deployed code survives the block.
- Failed-but-valid transactions are included and charged (Part 0 Q2); only invalid
  ones are skipped.

**Tests:** 70 pass (was 62): deploy (55,522 gas, runtime code stored by hash), call
(43,487 gas, one log, slot 0 = 42), revert is mined with status 0 and nonce
advanced, out-of-gas burns the whole limit, ETH conservation across contract
execution, two pool tests, and a miner end-to-end test (deploy, call, starved call).

## 7. Milestone 4: eth_call and eth_estimateGas

**What changed:**

- `executor::simulate_call` runs a call through the same REVM and `BreejaDb` as real
  execution, then discards the changes. `transact` returns them without applying
  them, so a call cannot leak state. A snapshot is taken under the read lock and
  the EVM runs with no lock held, the same discipline as the miner.
- `eth_call` returns the contract's output as hex. A revert is a JSON-RPC error
  with code 3, message `execution reverted` and the revert data in `data`; a halt
  is an error naming the reason. The block tag now matters: `eth_call` at an older
  block runs against that block's state and header values (`BreejaDb::from_state_at`),
  and a block that does not exist is an error.
- `eth_estimateGas` finds the smallest gas limit that succeeds by binary search
  between the intrinsic cost and the ceiling (`gas` from the request, at most the
  block gas limit), then adds a 5% margin. A plain transfer to an account with no
  code skips the search and returns exactly 21,000. If the call fails even with the
  full ceiling (always reverts), the error is returned instead of a number.
- Requests with no `to` are simulated as a deployment.
- `revm` is built with its `optional_balance_check` feature, which is what makes the
  balance check switchable.

**Tests:** 72 pass (was 70): contract call from an address with no ETH, storage
write discarded, revert as code-3 error, call at a past block (contract does not
exist yet), nonexistent block refused, estimates for transfer / call / deployment /
always-reverting / gas ceiling below intrinsic, and a test that the estimate equals
the true threshold plus the margin and that repeated simulations leave the state
untouched.

## 8. Milestone 5: the RPC methods contracts need

- `eth_getCode(address, block)`: the runtime code (not the init code) at any block,
  `0x` for an account without code or an address nobody used. Historical, because
  an account's `code_hash` is in every state snapshot and code itself is shared.
- `eth_getStorageAt(address, slot, block)`: always a full 32-byte word, so 42 is
  `0x00..2a`, not the minimal quantity `0x2a`. Storage is raw data, not a number.
- `eth_getLogs`: block range or a single `blockHash`; `address` as one value or a
  list; `topics` positional with `null` wildcards and OR lists. A log with fewer
  topics than the filter has positions does not match. The scan skips blocks whose
  `logsBloom` cannot match (that is what the bloom is for), and a request is capped
  at 10,000 blocks so `fromBlock: 0` on a long chain cannot pin the server.
- `eth_gasPrice`: the one fixed price, 1 gwei. This is what lets `cast send` work
  without `--gas-price` (see README; I could not run `cast` here, see below).

## 9. Milestone 6: making it hold together

1. **Block gas limit.** `peek_mempool_batch` now accumulates each transaction's
   gas limit and stops at the first one that would overflow `BLOCK_GAS_LIMIT`; the
   149-transaction rule is gone. It *stops* rather than skipping, because skipping
   would let a later transaction from the same sender run first and fail on its
   nonce. Admission rejects a gas limit above the block limit, since such a
   transaction could never be included and would block everything behind it.
   `execute_block` also refuses to start a transaction that does not fit in the
   gas actually remaining, as a last line of defence.
2. **Intrinsic gas at admission:** done in M3 (21,000 + calldata + 32,000 per
   creation + 2 per init-code word, and an init-code size cap).
3. **`logsBloom`.** Implemented by hand from the Yellow Paper (`bloom_add`):
   keccak the item, take three 11-bit values from byte pairs (0,1), (2,3), (4,5),
   set those bits counting from the end of 256 bytes. A test checks it against
   alloy's reference implementation. Receipts carry the bloom of their logs; blocks
   carry the union, and the field is part of the block header, so it is committed to
   by the block hash. The bloom covers addresses and topics, not data.
4. **State root.** `storage_root` is now derived from the account's storage when the
   account is encoded: slots sorted, each `slot||value` as 32-byte words, keccak256.
   Code was already committed, because `code_hash` is the keccak256 of the code.
   Tests show states differing in one storage slot, or only in code, hash differently.
   This is a flat hash, not a Merkle-Patricia trie, so there are no proofs of a
   single slot.

**Tests:** 88 pass (was 72): bloom vs reference and no false negatives, log filter
semantics, state-root sensitivity to storage and code, gas-based block fill (unit
and a 30-transaction miner test where no block exceeds the limit), admission of an
oversized gas limit, RPC tests for `eth_getCode`, `eth_getStorageAt`, `eth_getLogs`
(range, address, topics, wildcards, OR, reversed range), `eth_gasPrice`, and real
blooms on receipts and blocks.

## 10. The Solidity demo and a live run

`examples/Counter.sol` (storage, an event, a custom-error revert) is compiled by
`examples/compile.js` with solc 0.8.24 for Shanghai; the output is checked in. The
test `a_solidity_contract_can_be_deployed_called_estimated_logged_and_reverted`
deploys it over JSON-RPC, increments it twice using the node's own `eth_estimateGas`
as the gas limit, reads the result via `eth_call`, `eth_getStorageAt` and
`eth_getLogs` (filtered on the event signature and the indexed sender), and sends a
call that reverts with `TooBig()`: `eth_call` returns code 3 with the error selector,
and the real transaction is mined with status `0x0`, charges the sender, advances
the nonce and leaves the stored value unchanged.

I also ran a release `breeja --dev` node and drove it from an independent Python
signer (`eth-account`), not from this repo's test helpers: the deployment returned
the standard first-deployment address for the dev account, `getCode` matched solc's
runtime bytecode, two increments logged events with non-zero blooms and gas 44,967
then 27,867 (the first writes a fresh slot), the estimate was 47,215, `eth_getLogs`
returned both events, the reverting call had `status 0x0` and cost 21,502 gas, and
the block's gas used stayed under the limit. The node log showed no warnings or errors.

What I did **not** run: `cast`/Foundry is not installed here, so the `cast send`
command in the README is untested.

**Existing tests that changed (the brief asks for a justification).** Three of the
original 48 asserted the old no-EVM behaviour, which is now deliberately wrong, so
they were rewritten, not deleted: `eth_call_answers_transfers_and_refuses_anything_needing_the_evm`
(now `eth_call_runs_contract_code_and_changes_nothing` and a revert/history test),
`eth_estimate_gas_returns_the_intrinsic_cost_of_a_transfer` (now
`eth_estimate_gas_finds_the_gas_a_transaction_needs`, which still checks 21,000 for
a transfer), and the "unsupported method" test, which used `eth_gasPrice` as its
example of a missing method and now uses `eth_getProof`. Two tests were also
adjusted for the `to: Option<Address>` change. Total tests: 88.

## 11. Decisions

- [x] **Code representation: `code_hash` plus a shared map**, not `Bytecode` on each
      account. Identical contracts are stored once; it matches what REVM's
      `code_by_hash` expects; and history never copies code (an account's
      `code_hash` is all a snapshot has to remember), which softens the Q4 cost.
- [x] **`Database::basic` for an unknown address returns `None`**, not an empty
      account. REVM treats `None` as a non-existent account, which changes gas
      (sending value to or calling a non-existent account costs more), so
      reporting "empty" would change gas numbers.
- [x] **Two spellings of "no code" are both accepted.** Breeja's default
      `code_hash` is `B256::ZERO`; the EVM uses `KECCAK256_EMPTY`. `basic` always
      reports `KECCAK256_EMPTY`, and `is_empty_code_hash` accepts either.
- [x] **A storage slot written to zero is removed**, as on Ethereum, so two states
      that differ only by a zeroed slot are identical and will hash identically once
      the state root covers storage.
- [x] `eth_call`: nonce and balance checks **off**, and gas price 0. `eth_call` is a
      question, not a payment, and read calls routinely come from addresses with no
      ETH. Consequence to state honestly: a successful `eth_call` does not mean the
      real transaction would be admitted or succeed. State changes before the block
      is mined, `eth_call` ignores the mempool (admission uses the pending balance),
      block time/number differ, and the real gas limit may be lower. Checks stay
      enforced where money actually moves: admission and block execution.
- [x] `SpecId`: Shanghai, and the acknowledged "Frontier consensus, Shanghai
      execution" compromise. `prevrandao` = parent hash (no `mix_hash` field).
- [x] `basefee = 0`: Breeja has no EIP-1559 burn, so any other value would destroy
      ETH and break supply conservation.
- [x] Estimation safety margin: 5% on top of the binary-search minimum, none for a
      plain transfer (21,000 is exact). The search assumes more gas never hurts; a
      contract that branches on `gasleft()` can break that.

## 12. What does not work / known limitations

(Write this honestly as you go. Honesty is graded separately and heavily.)

- An invalid transaction that `execute_block` skips is not removed from the
  mempool, so it is retried every block (pre-existing behaviour). It cannot happen
  to a transaction that was valid at admission unless the sender's funds or nonce
  changed since, but it is a real defect.
- Blocks are filled using each transaction's gas *limit*, which is conservative: a
  block can be sealed with room left because the limits of queued transactions
  did not fit, even though their real usage would have.
- The state and storage roots are flat hashes, not Merkle-Patricia tries: they
  commit to all contents but cannot prove one account or slot.
- `eth_call` on `pending` uses the latest block's state, not the mempool. A call
  with `gasPrice` set ignores it (price is always 0 in a simulation).
- `eth_getLogs` is capped at 10,000 blocks per request, an arbitrary safety limit.
- `eth_estimateGas` assumes more gas never makes a call fail; a contract that
  branches on `gasleft()` can break that, and the 5% margin is a judgement call.
- `state_history` still clones every account, including its storage, on every block
  (Part 0 Q4). Code is shared; storage is not. Memory grows with blocks times state.
- The README's `cast send` example has not been run (no Foundry here); the same
  RPC sequence was verified with a Python signer.
- Execution runs under Shanghai rules while consensus is otherwise Frontier-like
  (5 ETH reward, pi-million gas limit, no fee market): the deliberate compromise
  from Pitfall 5, not a real chain.
