use std::collections::HashMap;

use alloy::primitives::{Address, B256, Bloom, Bytes, U256};
use primitives::{
    constants::{BLOCK_GAS_LIMIT, BLOCK_REWARD, CHAIN_ID, TRANSFER_GAS},
    node::{
        Account, Block, PendingTransaction, ReceiptLog, Transaction, TransactionReceipt,
        bloom_of_logs, hash_list_root, intrinsic_gas,
    },
};
use revm::{
    Context, DatabaseCommit, ExecuteEvm, MainBuilder, MainContext,
    context::{BlockEnv, CfgEnv, TxEnv},
    primitives::{TxKind, hardfork::SpecId},
};

use crate::evm::BreejaDb;

/// The fork rules transactions execute under.
///
/// Breeja is thematically Frontier: 5 ETH block rewards, "pi million" gas limit, no
/// fee market. But Frontier predates almost every opcode a modern Solidity compiler
/// emits (`PUSH0`, for one), so contracts compiled today would fail under it.
/// Shanghai has everything current `solc` needs without the blob-gas fields Cancun
/// requires. "Frontier consensus rules, Shanghai execution rules" is not a real
/// chain; it is a deliberate teaching compromise.
const SPEC: SpecId = SpecId::SHANGHAI;

/// What executing a batch of transactions produced.
pub struct Execution {
    /// Account state after the batch, ready to become the new head state.
    pub accounts: HashMap<Address, Account>,
    /// The chain-wide code map after the batch, including any new contracts.
    pub code: HashMap<B256, alloy::primitives::Bytes>,
    pub transactions: Vec<Transaction>,
    pub receipts: Vec<TransactionReceipt>,
    pub gas_used: u64,
    /// Bloom filter over every log in the block.
    pub logs_bloom: Bloom,
    pub transaction_root: B256,
    pub receipts_root: B256,
    pub state_root: B256,
}

/// Everything about the block being built that a transaction can observe.
///
/// Execution happens *before* mining, so none of this can include the block's own
/// hash: it does not exist yet, and depends on the state root that execution
/// produces. Anything the EVM reads about "this block" has to be known before the
/// nonce is found.
#[derive(Debug, Clone)]
pub struct BlockContext {
    pub number: u64,
    pub timestamp: u64,
    pub parent_hash: B256,
    pub gas_limit: u64,
    pub difficulty: u8,
    pub miner: Address,
}

impl BlockContext {
    /// The environment a call "at" an existing block runs in: that block's own
    /// header values.
    pub fn from_block(block: &Block) -> Self {
        Self {
            number: block.number,
            timestamp: block.timestamp,
            parent_hash: block.parent_hash,
            gas_limit: block.gas_limit,
            difficulty: block.difficulty,
            miner: block.miner,
        }
    }

    fn block_env(&self) -> BlockEnv {
        BlockEnv {
            number: alloy::primitives::U256::from(self.number),
            // REVM pays the transaction fee to this address itself.
            beneficiary: self.miner,
            timestamp: alloy::primitives::U256::from(self.timestamp),
            gas_limit: self.gas_limit,
            // Breeja is pre-EIP-1559: one fixed gas price, no burn. Any non-zero
            // base fee would burn part of every fee and break supply conservation.
            basefee: 0,
            difficulty: alloy::primitives::U256::from(self.difficulty),
            // Post-merge, DIFFICULTY becomes PREVRANDAO. Breeja has no beacon chain
            // to supply one, so the parent block's hash stands in: it is the one
            // block-derived value that already exists when execution runs.
            prevrandao: Some(self.parent_hash),
            ..Default::default()
        }
    }
}

/// What one executed transaction cost and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOutcome {
    pub gas_used: u64,
    /// `false` if the transaction ran but reverted or halted. Such a transaction is
    /// still valid, still mined, and the sender still pays for the gas it used.
    pub success: bool,
    /// Set when a creation succeeded.
    pub contract_address: Option<Address>,
    /// Empty unless the transaction succeeded.
    pub logs: Vec<ReceiptLog>,
}

/// Runs one transaction through REVM against `db`, committing its effects.
///
/// An `Err` means the transaction is *invalid* (wrong nonce, cannot afford the
/// up-front gas reservation, ...). Nothing was changed and the transaction should
/// not be mined. A transaction that is valid but fails while running is an `Ok`
/// with `success: false`, which is a different thing.
///
/// REVM does the whole state transition: it debits the sender, credits the
/// recipient, credits the miner the fee, refunds unused gas, and bumps the nonce.
/// None of that is done by hand here, because doing it twice mints or loses ETH.
pub fn execute_transaction(
    db: &mut BreejaDb,
    block: &BlockContext,
    tx: &PendingTransaction,
) -> Result<TxOutcome, anyhow::Error> {
    let tx_env = TxEnv {
        tx_type: 0, // legacy
        caller: tx.from,
        gas_limit: tx.gas,
        gas_price: tx.gas_price,
        kind: match tx.to {
            Some(to) => TxKind::Call(to),
            None => TxKind::Create,
        },
        data: tx.input.clone(),
        value: tx.value,
        nonce: tx.nonce,
        chain_id: Some(tx.chain_id),
        ..Default::default()
    };
    let cfg = CfgEnv::new_with_spec(SPEC).with_chain_id(CHAIN_ID);

    let mut evm = Context::mainnet()
        .with_db(&mut *db)
        .with_block(block.block_env())
        .with_cfg(cfg)
        .build_mainnet();

    let outcome = evm
        .transact(tx_env)
        .map_err(|error| anyhow::anyhow!("invalid transaction {}: {error}", tx.hash))?;

    let result = outcome.result;
    let state = outcome.state;
    // The EVM holds `&mut` on the database for as long as it lives, so it must be
    // dropped before the changes can be committed.
    drop(evm);
    db.commit(state);

    let success = result.is_success();
    // A reverted or halted call's logs are undone with the rest of its effects, so
    // the receipt of a failed transaction carries none.
    let logs = if success {
        result
            .logs()
            .iter()
            .map(|log| ReceiptLog {
                address: log.address,
                topics: log.data.topics().to_vec(),
                data: log.data.data.clone(),
                log_index: 0, // numbered block-wide by execute_block
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok(TxOutcome {
        gas_used: result.tx_gas_used(),
        success,
        contract_address: result.created_address(),
        logs,
    })
}

/// What a read-only call asks the EVM to do.
#[derive(Debug, Clone, Default)]
pub struct CallSpec {
    pub from: Address,
    /// `None` runs `input` as init code, without storing anything.
    pub to: Option<Address>,
    pub value: U256,
    pub input: Bytes,
    pub gas: u64,
}

/// How a simulated call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallResult {
    Success { output: Bytes, gas_used: u64 },
    /// The contract executed `REVERT`; `output` usually carries the reason.
    Revert { output: Bytes, gas_used: u64 },
    /// The EVM stopped it (out of gas, invalid opcode, ...).
    Halt { reason: String, gas_used: u64 },
}

impl CallResult {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    pub fn gas_used(&self) -> u64 {
        match self {
            Self::Success { gas_used, .. }
            | Self::Revert { gas_used, .. }
            | Self::Halt { gas_used, .. } => *gas_used,
        }
    }
}

/// Runs a call against `db` and throws every change away.
///
/// This is the same REVM and the same `BreejaDb` as real execution, with three
/// differences that make it a *simulation*: the result is never committed, the gas
/// price is zero, and the nonce and balance checks are off. That lets any address,
/// funded or not, ask a question. It also means a successful simulation does not
/// prove the real transaction would be admitted or mined (see EVM-NOTES.md).
///
/// `Err` means REVM refused to start (for example the gas limit is below the
/// intrinsic cost), as opposed to a call that ran and failed.
pub fn simulate_call(
    db: &mut BreejaDb,
    block: &BlockContext,
    spec: &CallSpec,
) -> Result<CallResult, anyhow::Error> {
    let tx_env = TxEnv {
        tx_type: 0,
        caller: spec.from,
        gas_limit: spec.gas,
        gas_price: 0,
        kind: match spec.to {
            Some(to) => TxKind::Call(to),
            None => TxKind::Create,
        },
        data: spec.input.clone(),
        value: spec.value,
        chain_id: Some(CHAIN_ID),
        ..Default::default()
    };
    let mut cfg = CfgEnv::new_with_spec(SPEC).with_chain_id(CHAIN_ID);
    cfg.disable_nonce_check = true;
    cfg.disable_balance_check = true;

    let mut evm = Context::mainnet()
        .with_db(&mut *db)
        .with_block(block.block_env())
        .with_cfg(cfg)
        .build_mainnet();

    // `transact` returns the changes without applying them; dropping them is what
    // makes this a read.
    let outcome = evm
        .transact(tx_env)
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    use revm::context::result::ExecutionResult;
    Ok(match outcome.result {
        ExecutionResult::Success { gas, output, .. } => CallResult::Success {
            output: output.into_data(),
            gas_used: gas.tx_gas_used(),
        },
        ExecutionResult::Revert { gas, output, .. } => CallResult::Revert {
            output,
            gas_used: gas.tx_gas_used(),
        },
        ExecutionResult::Halt { reason, gas, .. } => CallResult::Halt {
            reason: format!("{reason:?}"),
            gas_used: gas.tx_gas_used(),
        },
    })
}

/// Padding added to a computed estimate, in percent. State can move between an
/// estimate and the block that mines the transaction, so the exact minimum is a
/// bit risky to sign with. Plain transfers are exempt: 21,000 is exact.
pub const ESTIMATE_MARGIN_PERCENT: u64 = 5;

/// Why an estimate could not be produced.
#[derive(Debug)]
pub enum EstimateError {
    /// The call fails even with the maximum gas, so no limit would help.
    Fails(CallResult),
    /// The request cannot run at all (for example `spec.gas` is below the
    /// intrinsic cost).
    Invalid(anyhow::Error),
}

/// Finds the smallest gas limit with which `spec` succeeds, by binary search.
///
/// `spec.gas` is the ceiling. Gas needed is not simply gas *used*: refunds are
/// subtracted from used gas, and the EVM keeps 1/64 of gas back for a call's
/// caller, so a limit equal to gas used can still fail. Searching is the honest
/// way to find the real threshold. It assumes "more gas never hurts", which is
/// true for most contracts but not all (one that branches on `gasleft()` can break
/// it), so the result is an estimate, not a proof.
pub fn estimate_gas(
    db: &mut BreejaDb,
    block: &BlockContext,
    spec: &CallSpec,
) -> Result<u64, EstimateError> {
    let intrinsic = intrinsic_gas(&spec.input, spec.to.is_none());
    if spec.gas < intrinsic {
        return Err(EstimateError::Invalid(anyhow::anyhow!(
            "gas limit {} is below the intrinsic cost ({intrinsic})",
            spec.gas
        )));
    }

    // A transfer to an account with no code runs no opcodes: its cost is exactly
    // the intrinsic cost.
    if let Some(to) = spec.to {
        let has_code = db
            .accounts
            .get(&to)
            .is_some_and(|account| !primitives::node::is_empty_code_hash(&account.code_hash));
        if spec.input.is_empty() && !has_code {
            return Ok(TRANSFER_GAS);
        }
    }

    let ceiling = simulate_call(db, block, spec).map_err(EstimateError::Invalid)?;
    if !ceiling.is_success() {
        return Err(EstimateError::Fails(ceiling));
    }

    let succeeds = |db: &mut BreejaDb, gas: u64| {
        let attempt = CallSpec {
            gas,
            ..spec.clone()
        };
        matches!(simulate_call(db, block, &attempt), Ok(result) if result.is_success())
    };

    // Invariant: `low` fails, `high` succeeds.
    let mut low = intrinsic - 1;
    let mut high = spec.gas;
    while high - low > 1 {
        let mid = low + (high - low) / 2;
        if succeeds(db, mid) {
            high = mid;
        } else {
            low = mid;
        }
    }

    let padded = high + high * ESTIMATE_MARGIN_PERCENT / 100;
    Ok(padded.min(spec.gas))
}

/// The block gas limit, as the default and the ceiling for a call's gas.
pub const CALL_GAS_CEILING: u64 = BLOCK_GAS_LIMIT;

/// Pays the miner for sealing a block.
///
/// This is the only way new ETH enters the supply, and it is what makes mining
/// worth the electricity. Breeja pays Frontier's 5 ETH; Ethereum reduced this
/// twice before dropping it entirely at the merge. REVM knows nothing about block
/// rewards, so this is the node's own policy and is paid here.
pub fn pay_block_reward(
    accounts: &mut HashMap<Address, Account>,
    miner: Address,
) -> Result<(), anyhow::Error> {
    accounts.entry(miner).or_default().credit(BLOCK_REWARD)?;
    Ok(())
}

/// Executes a batch of pending transactions, producing the state and receipts the
/// next block will commit to.
///
/// A transaction that is *invalid* is skipped, not included: a nonce another
/// transaction already used, or a sender who can no longer afford it. Those are
/// races, and better dropped than mined. A transaction that is valid but reverts
/// or halts is *included* with a failed receipt, because that is what Ethereum
/// does: the work was done, the gas is spent, and the sender pays for it.
pub fn execute_block(
    mut db: BreejaDb,
    batch: &[PendingTransaction],
    block: &BlockContext,
) -> Execution {
    let mut transactions = Vec::new();
    let mut receipts = Vec::new();
    let mut gas_used = 0u64;
    let mut log_count = 0u64;
    let mut logs_bloom = Bloom::default();

    for tx in batch {
        // The miner picks a batch whose gas limits fit the block, so this should
        // never trigger; it is the last line of defence for the invariant that a
        // block never uses more gas than its limit.
        if tx.gas > block.gas_limit - gas_used {
            tracing::warn!(tx = %tx.hash, "transaction does not fit in the remaining block gas");
            break;
        }
        match execute_transaction(&mut db, block, tx) {
            Ok(mut outcome) => {
                for log in &mut outcome.logs {
                    log.log_index = log_count;
                    log_count += 1;
                }
                let receipt_bloom = bloom_of_logs(&outcome.logs);
                logs_bloom.accrue_bloom(&receipt_bloom);
                let index = transactions.len() as u64;
                gas_used += outcome.gas_used;

                transactions.push(Transaction {
                    hash: tx.hash,
                    // The block hash is unknown until the block is sealed, so it
                    // is stamped in afterwards.
                    block_hash: B256::ZERO,
                    block_number: block.number,
                    transaction_index: index,
                    chain_id: tx.chain_id,
                    from: tx.from,
                    to: tx.to,
                    value: tx.value,
                    nonce: tx.nonce,
                    gas: tx.gas,
                    gas_price: tx.gas_price,
                    input: tx.input.clone(),
                    v: tx.v,
                    r: tx.r,
                    s: tx.s,
                });

                receipts.push(TransactionReceipt {
                    transaction_hash: tx.hash,
                    transaction_index: index,
                    block_hash: B256::ZERO,
                    block_number: block.number,
                    from: tx.from,
                    to: tx.to,
                    gas_used: outcome.gas_used,
                    cummulative_gas_used: gas_used,
                    effective_gas_price: tx.gas_price,
                    contract_address: outcome.contract_address,
                    logs: outcome.logs,
                    logs_bloom: receipt_bloom,
                    status: outcome.success as u8,
                });
            }
            Err(error) => {
                tracing::warn!(tx = %tx.hash, %error, "dropping transaction that no longer applies");
            }
        }
    }

    // The miner's reward is paid after the transactions, so a miner cannot spend
    // it inside the very block that grants it.
    if let Err(error) = pay_block_reward(&mut db.accounts, block.miner) {
        tracing::error!(%error, "failed to pay block reward");
    }

    let tx_hashes: Vec<B256> = transactions.iter().map(|tx| tx.hash).collect();
    let receipt_hashes: Vec<B256> = receipts.iter().map(|r| r.transaction_hash).collect();

    Execution {
        transaction_root: hash_list_root(&tx_hashes),
        receipts_root: hash_list_root(&receipt_hashes),
        state_root: primitives::node::state_root_of(&db.accounts),
        accounts: db.accounts,
        code: db.code,
        transactions,
        receipts,
        gas_used,
        logs_bloom,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use primitives::constants::{DEV_ADDRESS, DEV_ADDRESS_1, GAS_PRICE, TRANSFER_GAS};

    use super::*;

    fn one_eth() -> U256 {
        U256::from(10u64).pow(U256::from(18))
    }

    fn funded(balance: U256) -> BreejaDb {
        let mut db = BreejaDb::default();
        db.accounts.insert(DEV_ADDRESS, Account::new(balance));
        db
    }

    fn block(miner: Address) -> BlockContext {
        BlockContext {
            number: 1,
            timestamp: 1,
            parent_hash: B256::repeat_byte(0x01),
            gas_limit: 3_141_592,
            difficulty: 4,
            miner,
        }
    }

    fn transfer(value: U256, nonce: u64) -> PendingTransaction {
        PendingTransaction {
            hash: B256::repeat_byte(0xab),
            from: DEV_ADDRESS,
            to: Some(DEV_ADDRESS_1),
            input: Default::default(),
            value,
            nonce,
            gas: TRANSFER_GAS,
            gas_price: GAS_PRICE,
            chain_id: 1,
            v: 0,
            r: U256::ZERO,
            s: U256::ZERO,
        }
    }

    /// The parity test for Milestone 2: the EVM must reproduce the behaviour the
    /// hand-written transfer had. A plain ETH transfer costs exactly 21,000 gas.
    #[test]
    fn a_plain_transfer_through_revm_costs_exactly_21000_gas() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth() * U256::from(10u64));

        let outcome = execute_transaction(&mut db, &block(miner), &transfer(one_eth(), 0)).unwrap();

        assert_eq!(outcome.gas_used, TRANSFER_GAS);
        assert!(outcome.success);
    }

    #[test]
    fn a_transfer_moves_value_charges_a_fee_and_bumps_the_nonce() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth() * U256::from(10u64));
        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);

        execute_transaction(&mut db, &block(miner), &transfer(one_eth(), 0)).unwrap();

        let sender = &db.accounts[&DEV_ADDRESS];
        assert_eq!(
            sender.balance,
            one_eth() * U256::from(10u64) - one_eth() - fee
        );
        // Exactly one nonce consumed: if both REVM and our own code bumped it, this
        // would be 2 (Pitfall 3 in the project brief).
        assert_eq!(sender.nonce, 1);
        assert_eq!(db.accounts[&DEV_ADDRESS_1].balance, one_eth());
        // Exactly one fee paid to the miner: if both REVM and our own code credited
        // it, this would be double (Pitfall 1).
        assert_eq!(db.accounts[&miner].balance, fee);
    }

    #[test]
    fn no_eth_is_created_or_destroyed_by_a_transfer() {
        let miner = Address::repeat_byte(0x99);
        let start = one_eth() * U256::from(10u64);
        let mut db = funded(start);

        execute_transaction(&mut db, &block(miner), &transfer(one_eth(), 0)).unwrap();

        let total: U256 = db
            .accounts
            .values()
            .map(|a| a.balance)
            .fold(U256::ZERO, |a, b| a + b);
        assert_eq!(total, start);
    }

    #[test]
    fn a_transfer_the_sender_cannot_afford_is_rejected_and_changes_nothing() {
        let miner = Address::repeat_byte(0x99);
        // Enough for the value but not the fee on top.
        let mut db = funded(one_eth());

        assert!(execute_transaction(&mut db, &block(miner), &transfer(one_eth(), 0)).is_err());

        // An invalid transaction must leave no trace: balance and nonce untouched.
        assert_eq!(db.accounts[&DEV_ADDRESS].balance, one_eth());
        assert_eq!(db.accounts[&DEV_ADDRESS].nonce, 0);
        assert!(!db.accounts.contains_key(&DEV_ADDRESS_1));
    }

    #[test]
    fn a_transfer_with_the_wrong_nonce_is_rejected() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth() * U256::from(10u64));
        assert!(execute_transaction(&mut db, &block(miner), &transfer(one_eth(), 7)).is_err());
    }

    #[test]
    fn executing_a_block_pays_the_reward_and_records_receipts() {
        let miner = Address::repeat_byte(0x99);
        let db = funded(one_eth() * U256::from(10u64));
        let batch = vec![transfer(one_eth(), 0)];

        let execution = execute_block(db, &batch, &block(miner));

        assert_eq!(execution.transactions.len(), 1);
        assert_eq!(execution.receipts.len(), 1);
        assert_eq!(execution.receipts[0].status, 1);
        assert_eq!(execution.gas_used, TRANSFER_GAS);
        assert_eq!(execution.receipts[0].cummulative_gas_used, TRANSFER_GAS);

        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);
        assert_eq!(execution.accounts[&miner].balance, BLOCK_REWARD + fee);

        // Non-empty roots, because this block committed to a transaction.
        assert_ne!(execution.transaction_root, B256::ZERO);
        assert_ne!(execution.state_root, B256::ZERO);
    }

    #[test]
    fn an_empty_block_still_pays_the_miner_but_commits_to_no_transactions() {
        let miner = Address::repeat_byte(0x99);
        let execution = execute_block(funded(one_eth()), &[], &block(miner));

        assert!(execution.transactions.is_empty());
        assert_eq!(execution.gas_used, 0);
        assert_eq!(execution.transaction_root, B256::ZERO);
        assert_eq!(execution.accounts[&miner].balance, BLOCK_REWARD);
    }

    #[test]
    fn a_transaction_that_no_longer_applies_is_skipped_not_mined() {
        let miner = Address::repeat_byte(0x99);
        let db = funded(one_eth() * U256::from(10u64));
        // Two transactions claiming the same nonce: only the first can apply.
        let batch = vec![transfer(one_eth(), 0), transfer(one_eth(), 0)];

        let execution = execute_block(db, &batch, &block(miner));
        assert_eq!(execution.transactions.len(), 1);
    }

    // ---- Milestone 3: contracts ----

    const STORE_42_INIT: [u8; 23] =
        alloy::hex!("600b600c600039600b6000f3602a60005560006000a000");
    const ALWAYS_REVERTS_INIT: [u8; 17] = alloy::hex!("6005600c60003960056000f360006000fd");

    fn create(init: &[u8], nonce: u64, tag: u8) -> PendingTransaction {
        PendingTransaction {
            hash: B256::repeat_byte(tag),
            to: None,
            input: init.to_vec().into(),
            gas: 200_000,
            ..transfer(U256::ZERO, nonce)
        }
    }

    fn call(to: Address, nonce: u64, gas: u64, tag: u8) -> PendingTransaction {
        PendingTransaction {
            hash: B256::repeat_byte(tag),
            to: Some(to),
            gas,
            ..transfer(U256::ZERO, nonce)
        }
    }

    #[test]
    fn a_deployment_yields_a_contract_address_code_and_a_receipt() {
        let miner = Address::repeat_byte(0x99);
        let db = funded(one_eth() * U256::from(10u64));
        let execution = execute_block(db, &[create(&STORE_42_INIT, 0, 1)], &block(miner));

        let receipt = &execution.receipts[0];
        let contract = DEV_ADDRESS.create(0);
        assert_eq!(receipt.status, 1);
        assert_eq!(receipt.to, None);
        assert_eq!(receipt.contract_address, Some(contract));
        assert_eq!(receipt.gas_used, 55_522); // the brief's figure on REVM 43

        // The runtime code (not the init code) is stored under its hash.
        let runtime = alloy::hex!("602a60005560006000a000");
        let hash = alloy::primitives::keccak256(runtime);
        assert_eq!(execution.accounts[&contract].code_hash, hash);
        assert_eq!(execution.code[&hash].as_ref(), runtime);
        assert_eq!(execution.transactions[0].input.as_ref(), STORE_42_INIT);
    }

    #[test]
    fn calling_a_contract_writes_storage_and_emits_a_log() {
        let miner = Address::repeat_byte(0x99);
        let db = funded(one_eth() * U256::from(10u64));
        let contract = DEV_ADDRESS.create(0);
        let batch = [
            create(&STORE_42_INIT, 0, 1),
            call(contract, 1, 100_000, 2),
        ];
        let execution = execute_block(db, &batch, &block(miner));

        let receipt = &execution.receipts[1];
        assert_eq!(receipt.status, 1);
        assert_eq!(receipt.gas_used, 43_487);
        assert_eq!(receipt.logs.len(), 1);
        assert_eq!(receipt.logs[0].address, contract);
        assert_eq!(execution.accounts[&contract].storage[&U256::ZERO], U256::from(42));
        // Gas accumulates across the block.
        assert_eq!(receipt.cummulative_gas_used, 55_522 + 43_487);
    }

    #[test]
    fn a_reverting_call_is_mined_charged_and_marked_failed() {
        let miner = Address::repeat_byte(0x99);
        let db = funded(one_eth() * U256::from(10u64));
        let contract = DEV_ADDRESS.create(0);
        let batch = [
            create(&ALWAYS_REVERTS_INIT, 0, 1),
            call(contract, 1, 100_000, 2),
        ];
        let execution = execute_block(db, &batch, &block(miner));

        // Included, not skipped (this is Part 0 Q2).
        assert_eq!(execution.transactions.len(), 2);
        let receipt = &execution.receipts[1];
        assert_eq!(receipt.status, 0);
        assert!(receipt.gas_used >= TRANSFER_GAS);
        assert!(receipt.logs.is_empty());
        // The sender paid for it and the nonce advanced.
        assert_eq!(execution.accounts[&DEV_ADDRESS].nonce, 2);
    }

    #[test]
    fn running_out_of_gas_burns_the_whole_limit_but_is_still_mined() {
        let miner = Address::repeat_byte(0x99);
        let db = funded(one_eth() * U256::from(10u64));
        let contract = DEV_ADDRESS.create(0);
        // Enough for the 21,000 intrinsic cost, not for the SSTORE.
        let batch = [create(&STORE_42_INIT, 0, 1), call(contract, 1, 30_000, 2)];
        let execution = execute_block(db, &batch, &block(miner));

        let receipt = &execution.receipts[1];
        assert_eq!(receipt.status, 0);
        assert_eq!(receipt.gas_used, 30_000);
        assert!(!execution.accounts[&contract].storage.contains_key(&U256::ZERO));
    }

    #[test]
    fn contract_execution_neither_creates_nor_destroys_eth() {
        let miner = Address::repeat_byte(0x99);
        let start = one_eth() * U256::from(10u64);
        let db = funded(start);
        let contract = DEV_ADDRESS.create(0);
        let batch = [
            create(&STORE_42_INIT, 0, 1),
            call(contract, 1, 100_000, 2),
            call(contract, 2, 30_000, 3),
        ];
        let execution = execute_block(db, &batch, &block(miner));
        let total: U256 = execution.accounts.values().map(|a| a.balance).sum();
        assert_eq!(total, start + BLOCK_REWARD);
    }

    #[test]
    fn the_estimate_is_the_real_threshold_and_a_simulation_changes_nothing() {
        let miner = Address::repeat_byte(0x99);
        let execution = execute_block(
            funded(one_eth() * U256::from(10u64)),
            &[create(&STORE_42_INIT, 0, 1)],
            &block(miner),
        );
        let mut db = BreejaDb {
            accounts: execution.accounts,
            code: execution.code,
            block_hashes: Default::default(),
        };
        let before = db.accounts.clone();
        let contract = DEV_ADDRESS.create(0);
        let spec = CallSpec {
            from: Address::repeat_byte(0x55), // has no ETH at all
            to: Some(contract),
            gas: 3_000_000,
            ..Default::default()
        };

        let estimate = estimate_gas(&mut db, &block(miner), &spec).unwrap();
        // Unpadded, the answer is the smallest limit that works.
        let exact = (1..=estimate)
            .rev()
            .find(|gas| {
                let attempt = CallSpec { gas: *gas, ..spec.clone() };
                !matches!(simulate_call(&mut db, &block(miner), &attempt), Ok(r) if r.is_success())
            })
            .unwrap()
            + 1;
        assert!(exact >= 43_487);
        assert_eq!(estimate, (exact + exact * ESTIMATE_MARGIN_PERCENT / 100).min(3_000_000));

        // Nothing was written, no matter how many simulations ran.
        assert_eq!(db.accounts, before);
    }

    #[test]
    fn receipts_and_the_block_carry_a_bloom_that_matches_their_logs() {
        let miner = Address::repeat_byte(0x99);
        let contract = DEV_ADDRESS.create(0);
        let batch = [
            create(&STORE_42_INIT, 0, 1),
            call(contract, 1, 100_000, 2),
        ];
        let execution = execute_block(funded(one_eth() * U256::from(10u64)), &batch, &block(miner));

        // The deployment and a plain transfer emit nothing: empty bloom.
        assert_eq!(execution.receipts[0].logs_bloom, Bloom::default());
        // The call emitted one log, so its bloom is non-zero and holds the emitter.
        let receipt = &execution.receipts[1];
        assert_eq!(receipt.logs_bloom, primitives::node::bloom_of_logs(&receipt.logs));
        assert_ne!(receipt.logs_bloom, Bloom::default());
        assert!(primitives::node::bloom_may_contain(&receipt.logs_bloom, contract.as_slice()));
        // The block's bloom is the union of its receipts'.
        assert_eq!(execution.logs_bloom, receipt.logs_bloom);

        let no_logs = execute_block(funded(one_eth()), &[transfer(U256::from(1u64), 0)], &block(miner));
        assert_eq!(no_logs.logs_bloom, Bloom::default());
    }

    #[test]
    fn execution_never_runs_a_transaction_that_cannot_fit_in_the_block() {
        let miner = Address::repeat_byte(0x99);
        let mut too_big = transfer(U256::from(1u64), 0);
        too_big.gas = 4_000_000; // above the 3,141,592 block limit
        let execution = execute_block(funded(one_eth() * U256::from(10u64)), &[too_big], &block(miner));
        assert!(execution.transactions.is_empty());
        assert!(execution.gas_used <= 3_141_592);
    }
}
