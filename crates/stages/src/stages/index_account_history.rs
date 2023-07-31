use crate::{ExecInput, ExecOutput, Stage, StageError, UnwindInput, UnwindOutput};
use reth_db::database::Database;
use reth_primitives::{
    stage::{StageCheckpoint, StageId},
    PruneModes,
};
use reth_provider::{AccountExtReader, DatabaseProviderRW, HistoryWriter};
use std::fmt::Debug;

/// Stage is indexing history the account changesets generated in
/// [`ExecutionStage`][crate::stages::ExecutionStage]. For more information
/// on index sharding take a look at [`reth_db::tables::AccountHistory`]
#[derive(Debug)]
pub struct IndexAccountHistoryStage {
    /// Number of blocks after which the control
    /// flow will be returned to the pipeline for commit.
    pub commit_threshold: u64,
    /// Pruning configuration.
    pub prune_modes: PruneModes,
}

impl IndexAccountHistoryStage {
    /// Create new instance of [IndexAccountHistoryStage].
    pub fn new(commit_threshold: u64) -> Self {
        Self { commit_threshold, prune_modes: PruneModes::default() }
    }
}

impl Default for IndexAccountHistoryStage {
    fn default() -> Self {
        Self { commit_threshold: 100_000, prune_modes: PruneModes::default() }
    }
}

#[async_trait::async_trait]
impl<DB: Database> Stage<DB> for IndexAccountHistoryStage {
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::IndexAccountHistory
    }

    /// Execute the stage.
    async fn execute(
        &mut self,
        provider: &DatabaseProviderRW<'_, &DB>,
        mut input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        if let Some((target_prunable_block, _)) =
            self.prune_modes.prune_target_block_account_history(input.target())?
        {
            if target_prunable_block > input.checkpoint().block_number {
                input.checkpoint = Some(StageCheckpoint::new(target_prunable_block));
            }
        }

        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()))
        }

        let (range, is_final_range) = input.next_block_range_with_threshold(self.commit_threshold);

        let indices = provider.changed_accounts_and_blocks_with_range(range.clone())?;
        // Insert changeset to history index
        provider.insert_account_history_index(indices)?;

        Ok(ExecOutput { checkpoint: StageCheckpoint::new(*range.end()), done: is_final_range })
    }

    /// Unwind the stage.
    async fn unwind(
        &mut self,
        provider: &DatabaseProviderRW<'_, &DB>,
        input: UnwindInput,
    ) -> Result<UnwindOutput, StageError> {
        let (range, unwind_progress, _) =
            input.unwind_block_range_with_threshold(self.commit_threshold);

        provider.unwind_account_history_indices(range)?;

        // from HistoryIndex higher than that number.
        Ok(UnwindOutput { checkpoint: StageCheckpoint::new(unwind_progress) })
    }
}

#[cfg(test)]
mod tests {
    use reth_provider::ProviderFactory;
    use std::collections::BTreeMap;

    use super::*;
    use crate::test_utils::{
        stage_test_suite_ext, ExecuteStageTestRunner, StageTestRunner, TestRunnerError,
        TestTransaction, UnwindStageTestRunner,
    };
    use itertools::Itertools;
    use reth_db::{
        cursor::DbCursorRO,
        models::{
            sharded_key, sharded_key::NUM_OF_INDICES_IN_SHARD, AccountBeforeTx, ShardedKey,
            StoredBlockBodyIndices,
        },
        tables,
        transaction::{DbTx, DbTxMut},
        BlockNumberList,
    };
    use reth_interfaces::test_utils::{
        generators,
        generators::{random_block_range, random_contract_account_range, random_transition_range},
    };
    use reth_primitives::{hex_literal::hex, Address, BlockNumber, H160, H256, MAINNET};

    const ADDRESS: H160 = H160(hex!("0000000000000000000000000000000000000001"));

    fn acc() -> AccountBeforeTx {
        AccountBeforeTx { address: ADDRESS, info: None }
    }

    /// Shard for account
    fn shard(shard_index: u64) -> ShardedKey<H160> {
        ShardedKey { key: ADDRESS, highest_block_number: shard_index }
    }

    fn list(list: &[usize]) -> BlockNumberList {
        BlockNumberList::new(list).unwrap()
    }

    fn cast(
        table: Vec<(ShardedKey<H160>, BlockNumberList)>,
    ) -> BTreeMap<ShardedKey<H160>, Vec<usize>> {
        table
            .into_iter()
            .map(|(k, v)| {
                let v = v.iter(0).collect();
                (k, v)
            })
            .collect()
    }

    fn partial_setup(tx: &TestTransaction) {
        // setup
        tx.commit(|tx| {
            // we just need first and last
            tx.put::<tables::BlockBodyIndices>(
                0,
                StoredBlockBodyIndices { tx_count: 3, ..Default::default() },
            )
            .unwrap();

            tx.put::<tables::BlockBodyIndices>(
                5,
                StoredBlockBodyIndices { tx_count: 5, ..Default::default() },
            )
            .unwrap();

            // setup changeset that are going to be applied to history index
            tx.put::<tables::AccountChangeSet>(4, acc()).unwrap();
            tx.put::<tables::AccountChangeSet>(5, acc()).unwrap();
            Ok(())
        })
        .unwrap()
    }

    async fn run(tx: &TestTransaction, run_to: u64) {
        let input = ExecInput { target: Some(run_to), ..Default::default() };
        let mut stage = IndexAccountHistoryStage::default();
        let factory = ProviderFactory::new(tx.tx.as_ref(), MAINNET.clone());
        let provider = factory.provider_rw().unwrap();
        let out = stage.execute(&provider, input).await.unwrap();
        assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(5), done: true });
        provider.commit().unwrap();
    }

    async fn unwind(tx: &TestTransaction, unwind_from: u64, unwind_to: u64) {
        let input = UnwindInput {
            checkpoint: StageCheckpoint::new(unwind_from),
            unwind_to,
            ..Default::default()
        };
        let mut stage = IndexAccountHistoryStage::default();
        let factory = ProviderFactory::new(tx.tx.as_ref(), MAINNET.clone());
        let provider = factory.provider_rw().unwrap();
        let out = stage.unwind(&provider, input).await.unwrap();
        assert_eq!(out, UnwindOutput { checkpoint: StageCheckpoint::new(unwind_to) });
        provider.commit().unwrap();
    }

    #[tokio::test]
    async fn insert_index_to_empty() {
        // init
        let tx = TestTransaction::default();

        // setup
        partial_setup(&tx);

        // run
        run(&tx, 5).await;

        // verify
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(table, BTreeMap::from([(shard(u64::MAX), vec![4, 5])]));

        // unwind
        unwind(&tx, 5, 0).await;

        // verify initial state
        let table = tx.table::<tables::AccountHistory>().unwrap();
        assert!(table.is_empty());
    }

    #[tokio::test]
    async fn insert_index_to_not_empty_shard() {
        // init
        let tx = TestTransaction::default();

        // setup
        partial_setup(&tx);
        tx.commit(|tx| {
            tx.put::<tables::AccountHistory>(shard(u64::MAX), list(&[1, 2, 3])).unwrap();
            Ok(())
        })
        .unwrap();

        // run
        run(&tx, 5).await;

        // verify
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(table, BTreeMap::from([(shard(u64::MAX), vec![1, 2, 3, 4, 5]),]));

        // unwind
        unwind(&tx, 5, 0).await;

        // verify initial state
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(table, BTreeMap::from([(shard(u64::MAX), vec![1, 2, 3]),]));
    }

    #[tokio::test]
    async fn insert_index_to_full_shard() {
        // init
        let tx = TestTransaction::default();
        let full_list = vec![3; NUM_OF_INDICES_IN_SHARD];

        // setup
        partial_setup(&tx);
        tx.commit(|tx| {
            tx.put::<tables::AccountHistory>(shard(u64::MAX), list(&full_list)).unwrap();
            Ok(())
        })
        .unwrap();

        // run
        run(&tx, 5).await;

        // verify
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(
            table,
            BTreeMap::from([(shard(3), full_list.clone()), (shard(u64::MAX), vec![4, 5])])
        );

        // unwind
        unwind(&tx, 5, 0).await;

        // verify initial state
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(table, BTreeMap::from([(shard(u64::MAX), full_list)]));
    }

    #[tokio::test]
    async fn insert_index_to_fill_shard() {
        // init
        let tx = TestTransaction::default();
        let mut close_full_list = vec![1; NUM_OF_INDICES_IN_SHARD - 2];

        // setup
        partial_setup(&tx);
        tx.commit(|tx| {
            tx.put::<tables::AccountHistory>(shard(u64::MAX), list(&close_full_list)).unwrap();
            Ok(())
        })
        .unwrap();

        // run
        run(&tx, 5).await;

        // verify
        close_full_list.push(4);
        close_full_list.push(5);
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(table, BTreeMap::from([(shard(u64::MAX), close_full_list.clone()),]));

        // unwind
        unwind(&tx, 5, 0).await;

        // verify initial state
        close_full_list.pop();
        close_full_list.pop();
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(table, BTreeMap::from([(shard(u64::MAX), close_full_list),]));

        // verify initial state
    }

    #[tokio::test]
    async fn insert_index_second_half_shard() {
        // init
        let tx = TestTransaction::default();
        let mut close_full_list = vec![1; NUM_OF_INDICES_IN_SHARD - 1];

        // setup
        partial_setup(&tx);
        tx.commit(|tx| {
            tx.put::<tables::AccountHistory>(shard(u64::MAX), list(&close_full_list)).unwrap();
            Ok(())
        })
        .unwrap();

        // run
        run(&tx, 5).await;

        // verify
        close_full_list.push(4);
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(
            table,
            BTreeMap::from([(shard(4), close_full_list.clone()), (shard(u64::MAX), vec![5])])
        );

        // unwind
        unwind(&tx, 5, 0).await;

        // verify initial state
        close_full_list.pop();
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(table, BTreeMap::from([(shard(u64::MAX), close_full_list),]));
    }

    #[tokio::test]
    async fn insert_index_to_third_shard() {
        // init
        let tx = TestTransaction::default();
        let full_list = vec![1; NUM_OF_INDICES_IN_SHARD];

        // setup
        partial_setup(&tx);
        tx.commit(|tx| {
            tx.put::<tables::AccountHistory>(shard(1), list(&full_list)).unwrap();
            tx.put::<tables::AccountHistory>(shard(2), list(&full_list)).unwrap();
            tx.put::<tables::AccountHistory>(shard(u64::MAX), list(&[2, 3])).unwrap();
            Ok(())
        })
        .unwrap();

        run(&tx, 5).await;

        // verify
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(
            table,
            BTreeMap::from([
                (shard(1), full_list.clone()),
                (shard(2), full_list.clone()),
                (shard(u64::MAX), vec![2, 3, 4, 5])
            ])
        );

        // unwind
        unwind(&tx, 5, 0).await;

        // verify initial state
        let table = cast(tx.table::<tables::AccountHistory>().unwrap());
        assert_eq!(
            table,
            BTreeMap::from([
                (shard(1), full_list.clone()),
                (shard(2), full_list.clone()),
                (shard(u64::MAX), vec![2, 3])
            ])
        );
    }

    stage_test_suite_ext!(IndexAccountHistoryTestRunner, index_account_history);

    struct IndexAccountHistoryTestRunner {
        pub(crate) tx: TestTransaction,
        commit_threshold: u64,
        prune_modes: PruneModes,
    }

    impl Default for IndexAccountHistoryTestRunner {
        fn default() -> Self {
            Self {
                tx: TestTransaction::default(),
                commit_threshold: 1000,
                prune_modes: PruneModes::default(),
            }
        }
    }

    impl StageTestRunner for IndexAccountHistoryTestRunner {
        type S = IndexAccountHistoryStage;

        fn tx(&self) -> &TestTransaction {
            &self.tx
        }

        fn stage(&self) -> Self::S {
            Self::S { commit_threshold: self.commit_threshold, prune_modes: self.prune_modes }
        }
    }

    impl ExecuteStageTestRunner for IndexAccountHistoryTestRunner {
        type Seed = ();

        fn seed_execution(&mut self, input: ExecInput) -> Result<Self::Seed, TestRunnerError> {
            let stage_process = input.checkpoint().block_number;
            let start = stage_process + 1;
            let end = input.target();
            let mut rng = generators::rng();

            let num_of_accounts = 31;
            let accounts = random_contract_account_range(&mut rng, &mut (0..num_of_accounts))
                .into_iter()
                .collect::<BTreeMap<_, _>>();

            let blocks = random_block_range(&mut rng, start..=end, H256::zero(), 0..3);

            let (transitions, _) = random_transition_range(
                &mut rng,
                blocks.iter(),
                accounts.into_iter().map(|(addr, acc)| (addr, (acc, Vec::new()))),
                0..3,
                0..256,
            );

            // add block changeset from block 1.
            self.tx.insert_transitions(transitions, Some(start))?;

            Ok(())
        }

        fn validate_execution(
            &self,
            input: ExecInput,
            output: Option<ExecOutput>,
        ) -> Result<(), TestRunnerError> {
            if let Some(output) = output {
                let start_block = input.next_block();
                let end_block = output.checkpoint.block_number;
                if start_block > end_block {
                    return Ok(())
                }

                assert_eq!(
                    output,
                    ExecOutput { checkpoint: StageCheckpoint::new(input.target()), done: true }
                );

                let provider = self.tx.inner();
                let mut changeset_cursor =
                    provider.tx_ref().cursor_read::<tables::AccountChangeSet>()?;

                let account_transitions =
                    changeset_cursor.walk_range(start_block..=end_block)?.try_fold(
                        BTreeMap::new(),
                        |mut accounts: BTreeMap<Address, Vec<u64>>,
                         entry|
                         -> Result<_, TestRunnerError> {
                            let (index, account) = entry?;
                            accounts.entry(account.address).or_default().push(index);
                            Ok(accounts)
                        },
                    )?;

                let mut result = BTreeMap::new();
                for (address, indices) in account_transitions {
                    // chunk indices and insert them in shards of N size.
                    let mut chunks = indices
                        .iter()
                        .chunks(sharded_key::NUM_OF_INDICES_IN_SHARD)
                        .into_iter()
                        .map(|chunks| chunks.map(|i| *i as usize).collect::<Vec<usize>>())
                        .collect::<Vec<_>>();
                    let last_chunk = chunks.pop();

                    chunks.into_iter().for_each(|list| {
                        result.insert(
                            ShardedKey::new(
                                address,
                                *list.last().expect("Chuck does not return empty list")
                                    as BlockNumber,
                            ) as ShardedKey<H160>,
                            list,
                        );
                    });

                    if let Some(last_list) = last_chunk {
                        result.insert(
                            ShardedKey::new(address, u64::MAX) as ShardedKey<H160>,
                            last_list,
                        );
                    };
                }

                let table = cast(self.tx.table::<tables::AccountHistory>().unwrap());
                assert_eq!(table, result);
            }
            Ok(())
        }
    }

    impl UnwindStageTestRunner for IndexAccountHistoryTestRunner {
        fn validate_unwind(&self, _input: UnwindInput) -> Result<(), TestRunnerError> {
            let table = self.tx.table::<tables::AccountHistory>().unwrap();
            assert!(table.is_empty());
            Ok(())
        }
    }
}
