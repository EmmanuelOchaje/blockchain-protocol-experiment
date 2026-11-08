# Testing the EVM integration

This page is for whoever reviews `feat/evm`. It says how to check that everything
builds and works, and which test proves which part of the project.

## One command

From `toy-node/`:

```bash
scripts/verify.sh
```

It runs, in order, and stops at the first failure:

| Step | Command | What it proves |
| --- | --- | --- |
| 1 | `cargo build --release` | Deliverable 1: the release build compiles |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` | Deliverable 1: no clippy warnings (warnings are treated as errors) |
| 3 | `cargo test --workspace` | Deliverables 2–4: all 89 tests pass |
| 4 | `scripts/e2e-cast.sh` | A live node, driven only by Foundry's `cast`, does everything in the brief |

Step 4 needs [Foundry](https://getfoundry.sh) (`cast`) and `jq`. If either is
missing, step 4 is skipped with a note and steps 1–3 still run. The Rust tests
cover the same behaviour through a JSON-RPC client.

Expected end of the output:

```text
ALL 35 CHECKS PASSED
######## verification complete
```

## The live end-to-end check (`scripts/e2e-cast.sh`)

The script builds the node, starts a fresh `breeja --dev`, and uses only standard
tooling against it. Every line below is an assertion; the script exits non-zero
on the first one that fails and always stops the node it started. Use
`PORT=8600 scripts/e2e-cast.sh` if 8545 is taken.

| Milestone | What `cast` does | What is asserted |
| --- | --- | --- |
| setup | `cast chain-id`, `cast gas-price` | chain id 1, fixed 1 gwei |
| M2 | `cast send --value 1ether` (the README command) | status 1, exactly 21,000 gas, recipient +1 ETH, `cast estimate` = 21,000, `cast block` parses |
| M3 | `cast send --create examples/Counter.bin` | status 1, `contractAddress` set, `to` is null |
| M5 | `cast code` | deployed code equals `examples/Counter.runtime.bin`; `0x` for an account with no code |
| M4 | `cast call count()` | reads 0 without changing state |
| M4 | `cast estimate increment()` | above 21,000 and at least the gas actually used |
| M3 | `cast send increment()` | status 1, one log, non-zero `logsBloom`, `count()` = 1, `cast storage` slot 0 = 1 |
| M5 | `cast logs` by address and event | both events found, indexed topic is the sender, data is the new count; none for another address |
| M4 | `cast call set(500)` | revert reported as error code 3 with data `0x8aa0e18c` (`TooBig()`) |
| M3 | `cast send set(500)` with a gas limit | mined with status 0, sender charged, no logs, nonce advanced, `count()` unchanged |
| M6 | `cast block`, `cast tx` | block gas within its limit; `cast tx` reads the transaction |
| all | node log | no warnings or errors |

`cast` refuses to send a transaction whose gas estimate reverts. That is
`eth_estimateGas` working, so the reverting send passes `--gas-limit` explicitly.

## The Rust tests, by milestone

Run a single one with `cargo test <name>`.

### M1: state knows about code and storage (`crates/node/src/evm.rs`, `crates/primitives/src/node.rs`)

| Test | Proves |
| --- | --- |
| `evm::tests::basic_returns_the_balance_and_nonce_of_a_funded_account` | REVM reads an account through `BreejaDb` |
| `evm::tests::basic_reports_an_unknown_address_as_no_account_not_an_empty_one` | unknown address is `None`, not an empty account |
| `evm::tests::code_and_storage_come_back_through_the_database` | `code_by_hash` and `storage` return what was stored |
| `evm::tests::block_hash_answers_for_known_blocks_and_zero_for_unknown_ones` | the `BLOCKHASH` lookup |
| `evm::tests::commit_persists_balance_nonce_code_and_a_storage_slot` | `DatabaseCommit` writes everything back |
| `evm::tests::commit_ignores_accounts_that_were_only_read` | read-only (untouched) accounts are not written |
| `evm::tests::writing_zero_to_a_slot_removes_it` | a zero slot is deleted, as Ethereum does |
| `evm::tests::a_selfdestructed_account_is_removed_entirely` | self-destruct removes the account |
| `evm::tests::a_snapshot_copies_accounts_code_and_recent_block_hashes_out_of_state` | the miner's snapshot is complete |
| `evm::tests::a_real_evm_deploys_and_calls_a_contract_through_the_database` | REVM end to end on our database |
| `node::tests::code_is_content_addressed_and_no_code_reads_as_empty` | code keyed by `code_hash` |
| `node::tests::storage_reads_zero_until_written_and_has_history` | storage reads and history |
| `node::tests::storage_is_committed_through_a_derived_storage_root` | `storage_root` derived from storage |
| `node::tests::two_states_differing_in_one_storage_slot_have_different_state_roots` | storage is in the state root |
| `node::tests::two_states_differing_only_in_code_have_different_state_roots` | code is in the state root |

### M2: transfers through REVM (`crates/node/src/executor.rs`)

| Test | Proves |
| --- | --- |
| `executor::tests::a_plain_transfer_through_revm_costs_exactly_21000_gas` | gas parity with the old executor |
| `executor::tests::a_transfer_moves_value_charges_a_fee_and_bumps_the_nonce` | REVM bumps the nonce (pitfall 3) and charges the fee |
| `executor::tests::no_eth_is_created_or_destroyed_by_a_transfer` | the miner is not paid twice (pitfall 1) |
| `executor::tests::executing_a_block_pays_the_reward_and_records_receipts` | block reward is still ours, receipts recorded |
| `executor::tests::an_empty_block_still_pays_the_miner_but_commits_to_no_transactions` | empty blocks |
| `executor::tests::a_transfer_with_the_wrong_nonce_is_rejected` | invalid transaction is an error |
| `executor::tests::a_transfer_the_sender_cannot_afford_is_rejected_and_changes_nothing` | invalid transaction changes nothing |
| `executor::tests::a_transaction_that_no_longer_applies_is_skipped_not_mined` | invalid transactions are skipped |

### M3: contracts and failed transactions

| Test | Proves |
| --- | --- |
| `executor::tests::a_deployment_yields_a_contract_address_code_and_a_receipt` | creation |
| `executor::tests::calling_a_contract_writes_storage_and_emits_a_log` | calls, storage writes, logs |
| `executor::tests::a_reverting_call_is_mined_charged_and_marked_failed` | revert: mined, charged, status 0 |
| `executor::tests::running_out_of_gas_burns_the_whole_limit_but_is_still_mined` | halt: whole limit burned, still mined |
| `executor::tests::contract_execution_neither_creates_nor_destroys_eth` | ETH conservation with contracts |
| `pool::tests::a_creation_and_calldata_are_now_admissible` | the no-EVM pool checks are gone |
| `pool::tests::a_gas_limit_below_the_intrinsic_cost_is_rejected` | intrinsic gas check |
| `a_contract_can_be_deployed_called_and_failed_calls_are_still_mined` (`node/tests/mining.rs`) | the same, on a live miner |
| `the_node_refuses_transactions_it_cannot_execute` (`node/tests/mining.rs`) | admission rules on a live node |

### M4: `eth_call` and `eth_estimateGas`

| Test | Proves |
| --- | --- |
| `executor::tests::the_estimate_is_the_real_threshold_and_a_simulation_changes_nothing` | estimate is the real threshold; simulation keeps nothing |
| `eth_call_runs_contract_code_and_changes_nothing` | `eth_call` over JSON-RPC |
| `eth_call_reports_a_revert_as_an_error_and_can_read_the_past` | error code 3 with data; historical blocks |
| `eth_estimate_gas_finds_the_gas_a_transaction_needs` | estimate over JSON-RPC, including failures |

### M5: new RPC methods and logs

| Test | Proves |
| --- | --- |
| `eth_get_code_and_eth_get_storage_at_read_a_deployed_contract` | `eth_getCode`, `eth_getStorageAt` |
| `eth_get_logs_filters_by_range_address_and_topics` | `eth_getLogs` filters |
| `eth_gas_price_returns_the_one_fixed_price` | `eth_gasPrice` |
| `params::tests::a_log_filter_accepts_single_values_lists_and_null_wildcards` | filter parsing |
| `node::tests::log_filters_match_by_address_and_by_positional_topics_with_wildcards` | filter matching |

### M6: blocks, gas and blooms

| Test | Proves |
| --- | --- |
| `blocks_are_filled_by_gas_and_never_exceed_the_block_gas_limit` (`node/tests/mining.rs`) | gas-based block fill on a live miner |
| `node::tests::a_block_is_filled_by_gas_and_stops_at_the_first_transaction_that_does_not_fit` | stop, don't skip (nonce order kept) |
| `executor::tests::execution_never_runs_a_transaction_that_cannot_fit_in_the_block` | executor gas guard |
| `pool::tests::a_gas_limit_above_the_block_gas_limit_is_rejected` | admission guard |
| `executor::tests::receipts_and_the_block_carry_a_bloom_that_matches_their_logs` | receipt and block blooms |
| `node::tests::our_bloom_matches_alloys_reference_implementation` | bloom algorithm matches alloy |
| `node::tests::a_bloom_has_no_false_negatives` | bloom correctness |
| `logs_bloom_is_real_for_receipts_and_blocks` | blooms over JSON-RPC |

### Deliverable 4: the Solidity demo

| Test | Proves |
| --- | --- |
| `a_solidity_contract_can_be_deployed_called_estimated_logged_and_reverted` | `examples/Counter.sol` (solc 0.8.24, uses `PUSH0`): deploy, estimate, call, logs, custom-error revert |
| `responses_parse_as_standard_alloy_rpc_types` | blocks, transactions, receipts and logs parse with the RPC types `cast` uses |

### Unchanged behaviour still holds

The remaining tests (proof of work, difficulty, genesis, balances, nonces, block
lookups, response encoding) predate the EVM and still pass. Where an existing
test's expectation had to change, the test carries a comment saying why the old
expectation was wrong.

## What running `cast` found

Until `cast` was run against a live node, the README's `cast send` example was
untested. Running it showed the transaction was mined (status 1), but `cast`
could not read the result:

- receipts used the key `cummulativeGasUsed` instead of `cumulativeGasUsed`;
- blocks had no `sha3Uncles`, `mixHash` or `uncles`.

Both came from the original node, before the EVM work. Both are fixed in
`crates/primitives/src/response.rs`, and `responses_parse_as_standard_alloy_rpc_types`
fails if either comes back.
