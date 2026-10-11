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

## 5. Decisions

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
- [ ] `eth_call`: keep or disable nonce and balance checks (via `CfgEnv`).
- [ ] `SpecId`: Shanghai, and the acknowledged "Frontier consensus, Shanghai
      execution" compromise.
- [ ] `basefee = 0` and why (pre-EIP-1559; any other value burns ETH).
- [ ] Estimation safety margin for `eth_estimateGas`.

## 6. What does not work / known limitations

(Write this honestly as you go. Honesty is graded separately and heavily.)

- After M1 the state root still ignores code and storage contents: `storage_root`
  is not yet derived from `storage`, and `code_hash` is not yet populated by any
  real transaction. Two chains differing only in contract state would currently
  hash the same. Milestone 6 fixes this.
- `BreejaDb` is built and tested, but nothing executes through it yet. The miner
  still uses `apply_transfer` until Milestone 2.
- `state_history` still clones every account, now including its storage, on every
  block (see Q4). Code is shared, storage is not.
