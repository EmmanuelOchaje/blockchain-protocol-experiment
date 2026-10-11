//! The bridge between Breeja's state and REVM.
//!
//! REVM does not take your state as an argument. It *asks* for what it needs while
//! it runs, through the [`Database`] trait, and hands back a bundle of changes that
//! you apply through [`DatabaseCommit`]. This module implements both on
//! [`BreejaDb`], a *snapshot* of chain state.
//!
//! It is a snapshot and not `BlockState` itself on purpose. REVM needs `&mut` access
//! for the whole of a transaction, while `BlockState` sits behind an `RwLock` shared
//! with the JSON-RPC server. Holding that lock across a long contract call would
//! freeze every RPC request, so the miner copies state out, executes against the
//! copy with no lock held, and only takes the lock again to commit. That is the same
//! pattern the miner already uses with `accounts_snapshot()`.

use std::{collections::HashMap, convert::Infallible};

use alloy::primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, keccak256};
use primitives::node::{Account, BlockState, is_empty_code_hash};
use revm::{
    Database, DatabaseCommit,
    bytecode::Bytecode,
    primitives::{AddressMap, StorageKey, StorageValue},
    state::{Account as EvmAccount, AccountInfo},
};

/// A snapshot of chain state that REVM can read from and write back to.
#[derive(Debug, Clone, Default)]
pub struct BreejaDb {
    pub accounts: HashMap<Address, Account>,
    /// Contract code by `keccak256(code)`. Shared between accounts and never pruned.
    pub code: HashMap<B256, Bytes>,
    /// Hashes of past blocks, for the `BLOCKHASH` opcode.
    pub block_hashes: HashMap<u64, B256>,
}

impl BreejaDb {
    /// Copies the chain head out of `state`, so the caller can release the lock
    /// before executing anything.
    pub fn from_state(state: &BlockState) -> Self {
        let head = state.get_block_number();
        // BLOCKHASH can only see recent blocks (the last 256), so there is no need
        // to copy the whole chain.
        let block_hashes = (head.saturating_sub(255)..=head)
            .filter_map(|n| state.get_block(n).map(|block| (n, block.hash)))
            .collect();

        Self {
            accounts: state.accounts_snapshot(),
            code: state.code_snapshot(),
            block_hashes,
        }
    }
}

impl BreejaDb {
    /// Like [`from_state`](Self::from_state) but for the state after block `number`,
    /// or `None` if that block does not exist. Code is shared across history, so
    /// only the accounts differ.
    pub fn from_state_at(state: &BlockState, number: u64) -> Option<Self> {
        let accounts = state.accounts_at(number)?;
        let block_hashes = (number.saturating_sub(255)..=number)
            .filter_map(|n| state.get_block(n).map(|block| (n, block.hash)))
            .collect();
        Some(Self {
            accounts,
            code: state.code_snapshot(),
            block_hashes,
        })
    }
}

impl Database for BreejaDb {
    // Reading an in-memory map cannot fail. A database backed by disk would use a
    // real error type here.
    type Error = Infallible;

    /// An address that has never been touched is `None`, which REVM treats as a
    /// non-existent account. That matters: calling or sending value to a
    /// non-existent account costs more gas than to an existing one, so reporting
    /// "empty account" instead of "no account" would change gas.
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let Some(account) = self.accounts.get(&address) else {
            return Ok(None);
        };

        // Breeja leaves `code_hash` as zero for an account with no code; the EVM
        // spells the same thing `KECCAK256_EMPTY`.
        let (code_hash, code) = if is_empty_code_hash(&account.code_hash) {
            (KECCAK256_EMPTY, Bytecode::default())
        } else {
            let bytes = self
                .code
                .get(&account.code_hash)
                .cloned()
                .unwrap_or_default();
            (account.code_hash, Bytecode::new_raw(bytes))
        };

        Ok(Some(AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash,
            code: Some(code),
            ..Default::default()
        }))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self
            .code
            .get(&code_hash)
            .cloned()
            .map(Bytecode::new_raw)
            .unwrap_or_default())
    }

    /// A slot nobody has written reads as zero.
    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        Ok(self
            .accounts
            .get(&address)
            .and_then(|account| account.storage.get(&index).copied())
            .unwrap_or(U256::ZERO))
    }

    /// Backs the `BLOCKHASH` opcode. An unknown block is the zero hash, which is
    /// what the opcode returns for a block it cannot see.
    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self
            .block_hashes
            .get(&number)
            .copied()
            .unwrap_or(B256::ZERO))
    }
}

impl DatabaseCommit for BreejaDb {
    fn commit(&mut self, changes: AddressMap<EvmAccount>) {
        for (address, account) in changes {
            // REVM reports every account it *looked at*, not only the ones it
            // changed. Only touched accounts represent a change.
            if !account.is_touched() {
                continue;
            }

            // SELFDESTRUCT deletes the account entirely, storage and all.
            if account.is_selfdestructed() {
                self.accounts.remove(&address);
                continue;
            }

            let entry = self.accounts.entry(address).or_default();
            entry.balance = account.info.balance;
            entry.nonce = account.info.nonce;

            // Code is set once, at deployment. Store it by hash so identical
            // contracts share one copy, and point the account at it.
            if let Some(code) = &account.info.code
                && !code.is_empty()
            {
                let bytes = code.original_bytes();
                let hash = keccak256(&bytes);
                self.code.insert(hash, bytes);
                entry.code_hash = hash;
            }

            // A slot that holds zero is absent, as on Ethereum. Dropping it keeps
            // two states that differ only by a zeroed slot identical.
            for (slot, value) in account.storage {
                if value.present_value.is_zero() {
                    entry.storage.remove(&slot);
                } else {
                    entry.storage.insert(slot, value.present_value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use primitives::node::BreejaGenesis;
    use revm::state::{EvmStorageSlot, TransactionId};

    use super::*;

    fn one_eth() -> U256 {
        U256::from(10u64).pow(U256::from(18))
    }

    fn funded_db(address: Address, balance: U256, nonce: u64) -> BreejaDb {
        let mut db = BreejaDb::default();
        db.accounts.insert(
            address,
            Account {
                balance,
                nonce,
                ..Default::default()
            },
        );
        db
    }

    /// Builds the change record REVM would return for an account it touched.
    fn touched(balance: U256, nonce: u64) -> EvmAccount {
        let mut account = EvmAccount::from(AccountInfo {
            balance,
            nonce,
            ..Default::default()
        });
        account.mark_touch();
        account
    }

    #[test]
    fn basic_returns_the_balance_and_nonce_of_a_funded_account() {
        let sender = Address::repeat_byte(0x11);
        let mut db = funded_db(sender, one_eth(), 3);

        let info = db.basic(sender).unwrap().expect("account exists");
        assert_eq!(info.balance, one_eth());
        assert_eq!(info.nonce, 3);
        // No code: the EVM's spelling of "empty", not Breeja's all-zero default.
        assert_eq!(info.code_hash, KECCAK256_EMPTY);
    }

    #[test]
    fn basic_reports_an_unknown_address_as_no_account_not_an_empty_one() {
        let mut db = BreejaDb::default();
        assert!(db.basic(Address::repeat_byte(0x99)).unwrap().is_none());
    }

    #[test]
    fn code_and_storage_come_back_through_the_database() {
        let contract = Address::repeat_byte(0x22);
        let program = Bytes::from_static(&[0x60, 0x2a, 0x60, 0x00, 0x55, 0x00]);
        let hash = keccak256(&program);

        let mut db = BreejaDb::default();
        db.code.insert(hash, program.clone());
        let mut account = Account::new(U256::ZERO);
        account.code_hash = hash;
        account.storage.insert(U256::from(7u64), U256::from(42u64));
        db.accounts.insert(contract, account);

        // The account points at its code, and the code is fetched by hash.
        let info = db.basic(contract).unwrap().unwrap();
        assert_eq!(info.code_hash, hash);
        assert_eq!(db.code_by_hash(hash).unwrap().original_bytes(), program);

        assert_eq!(
            db.storage(contract, U256::from(7u64)).unwrap(),
            U256::from(42u64)
        );
        // A slot nobody wrote, and a contract that does not exist, both read zero.
        assert_eq!(db.storage(contract, U256::from(8u64)).unwrap(), U256::ZERO);
        assert_eq!(
            db.storage(Address::repeat_byte(0x77), U256::from(7u64))
                .unwrap(),
            U256::ZERO
        );
        // Code that was never stored is empty, not an error.
        assert!(db.code_by_hash(B256::repeat_byte(0xee)).unwrap().is_empty());
    }

    #[test]
    fn block_hash_answers_for_known_blocks_and_zero_for_unknown_ones() {
        let mut db = BreejaDb::default();
        db.block_hashes.insert(5, B256::repeat_byte(0x05));
        assert_eq!(db.block_hash(5).unwrap(), B256::repeat_byte(0x05));
        assert_eq!(db.block_hash(6).unwrap(), B256::ZERO);
    }

    #[test]
    fn commit_persists_balance_nonce_code_and_a_storage_slot() {
        let contract = Address::repeat_byte(0x33);
        let program = Bytes::from_static(&[0x60, 0x2a, 0x60, 0x00, 0x55, 0x00]);

        let mut change = touched(one_eth(), 1);
        change.info.code = Some(Bytecode::new_raw(program.clone()));
        change.info.code_hash = keccak256(&program);
        change.storage.insert(
            U256::from(7u64),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(42u64), TransactionId::ZERO),
        );

        let mut db = BreejaDb::default();
        let mut changes = AddressMap::default();
        changes.insert(contract, change);
        db.commit(changes);

        let account = &db.accounts[&contract];
        assert_eq!(account.balance, one_eth());
        assert_eq!(account.nonce, 1);
        assert_eq!(account.code_hash, keccak256(&program));
        assert_eq!(account.storage[&U256::from(7u64)], U256::from(42u64));
        // The code landed in the shared map, under its hash.
        assert_eq!(db.code[&keccak256(&program)], program);
    }

    #[test]
    fn commit_ignores_accounts_that_were_only_read() {
        let reader = Address::repeat_byte(0x44);
        // Looked at but never touched: REVM still includes it in the change set.
        let untouched = EvmAccount::from(AccountInfo {
            balance: one_eth(),
            ..Default::default()
        });

        let mut db = BreejaDb::default();
        let mut changes = AddressMap::default();
        changes.insert(reader, untouched);
        db.commit(changes);

        assert!(db.accounts.is_empty());
    }

    #[test]
    fn writing_zero_to_a_slot_removes_it() {
        let contract = Address::repeat_byte(0x55);
        let slot = U256::from(1u64);

        let mut db = funded_db(contract, U256::ZERO, 0);
        db.accounts
            .get_mut(&contract)
            .unwrap()
            .storage
            .insert(slot, U256::from(9u64));

        let mut change = touched(U256::ZERO, 0);
        change.storage.insert(
            slot,
            EvmStorageSlot::new_changed(U256::from(9u64), U256::ZERO, TransactionId::ZERO),
        );
        let mut changes = AddressMap::default();
        changes.insert(contract, change);
        db.commit(changes);

        assert!(!db.accounts[&contract].storage.contains_key(&slot));
    }

    #[test]
    fn a_selfdestructed_account_is_removed_entirely() {
        let doomed = Address::repeat_byte(0x66);
        let mut db = funded_db(doomed, one_eth(), 1);

        let mut change = touched(one_eth(), 1);
        change.mark_selfdestruct();
        let mut changes = AddressMap::default();
        changes.insert(doomed, change);
        db.commit(changes);

        assert!(!db.accounts.contains_key(&doomed));
    }

    /// Runs one transaction through a real REVM against `db` and commits the result,
    /// returning the execution result. This is the whole integration in miniature:
    /// REVM reads through `Database` while it runs, then `DatabaseCommit` applies
    /// what it changed.
    fn run(
        db: &mut BreejaDb,
        miner: Address,
        tx: revm::context::TxEnv,
    ) -> revm::context_interface::result::ExecutionResult {
        use revm::{
            Context, ExecuteEvm, MainBuilder, MainContext,
            context::{BlockEnv, CfgEnv},
            primitives::hardfork::SpecId,
        };

        let block = BlockEnv {
            number: U256::from(1u64),
            beneficiary: miner,
            timestamp: U256::from(1u64),
            gas_limit: 3_141_592,
            basefee: 0,
            difficulty: U256::from(1u64),
            prevrandao: Some(B256::repeat_byte(0x01)),
            ..Default::default()
        };
        let cfg = CfgEnv::new_with_spec(SpecId::SHANGHAI).with_chain_id(1);

        let mut evm = Context::mainnet()
            .with_db(&mut *db)
            .with_block(block)
            .with_cfg(cfg)
            .build_mainnet();
        let outcome = evm.transact(tx).expect("transaction is valid");

        let result = outcome.result;
        let state = outcome.state;
        // The EVM holds `&mut` on the database for as long as it lives, so it has
        // to be dropped before anything can be committed.
        drop(evm);
        db.commit(state);
        result
    }

    /// The end-to-end proof for this milestone: a real EVM deploys a contract and
    /// calls it, and everything it reads and writes goes through `BreejaDb`.
    ///
    /// The contract is the 11-byte program from the project brief:
    /// `PUSH1 42, PUSH1 0, SSTORE, PUSH1 0, PUSH1 0, LOG0, STOP`.
    #[test]
    fn a_real_evm_deploys_and_calls_a_contract_through_the_database() {
        use revm::{context::TxEnv, primitives::TxKind};

        let sender = Address::repeat_byte(0x11);
        let miner = Address::repeat_byte(0x99);
        let gas_price = 1_000_000_000u128;
        let initcode = Bytes::from_static(&alloy::hex!(
            "600b600c600039600b6000f3602a60005560006000a000"
        ));
        let runtime = Bytes::from_static(&alloy::hex!("602a60005560006000a000"));

        let mut db = funded_db(sender, one_eth() * U256::from(10u64), 0);

        // 1. Deploy: a Create transaction with the initcode as data.
        let deploy = run(
            &mut db,
            miner,
            TxEnv {
                caller: sender,
                gas_limit: 100_000,
                gas_price,
                kind: TxKind::Create,
                data: initcode,
                nonce: 0,
                chain_id: Some(1),
                ..Default::default()
            },
        );
        assert!(deploy.is_success(), "deploy should succeed, got {deploy:?}");
        let deploy_gas = deploy.tx_gas_used();
        let contract = deploy
            .created_address()
            .expect("a deploy returns an address");

        // The runtime code (not the initcode) was committed under its hash, and the
        // new account points at it.
        assert_eq!(db.accounts[&contract].code_hash, keccak256(&runtime));
        assert_eq!(db.code[&keccak256(&runtime)], runtime);
        assert_eq!(db.accounts[&sender].nonce, 1, "the deploy used one nonce");

        // 2. Call it: SSTORE writes slot 0, LOG0 emits an event.
        let call = run(
            &mut db,
            miner,
            TxEnv {
                caller: sender,
                gas_limit: 100_000,
                gas_price,
                kind: TxKind::Call(contract),
                nonce: 1,
                chain_id: Some(1),
                ..Default::default()
            },
        );
        assert!(call.is_success(), "call should succeed, got {call:?}");
        let call_gas = call.tx_gas_used();
        assert_eq!(call.logs().len(), 1, "LOG0 emitted one log");
        assert_eq!(
            db.accounts[&contract].storage[&U256::ZERO],
            U256::from(42u64),
            "SSTORE wrote 42 to slot 0, and it was committed"
        );
        assert_eq!(db.accounts[&sender].nonce, 2);

        // The gas numbers the project brief measured on this REVM version.
        assert_eq!(deploy_gas, 55_522, "deploy gas");
        assert_eq!(call_gas, 43_487, "call gas");

        // REVM itself paid the miner the fees: nothing here credited them by hand.
        // This is Pitfall 1 in the project brief.
        let fees = U256::from(deploy_gas + call_gas) * U256::from(gas_price);
        assert_eq!(db.accounts[&miner].balance, fees);

        // And no ETH was created or destroyed.
        let supply: U256 = db.accounts.values().map(|a| a.balance).sum();
        assert_eq!(supply, one_eth() * U256::from(10u64));
    }

    #[test]
    fn a_snapshot_copies_accounts_code_and_recent_block_hashes_out_of_state() {
        let funded = Address::repeat_byte(0x11);
        let genesis = BreejaGenesis {
            accounts: vec![(funded, one_eth())],
        };
        let mut state = BlockState::new_with_gensis(genesis, 8);
        let hash = state.insert_code(Bytes::from_static(&[0x00]));

        let mut db = BreejaDb::from_state(&state);
        assert_eq!(db.basic(funded).unwrap().unwrap().balance, one_eth());
        assert!(!db.code_by_hash(hash).unwrap().is_empty());
        assert_eq!(db.block_hash(0).unwrap(), state.head().hash);
    }
}
