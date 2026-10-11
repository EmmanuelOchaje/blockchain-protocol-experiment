use std::collections::HashMap;

use alloy::primitives::{Address, B256};
use primitives::{
    constants::{BLOCK_REWARD, CHAIN_ID},
    node::{
        Account, PendingTransaction, ReceiptLog, Transaction, TransactionReceipt, hash_list_root,
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

    for tx in batch {
        match execute_transaction(&mut db, block, tx) {
            Ok(mut outcome) => {
                for log in &mut outcome.logs {
                    log.log_index = log_count;
                    log_count += 1;
                }
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
}
