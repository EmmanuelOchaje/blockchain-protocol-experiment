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

## 4. Decisions (fill in as you make them)

- [ ] Code representation: `Bytecode` on the account, or `code_hash` plus shared map.
- [ ] `Database::basic` for an unknown address: `None` or an empty account.
- [ ] `eth_call`: keep or disable nonce and balance checks (via `CfgEnv`).
- [ ] `SpecId`: Shanghai, and the acknowledged "Frontier consensus, Shanghai
      execution" compromise.
- [ ] `basefee = 0` and why (pre-EIP-1559; any other value burns ETH).
- [ ] Estimation safety margin for `eth_estimateGas`.

## 5. What does not work / known limitations

(Write this honestly as you go. Honesty is graded separately and heavily.)
