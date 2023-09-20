use crate::{
    database::StateProviderDatabase,
    env::{fill_cfg_and_block_env, fill_tx_env},
    eth_dao_fork::{DAO_HARDFORK_BENEFICIARY, DAO_HARDKFORK_ACCOUNTS},
    into_reth_log,
    stack::{InspectorStack, InspectorStackConfig},
    state_change::{apply_beacon_root_contract_call, post_block_balance_increments},
};
use reth_interfaces::{
    executor::{BlockExecutionError, BlockValidationError},
    RethError,
};
use reth_primitives::{
    Address, Block, BlockNumber, Bloom, ChainSpec, Hardfork, Header, PruneMode, PruneModes,
    PrunePartError, Receipt, ReceiptWithBloom, TransactionSigned, H256, MINIMUM_PRUNING_DISTANCE,
    U256,
};
use reth_provider::{
    BlockExecutor, BlockExecutorStats, BundleStateWithReceipts, PrunableBlockExecutor,
    StateProvider,
};
use revm::{
    db::{states::bundle_state::BundleRetention, StateDBBox},
    primitives::ResultAndState,
    DatabaseCommit, State, EVM,
};
use std::{sync::Arc, time::Instant};
use tracing::{debug, trace};

/// EVMProcessor is a block executor that uses revm to execute blocks or multiple blocks.
///
/// Output is obtained by calling `take_output_state` function.
///
/// It is capable of pruning the data that will be written to the database
/// and implemented [PrunableBlockExecutor] traits.
///
/// It implemented the [BlockExecutor] that give it the ability to take block
/// apply pre state (Cancun system contract call), execute transaction and apply
/// state change and then apply post execution changes (block reward, withdrawals, irregular DAO
/// hardfork state change). And if `execute_and_verify_receipt` is called it will verify the
/// receipt.
///
/// InspectorStack are used for optional inspecting execution. And it contains
/// various duration of parts of execution.
pub struct EVMProcessor<'a> {
    /// The configured chain-spec
    chain_spec: Arc<ChainSpec>,
    /// revm instance that contains database and env environment.
    evm: EVM<StateDBBox<'a, RethError>>,
    /// Hook and inspector stack that we want to invoke on that hook.
    stack: InspectorStack,
    /// The collection of receipts.
    /// Outer vector stores receipts for each block sequentially.
    /// The inner vector stores receipts ordered by transaction number.
    ///
    /// If receipt is None it means it is pruned.
    receipts: Vec<Vec<Option<Receipt>>>,
    /// First block will be initialized to `None`
    /// and be set to the block number of first block executed.
    first_block: Option<BlockNumber>,
    /// The maximum known block.
    tip: Option<BlockNumber>,
    /// Pruning configuration.
    prune_modes: PruneModes,
    /// Memoized address pruning filter.
    /// Empty implies that there is going to be addresses to include in the filter in a future
    /// block. None means there isn't any kind of configuration.
    pruning_address_filter: Option<(u64, Vec<Address>)>,
    /// Execution stats
    stats: BlockExecutorStats,
}

impl<'a> EVMProcessor<'a> {
    /// Return chain spec.
    pub fn chain_spec(&self) -> &Arc<ChainSpec> {
        &self.chain_spec
    }

    /// Create a new pocessor with the given chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        let evm = EVM::new();
        EVMProcessor {
            chain_spec,
            evm,
            stack: InspectorStack::new(InspectorStackConfig::default()),
            receipts: Vec::new(),
            first_block: None,
            tip: None,
            prune_modes: PruneModes::none(),
            pruning_address_filter: None,
            stats: BlockExecutorStats::default(),
        }
    }

    /// Creates a new executor from the given chain spec and database.
    pub fn new_with_db<DB: StateProvider + 'a>(
        chain_spec: Arc<ChainSpec>,
        db: StateProviderDatabase<DB>,
    ) -> Self {
        let state = State::builder()
            .with_database_boxed(Box::new(db))
            .with_bundle_update()
            .without_state_clear()
            .build();
        EVMProcessor::new_with_state(chain_spec, state)
    }

    /// Create a new EVM processor with the given revm state.
    pub fn new_with_state(
        chain_spec: Arc<ChainSpec>,
        revm_state: StateDBBox<'a, RethError>,
    ) -> Self {
        let mut evm = EVM::new();
        evm.database(revm_state);
        EVMProcessor {
            chain_spec,
            evm,
            stack: InspectorStack::new(InspectorStackConfig::default()),
            receipts: Vec::new(),
            first_block: None,
            tip: None,
            prune_modes: PruneModes::none(),
            pruning_address_filter: None,
            stats: BlockExecutorStats::default(),
        }
    }

    /// Configures the executor with the given inspectors.
    pub fn set_stack(&mut self, stack: InspectorStack) {
        self.stack = stack;
    }

    /// Returns a reference to the database
    pub fn db_mut(&mut self) -> &mut StateDBBox<'a, RethError> {
        // Option will be removed from EVM in the future.
        // as it is always some.
        // https://github.com/bluealloy/revm/issues/697
        self.evm.db().expect("Database inside EVM is always set")
    }

    fn recover_senders(
        &mut self,
        body: &[TransactionSigned],
        senders: Option<Vec<Address>>,
    ) -> Result<Vec<Address>, BlockExecutionError> {
        if let Some(senders) = senders {
            if body.len() == senders.len() {
                Ok(senders)
            } else {
                Err(BlockValidationError::SenderRecoveryError.into())
            }
        } else {
            let time = Instant::now();
            let ret = TransactionSigned::recover_signers(body, body.len())
                .ok_or(BlockValidationError::SenderRecoveryError.into());
            self.stats.sender_recovery_duration += time.elapsed();
            ret
        }
    }

    /// Initializes the config and block env.
    fn init_env(&mut self, header: &Header, total_difficulty: U256) {
        // Set state clear flag.
        let state_clear_flag =
            self.chain_spec.fork(Hardfork::SpuriousDragon).active_at_block(header.number);

        self.db_mut().set_state_clear_flag(state_clear_flag);

        fill_cfg_and_block_env(
            &mut self.evm.env.cfg,
            &mut self.evm.env.block,
            &self.chain_spec,
            header,
            total_difficulty,
        );
    }

    /// Applies the pre-block call to the EIP-4788 beacon block root contract.
    ///
    /// If cancun is not activated or the block is the genesis block, then this is a no-op, and no
    /// state changes are made.
    pub fn apply_beacon_root_contract_call(
        &mut self,
        block: &Block,
    ) -> Result<(), BlockExecutionError> {
        apply_beacon_root_contract_call(
            &self.chain_spec,
            block.timestamp,
            block.number,
            block.parent_beacon_block_root,
            &mut self.evm,
        )?;
        Ok(())
    }

    /// Apply post execution state changes, including block rewards, withdrawals, and irregular DAO
    /// hardfork state change.
    pub fn apply_post_execution_state_change(
        &mut self,
        block: &Block,
        total_difficulty: U256,
    ) -> Result<(), BlockExecutionError> {
        let mut balance_increments = post_block_balance_increments(
            &self.chain_spec,
            block.number,
            block.difficulty,
            block.beneficiary,
            block.timestamp,
            total_difficulty,
            &block.ommers,
            block.withdrawals.as_deref(),
        );

        // Irregular state change at Ethereum DAO hardfork
        if self.chain_spec.fork(Hardfork::Dao).transitions_at_block(block.number) {
            // drain balances from hardcoded addresses.
            let drained_balance: u128 = self
                .db_mut()
                .drain_balances(DAO_HARDKFORK_ACCOUNTS)
                .map_err(|_| BlockValidationError::IncrementBalanceFailed)?
                .into_iter()
                .sum();

            // return balance to DAO beneficiary.
            *balance_increments.entry(DAO_HARDFORK_BENEFICIARY).or_default() += drained_balance;
        }
        // increment balances
        self.db_mut()
            .increment_balances(balance_increments.into_iter().map(|(k, v)| (k, v)))
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        Ok(())
    }

    /// Runs a single transaction in the configured environment and proceeds
    /// to return the result and state diff (without applying it).
    ///
    /// Assumes the rest of the block environment has been filled via `init_block_env`.
    pub fn transact(
        &mut self,
        transaction: &TransactionSigned,
        sender: Address,
    ) -> Result<ResultAndState, BlockExecutionError> {
        // Fill revm structure.
        fill_tx_env(&mut self.evm.env.tx, transaction, sender);

        let hash = transaction.hash();
        let out = if self.stack.should_inspect(&self.evm.env, hash) {
            // execution with inspector.
            let output = self.evm.inspect(&mut self.stack);
            tracing::trace!(
                target: "evm",
                ?hash, ?output, ?transaction, env = ?self.evm.env,
                "Executed transaction"
            );
            output
        } else {
            // main execution.
            self.evm.transact()
        };
        out.map_err(|e| BlockValidationError::EVM { hash, message: format!("{e:?}") }.into())
    }

    /// Runs the provided transactions and commits their state to the run-time database.
    ///
    /// The returned [BundleStateWithReceipts] can be used to persist the changes to disk, and
    /// contains the changes made by each transaction.
    ///
    /// The changes in [BundleStateWithReceipts] have a transition ID associated with them: there is
    /// one transition ID for each transaction (with the first executed tx having transition ID
    /// 0, and so on).
    ///
    /// The second returned value represents the total gas used by this block of transactions.
    pub fn execute_transactions(
        &mut self,
        block: &Block,
        total_difficulty: U256,
        senders: Option<Vec<Address>>,
    ) -> Result<(Vec<Receipt>, u64), BlockExecutionError> {
        self.init_env(&block.header, total_difficulty);

        // perf: do not execute empty blocks
        if block.body.is_empty() {
            return Ok((Vec::new(), 0))
        }

        let senders = self.recover_senders(&block.body, senders)?;

        let mut cumulative_gas_used = 0;
        let mut receipts = Vec::with_capacity(block.body.len());
        for (transaction, sender) in block.body.iter().zip(senders) {
            let time = Instant::now();
            // The sum of the transaction’s gas limit, Tg, and the gas utilized in this block prior,
            // must be no greater than the block’s gasLimit.
            let block_available_gas = block.header.gas_limit - cumulative_gas_used;
            if transaction.gas_limit() > block_available_gas {
                return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: transaction.gas_limit(),
                    block_available_gas,
                }
                .into())
            }
            // Execute transaction.
            let ResultAndState { result, state } = self.transact(transaction, sender)?;
            trace!(
                target: "evm",
                ?transaction, ?result, ?state,
                "Executed transaction"
            );
            self.stats.execution_duration += time.elapsed();
            let time = Instant::now();

            self.db_mut().commit(state);

            self.stats.apply_state_duration += time.elapsed();

            // append gas used
            cumulative_gas_used += result.gas_used();

            // Push transaction changeset and calculate header bloom filter for receipt.
            receipts.push(Receipt {
                tx_type: transaction.tx_type(),
                // Success flag was added in `EIP-658: Embedding transaction status code in
                // receipts`.
                success: result.is_success(),
                cumulative_gas_used,
                // convert to reth log
                logs: result.into_logs().into_iter().map(into_reth_log).collect(),
            });
        }

        Ok((receipts, cumulative_gas_used))
    }

    /// Execute the block, verify gas usage and apply post-block state changes.
    fn execute_inner(
        &mut self,
        block: &Block,
        total_difficulty: U256,
        senders: Option<Vec<Address>>,
    ) -> Result<Vec<Receipt>, BlockExecutionError> {
        self.init_env(&block.header, total_difficulty);
        self.apply_beacon_root_contract_call(block)?;
        let (receipts, cumulative_gas_used) =
            self.execute_transactions(block, total_difficulty, senders)?;

        // Check if gas used matches the value set in header.
        if block.gas_used != cumulative_gas_used {
            return Err(BlockValidationError::BlockGasUsed {
                got: cumulative_gas_used,
                expected: block.gas_used,
                gas_spent_by_tx: self
                    .receipts
                    .last()
                    .map(|block_r| {
                        block_r
                            .iter()
                            .enumerate()
                            .map(|(id, tx_r)| {
                                (
                                    id as u64,
                                    tx_r.as_ref()
                                        .expect("receipts have not been pruned")
                                        .cumulative_gas_used,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }
            .into())
        }
        let time = Instant::now();
        self.apply_post_execution_state_change(block, total_difficulty)?;
        self.stats.apply_post_execution_state_changes_duration += time.elapsed();

        let time = Instant::now();
        let retention = if self.tip.map_or(true, |tip| {
            !self.prune_modes.should_prune_account_history(block.number, tip) &&
                !self.prune_modes.should_prune_storage_history(block.number, tip)
        }) {
            BundleRetention::Reverts
        } else {
            BundleRetention::PlainState
        };
        self.db_mut().merge_transitions(retention);
        self.stats.merge_transitions_duration += time.elapsed();

        if self.first_block.is_none() {
            self.first_block = Some(block.number);
        }

        Ok(receipts)
    }

    /// Save receipts to the executor.
    pub fn save_receipts(&mut self, receipts: Vec<Receipt>) -> Result<(), BlockExecutionError> {
        let mut receipts = receipts.into_iter().map(Option::Some).collect();
        // Prune receipts if necessary.
        self.prune_receipts(&mut receipts)?;
        // Save receipts.
        self.receipts.push(receipts);
        Ok(())
    }

    /// Prune receipts according to the pruning configuration.
    fn prune_receipts(
        &mut self,
        receipts: &mut Vec<Option<Receipt>>,
    ) -> Result<(), PrunePartError> {
        let (first_block, tip) = match self.first_block.zip(self.tip) {
            Some((block, tip)) => (block, tip),
            _ => return Ok(()),
        };

        let block_number = first_block + self.receipts.len() as u64;

        // Block receipts should not be retained
        if self.prune_modes.receipts == Some(PruneMode::Full) ||
                // [`PrunePart::Receipts`] takes priority over [`PrunePart::ContractLogs`]
                self.prune_modes.should_prune_receipts(block_number, tip)
        {
            receipts.clear();
            return Ok(())
        }

        // All receipts from the last 128 blocks are required for blockchain tree, even with
        // [`PrunePart::ContractLogs`].
        let prunable_receipts =
            PruneMode::Distance(MINIMUM_PRUNING_DISTANCE).should_prune(block_number, tip);
        if !prunable_receipts {
            return Ok(())
        }

        let contract_log_pruner = self.prune_modes.receipts_log_filter.group_by_block(tip, None)?;

        if !contract_log_pruner.is_empty() {
            let (prev_block, filter) = self.pruning_address_filter.get_or_insert((0, Vec::new()));
            for (_, addresses) in contract_log_pruner.range(*prev_block..=block_number) {
                filter.extend(addresses.iter().copied());
            }
        }

        for receipt in receipts.iter_mut() {
            let inner_receipt = receipt.as_ref().expect("receipts have not been pruned");

            // If there is an address_filter, and it does not contain any of the
            // contract addresses, then remove this receipts
            if let Some((_, filter)) = &self.pruning_address_filter {
                if !inner_receipt.logs.iter().any(|log| filter.contains(&log.address)) {
                    receipt.take();
                }
            }
        }

        Ok(())
    }
}

impl<'a> BlockExecutor for EVMProcessor<'a> {
    fn execute(
        &mut self,
        block: &Block,
        total_difficulty: U256,
        senders: Option<Vec<Address>>,
    ) -> Result<(), BlockExecutionError> {
        let receipts = self.execute_inner(block, total_difficulty, senders)?;
        self.save_receipts(receipts)
    }

    fn execute_and_verify_receipt(
        &mut self,
        block: &Block,
        total_difficulty: U256,
        senders: Option<Vec<Address>>,
    ) -> Result<(), BlockExecutionError> {
        // execute block
        let receipts = self.execute_inner(block, total_difficulty, senders)?;

        // TODO Before Byzantium, receipts contained state root that would mean that expensive
        // operation as hashing that is needed for state root got calculated in every
        // transaction This was replaced with is_success flag.
        // See more about EIP here: https://eips.ethereum.org/EIPS/eip-658
        if self.chain_spec.fork(Hardfork::Byzantium).active_at_block(block.header.number) {
            let time = Instant::now();
            if let Err(error) =
                verify_receipt(block.header.receipts_root, block.header.logs_bloom, receipts.iter())
            {
                debug!(target: "evm", ?error, ?receipts, "receipts verification failed");
                return Err(error)
            };
            self.stats.receipt_root_duration += time.elapsed();
        }

        self.save_receipts(receipts)
    }

    fn take_output_state(&mut self) -> BundleStateWithReceipts {
        let receipts = std::mem::take(&mut self.receipts);
        BundleStateWithReceipts::new(
            self.evm.db().unwrap().take_bundle(),
            receipts,
            self.first_block.unwrap_or_default(),
        )
    }

    fn stats(&self) -> BlockExecutorStats {
        self.stats.clone()
    }

    fn size_hint(&self) -> Option<usize> {
        self.evm.db.as_ref().map(|db| db.bundle_size_hint())
    }
}

impl<'a> PrunableBlockExecutor for EVMProcessor<'a> {
    fn set_tip(&mut self, tip: BlockNumber) {
        self.tip = Some(tip);
    }

    fn set_prune_modes(&mut self, prune_modes: PruneModes) {
        self.prune_modes = prune_modes;
    }
}

/// Verify receipts
pub fn verify_receipt<'a>(
    expected_receipts_root: H256,
    expected_logs_bloom: Bloom,
    receipts: impl Iterator<Item = &'a Receipt> + Clone,
) -> Result<(), BlockExecutionError> {
    // Check receipts root.
    let receipts_with_bloom = receipts.map(|r| r.clone().into()).collect::<Vec<ReceiptWithBloom>>();
    let receipts_root = reth_primitives::proofs::calculate_receipt_root(&receipts_with_bloom);
    if receipts_root != expected_receipts_root {
        return Err(BlockValidationError::ReceiptRootDiff {
            got: receipts_root,
            expected: expected_receipts_root,
        }
        .into())
    }

    // Create header log bloom.
    let logs_bloom = receipts_with_bloom.iter().fold(Bloom::zero(), |bloom, r| bloom | r.bloom);
    if logs_bloom != expected_logs_bloom {
        return Err(BlockValidationError::BloomLogDiff {
            expected: Box::new(expected_logs_bloom),
            got: Box::new(logs_bloom),
        }
        .into())
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use reth_interfaces::RethResult;
    use reth_primitives::{
        constants::{BEACON_ROOTS_ADDRESS, SYSTEM_ADDRESS},
        keccak256, Account, Bytecode, Bytes, ChainSpecBuilder, ForkCondition, StorageKey, MAINNET,
    };
    use reth_provider::{AccountReader, BlockHashReader, StateRootProvider};
    use reth_revm_primitives::TransitionState;
    use revm::Database;
    use std::{collections::HashMap, str::FromStr};

    use super::*;

    /// Returns the beacon root contract code
    fn beacon_root_contract_code() -> Bytes {
        Bytes::from_str("0x3373fffffffffffffffffffffffffffffffffffffffe14604457602036146024575f5ffd5b620180005f350680545f35146037575f5ffd5b6201800001545f5260205ff35b6201800042064281555f359062018000015500").unwrap()
    }

    #[derive(Debug, Default, Clone, Eq, PartialEq)]
    struct StateProviderTest {
        accounts: HashMap<Address, (HashMap<StorageKey, U256>, Account)>,
        contracts: HashMap<H256, Bytecode>,
        block_hash: HashMap<u64, H256>,
    }

    impl StateProviderTest {
        /// Insert account.
        fn insert_account(
            &mut self,
            address: Address,
            mut account: Account,
            bytecode: Option<Bytes>,
            storage: HashMap<StorageKey, U256>,
        ) {
            if let Some(bytecode) = bytecode {
                let hash = keccak256(&bytecode);
                account.bytecode_hash = Some(hash);
                self.contracts.insert(hash, Bytecode::new_raw(bytecode.into()));
            }
            self.accounts.insert(address, (storage, account));
        }
    }

    impl AccountReader for StateProviderTest {
        fn basic_account(&self, address: Address) -> RethResult<Option<Account>> {
            let ret = Ok(self.accounts.get(&address).map(|(_, acc)| *acc));
            ret
        }
    }

    impl BlockHashReader for StateProviderTest {
        fn block_hash(&self, number: u64) -> RethResult<Option<H256>> {
            Ok(self.block_hash.get(&number).cloned())
        }

        fn canonical_hashes_range(
            &self,
            start: BlockNumber,
            end: BlockNumber,
        ) -> RethResult<Vec<H256>> {
            let range = start..end;
            Ok(self
                .block_hash
                .iter()
                .filter_map(|(block, hash)| range.contains(block).then_some(*hash))
                .collect())
        }
    }

    impl StateRootProvider for StateProviderTest {
        fn state_root(&self, _bundle_state: BundleStateWithReceipts) -> RethResult<H256> {
            todo!()
        }
    }

    impl StateProvider for StateProviderTest {
        fn storage(
            &self,
            account: Address,
            storage_key: StorageKey,
        ) -> RethResult<Option<reth_primitives::StorageValue>> {
            Ok(self
                .accounts
                .get(&account)
                .and_then(|(storage, _)| storage.get(&storage_key).cloned()))
        }

        fn bytecode_by_hash(&self, code_hash: H256) -> RethResult<Option<Bytecode>> {
            Ok(self.contracts.get(&code_hash).cloned())
        }

        fn proof(
            &self,
            _address: Address,
            _keys: &[H256],
        ) -> RethResult<(Vec<Bytes>, H256, Vec<Vec<Bytes>>)> {
            todo!()
        }
    }

    #[test]
    fn eip_4788_non_genesis_call() {
        let mut header =
            Header { timestamp: 1, number: 1, excess_blob_gas: Some(0), ..Header::default() };

        let mut db = StateProviderTest::default();

        let beacon_root_contract_code = beacon_root_contract_code();

        let beacon_root_contract_account = Account {
            balance: U256::ZERO,
            bytecode_hash: Some(keccak256(beacon_root_contract_code.clone())),
            nonce: 1,
        };

        db.insert_account(
            BEACON_ROOTS_ADDRESS,
            beacon_root_contract_account,
            Some(beacon_root_contract_code),
            HashMap::new(),
        );

        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(1))
                .build(),
        );

        // execute invalid header (no parent beacon block root)
        let mut executor = EVMProcessor::new_with_db(chain_spec, StateProviderDatabase::new(db));

        // attempt to execute a block without parent beacon block root, expect err
        let err = executor
            .execute_and_verify_receipt(
                &Block { header: header.clone(), body: vec![], ommers: vec![], withdrawals: None },
                U256::ZERO,
                None,
            )
            .expect_err(
                "Executing cancun block without parent beacon block root field should fail",
            );
        assert_eq!(
            err,
            BlockExecutionError::Validation(BlockValidationError::MissingParentBeaconBlockRoot)
        );

        // fix header, set a gas limit
        header.parent_beacon_block_root = Some(H256::from_low_u64_be(0x1337));

        // Now execute a block with the fixed header, ensure that it does not fail
        executor
            .execute(
                &Block { header: header.clone(), body: vec![], ommers: vec![], withdrawals: None },
                U256::ZERO,
                None,
            )
            .unwrap();

        // check the actual storage of the contract - it should be:
        // * The storage value at header.timestamp % HISTORY_BUFFER_LENGTH should be
        // header.timestamp
        // * The storage value at header.timestamp % HISTORY_BUFFER_LENGTH + HISTORY_BUFFER_LENGTH
        // should be parent_beacon_block_root
        let history_buffer_length = 98304u64;
        let timestamp_index = header.timestamp % history_buffer_length;
        let parent_beacon_block_root_index =
            timestamp_index % history_buffer_length + history_buffer_length;

        // get timestamp storage and compare
        let timestamp_storage =
            executor.db_mut().storage(BEACON_ROOTS_ADDRESS, U256::from(timestamp_index)).unwrap();
        assert_eq!(timestamp_storage, U256::from(header.timestamp));

        // get parent beacon block root storage and compare
        let parent_beacon_block_root_storage = executor
            .db_mut()
            .storage(BEACON_ROOTS_ADDRESS, U256::from(parent_beacon_block_root_index))
            .expect("storage value should exist");
        assert_eq!(parent_beacon_block_root_storage, U256::from(0x1337));
    }

    #[test]
    fn eip_4788_no_code_cancun() {
        // This test ensures that we "silently fail" when cancun is active and there is no code at
        // BEACON_ROOTS_ADDRESS
        let header = Header {
            timestamp: 1,
            number: 1,
            parent_beacon_block_root: Some(H256::from_low_u64_be(0x1337)),
            excess_blob_gas: Some(0),
            ..Header::default()
        };

        let db = StateProviderTest::default();

        // DON'T deploy the contract at genesis
        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(1))
                .build(),
        );

        let mut executor = EVMProcessor::new_with_db(chain_spec, StateProviderDatabase::new(db));
        executor.init_env(&header, U256::ZERO);

        // get the env
        let previous_env = executor.evm.env.clone();

        // attempt to execute an empty block with parent beacon block root, this should not fail
        executor
            .execute_and_verify_receipt(
                &Block { header: header.clone(), body: vec![], ommers: vec![], withdrawals: None },
                U256::ZERO,
                None,
            )
            .expect(
                "Executing a block with no transactions while cancun is active should not fail",
            );

        // ensure that the env has not changed
        assert_eq!(executor.evm.env, previous_env);
    }

    #[test]
    fn eip_4788_empty_account_call() {
        // This test ensures that we do not increment the nonce of an empty SYSTEM_ADDRESS account
        // during the pre-block call
        let mut db = StateProviderTest::default();

        let beacon_root_contract_code = beacon_root_contract_code();

        let beacon_root_contract_account = Account {
            balance: U256::ZERO,
            bytecode_hash: Some(keccak256(beacon_root_contract_code.clone())),
            nonce: 1,
        };

        db.insert_account(
            BEACON_ROOTS_ADDRESS,
            beacon_root_contract_account,
            Some(beacon_root_contract_code),
            HashMap::new(),
        );

        // insert an empty SYSTEM_ADDRESS
        db.insert_account(SYSTEM_ADDRESS, Account::default(), None, HashMap::new());

        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(1))
                .build(),
        );

        let mut executor = EVMProcessor::new_with_db(chain_spec, StateProviderDatabase::new(db));

        // construct the header for block one
        let header = Header {
            timestamp: 1,
            number: 1,
            parent_beacon_block_root: Some(H256::from_low_u64_be(0x1337)),
            excess_blob_gas: Some(0),
            ..Header::default()
        };

        executor.init_env(&header, U256::ZERO);

        // attempt to execute an empty block with parent beacon block root, this should not fail
        executor
            .execute_and_verify_receipt(
                &Block { header: header.clone(), body: vec![], ommers: vec![], withdrawals: None },
                U256::ZERO,
                None,
            )
            .expect(
                "Executing a block with no transactions while cancun is active should not fail",
            );

        // ensure that the nonce of the system address account has not changed
        let nonce = executor.db_mut().basic(SYSTEM_ADDRESS).unwrap().unwrap().nonce;
        assert_eq!(nonce, 0);
    }

    #[test]
    fn eip_4788_genesis_call() {
        let mut db = StateProviderTest::default();

        let beacon_root_contract_code = beacon_root_contract_code();

        let beacon_root_contract_account = Account {
            balance: U256::ZERO,
            bytecode_hash: Some(keccak256(beacon_root_contract_code.clone())),
            nonce: 1,
        };

        db.insert_account(
            BEACON_ROOTS_ADDRESS,
            beacon_root_contract_account,
            Some(beacon_root_contract_code),
            HashMap::new(),
        );

        // activate cancun at genesis
        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(0))
                .build(),
        );

        let mut header = chain_spec.genesis_header();

        let mut executor = EVMProcessor::new_with_db(chain_spec, StateProviderDatabase::new(db));
        executor.init_env(&header, U256::ZERO);

        // attempt to execute the genesis block with non-zero parent beacon block root, expect err
        header.parent_beacon_block_root = Some(H256::from_low_u64_be(0x1337));
        let _err = executor
            .execute_and_verify_receipt(
                &Block { header: header.clone(), body: vec![], ommers: vec![], withdrawals: None },
                U256::ZERO,
                None,
            )
            .expect_err(
                "Executing genesis cancun block with non-zero parent beacon block root field should fail",
            );

        // fix header
        header.parent_beacon_block_root = Some(H256::zero());

        // now try to process the genesis block again, this time ensuring that a system contract
        // call does not occur
        executor
            .execute(
                &Block { header: header.clone(), body: vec![], ommers: vec![], withdrawals: None },
                U256::ZERO,
                None,
            )
            .unwrap();

        // there is no system contract call so there should be NO STORAGE CHANGES
        // this means we'll check the transition state
        let state = executor.evm.db().unwrap();
        let transition_state = state
            .transition_state
            .clone()
            .expect("the evm should be initialized with bundle updates");

        // assert that it is the default (empty) transition state
        assert_eq!(transition_state, TransitionState::default());
    }

    #[test]
    fn eip_4788_high_base_fee() {
        // This test ensures that if we have a base fee, then we don't return an error when the
        // system contract is called, due to the gas price being less than the base fee.
        let header = Header {
            timestamp: 1,
            number: 1,
            parent_beacon_block_root: Some(H256::from_low_u64_be(0x1337)),
            base_fee_per_gas: Some(u64::MAX),
            excess_blob_gas: Some(0),
            ..Header::default()
        };

        let mut db = StateProviderTest::default();

        let beacon_root_contract_code = beacon_root_contract_code();

        let beacon_root_contract_account = Account {
            balance: U256::ZERO,
            bytecode_hash: Some(keccak256(beacon_root_contract_code.clone())),
            nonce: 1,
        };

        db.insert_account(
            BEACON_ROOTS_ADDRESS,
            beacon_root_contract_account,
            Some(beacon_root_contract_code),
            HashMap::new(),
        );

        let chain_spec = Arc::new(
            ChainSpecBuilder::from(&*MAINNET)
                .shanghai_activated()
                .with_fork(Hardfork::Cancun, ForkCondition::Timestamp(1))
                .build(),
        );

        // execute header
        let mut executor = EVMProcessor::new_with_db(chain_spec, StateProviderDatabase::new(db));
        executor.init_env(&header, U256::ZERO);

        // ensure that the env is configured with a base fee
        assert_eq!(executor.evm.env.block.basefee, U256::from(u64::MAX));

        // Now execute a block with the fixed header, ensure that it does not fail
        executor
            .execute(
                &Block { header: header.clone(), body: vec![], ommers: vec![], withdrawals: None },
                U256::ZERO,
                None,
            )
            .unwrap();

        // check the actual storage of the contract - it should be:
        // * The storage value at header.timestamp % HISTORY_BUFFER_LENGTH should be
        // header.timestamp
        // * The storage value at header.timestamp % HISTORY_BUFFER_LENGTH + HISTORY_BUFFER_LENGTH
        // should be parent_beacon_block_root
        let history_buffer_length = 98304u64;
        let timestamp_index = header.timestamp % history_buffer_length;
        let parent_beacon_block_root_index =
            timestamp_index % history_buffer_length + history_buffer_length;

        // get timestamp storage and compare
        let timestamp_storage =
            executor.db_mut().storage(BEACON_ROOTS_ADDRESS, U256::from(timestamp_index)).unwrap();
        assert_eq!(timestamp_storage, U256::from(header.timestamp));

        // get parent beacon block root storage and compare
        let parent_beacon_block_root_storage = executor
            .db_mut()
            .storage(BEACON_ROOTS_ADDRESS, U256::from(parent_beacon_block_root_index))
            .unwrap();
        assert_eq!(parent_beacon_block_root_storage, U256::from(0x1337));
    }
}
