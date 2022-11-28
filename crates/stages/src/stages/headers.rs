use crate::{
    db::StageDB, DatabaseIntegrityError, ExecInput, ExecOutput, Stage, StageError, StageId,
    UnwindInput, UnwindOutput,
};
use reth_interfaces::{
    consensus::{Consensus, ForkchoiceState},
    db::{models::blocks::BlockNumHash, tables, Database, DbCursorRO, DbCursorRW, DbTx, DbTxMut},
    p2p::headers::{
        client::HeadersClient,
        downloader::{ensure_parent, HeaderDownloader},
        error::DownloadError,
    },
};
use reth_primitives::{BlockNumber, SealedHeader, H256, U256};
use std::{fmt::Debug, sync::Arc};
use tracing::*;

const HEADERS: StageId = StageId("Headers");

/// The headers stage.
///
/// The headers stage downloads all block headers from the highest block in the local database to
/// the perceived highest block on the network.
///
/// The headers are processed and data is inserted into these tables:
///
/// - [`HeaderNumbers`][reth_interfaces::db::tables::HeaderNumbers]
/// - [`Headers`][reth_interfaces::db::tables::Headers]
/// - [`CanonicalHeaders`][reth_interfaces::db::tables::CanonicalHeaders]
/// - [`HeaderTD`][reth_interfaces::db::tables::HeaderTD]
#[derive(Debug)]
pub struct HeaderStage<D: HeaderDownloader, C: Consensus, H: HeadersClient> {
    /// Strategy for downloading the headers
    pub downloader: D,
    /// Consensus client implementation
    pub consensus: Arc<C>,
    /// Downloader client implementation
    pub client: Arc<H>,
}

#[async_trait::async_trait]
impl<DB: Database, D: HeaderDownloader, C: Consensus, H: HeadersClient> Stage<DB>
    for HeaderStage<D, C, H>
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        HEADERS
    }

    /// Download the headers in reverse order
    /// starting from the tip
    async fn execute(
        &mut self,
        db: &mut StageDB<'_, DB>,
        input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        let stage_progress = input.stage_progress.unwrap_or_default();
        self.update_head::<DB>(db, stage_progress).await?;

        // Lookup the last stored header
        let last_hash = db.get_block_hash(stage_progress)?;
        let last_header =
            db.get::<tables::Headers>((stage_progress, last_hash).into())?.ok_or({
                DatabaseIntegrityError::Header { number: stage_progress, hash: last_hash }
            })?;
        let head = SealedHeader::new(last_header, last_hash);

        let forkchoice = self.next_fork_choice_state(&head.hash()).await;
        if let Some(number) = db.get::<tables::HeaderNumbers>(forkchoice.head_block_hash)? {
            if number < head.number {
                // Nothing to do here
                warn!("Consensus reported old head {}", forkchoice.head_block_hash);
                return Ok(ExecOutput { stage_progress, done: true, reached_tip: true })
            }
        }

        // The stage relies on the downloader to return the headers
        // in descending order starting from the tip down to
        // the local head (latest block in db)
        // TODO: add batching
        let headers = match self.downloader.download(head.clone(), forkchoice.clone()).await {
            Ok(res) => {
                // Perform basic response validation
                self.validate_header_response(&res, head, forkchoice)?;
                res
            }
            Err(e) => match e {
                DownloadError::Timeout => {
                    warn!("No response for header request");
                    return Ok(ExecOutput { stage_progress, reached_tip: false, done: false })
                }
                DownloadError::HeaderValidation { hash, error } => {
                    warn!("Validation error for header {hash}: {error}");
                    return Err(StageError::Validation { block: stage_progress, error })
                }
                error => {
                    warn!("Unexpected error occurred: {error}");
                    return Err(StageError::Download(error.to_string()))
                }
            },
        };
        let stage_progress = self.write_headers::<DB>(db, headers).await?.unwrap_or(stage_progress);
        Ok(ExecOutput { stage_progress, reached_tip: true, done: true })
    }

    /// Unwind the stage.
    async fn unwind(
        &mut self,
        db: &mut StageDB<'_, DB>,
        input: UnwindInput,
    ) -> Result<UnwindOutput, Box<dyn std::error::Error + Send + Sync>> {
        // TODO: handle bad block
        db.unwind_table_by_walker::<tables::CanonicalHeaders, tables::HeaderNumbers>(
            input.unwind_to + 1,
        )?;
        db.unwind_table_by_num::<tables::CanonicalHeaders>(input.unwind_to)?;
        db.unwind_table_by_num_hash::<tables::Headers>(input.unwind_to)?;
        db.unwind_table_by_num_hash::<tables::HeaderTD>(input.unwind_to)?;
        Ok(UnwindOutput { stage_progress: input.unwind_to })
    }
}

impl<D: HeaderDownloader, C: Consensus, H: HeadersClient> HeaderStage<D, C, H> {
    async fn update_head<DB: Database>(
        &self,
        db: &StageDB<'_, DB>,
        height: BlockNumber,
    ) -> Result<(), StageError> {
        let block_key = db.get_block_numhash(height)?;
        let td: U256 = *db
            .get::<tables::HeaderTD>(block_key)?
            .ok_or(DatabaseIntegrityError::TotalDifficulty { number: height })?;
        self.client.update_status(height, block_key.hash(), td);
        Ok(())
    }

    async fn next_fork_choice_state(&self, head: &H256) -> ForkchoiceState {
        let mut state_rcv = self.consensus.fork_choice_state();
        loop {
            let _ = state_rcv.changed().await;
            let forkchoice = state_rcv.borrow();
            if !forkchoice.head_block_hash.is_zero() && forkchoice.head_block_hash != *head {
                return forkchoice.clone()
            }
        }
    }

    /// Perform basic header response validation
    fn validate_header_response(
        &self,
        headers: &[SealedHeader],
        head: SealedHeader,
        forkchoice: ForkchoiceState,
    ) -> Result<(), StageError> {
        // The response must include at least head and tip
        if headers.len() < 2 {
            return Err(StageError::Download("Not enough headers".to_owned()))
        }

        let mut headers_iter = headers.iter().rev().peekable();
        if headers_iter.peek().unwrap().hash() != forkchoice.head_block_hash {
            return Err(StageError::Download("Response must end with tip".to_owned()))
        }

        while let Some(header) = headers_iter.next() {
            ensure_parent(header, headers_iter.peek().unwrap_or(&&head))
                .map_err(|err| StageError::Download(err.to_string()))?;
        }

        Ok(())
    }

    /// Write downloaded headers to the database
    async fn write_headers<DB: Database>(
        &self,
        db: &StageDB<'_, DB>,
        headers: Vec<SealedHeader>,
    ) -> Result<Option<BlockNumber>, StageError> {
        let mut cursor_header = db.cursor_mut::<tables::Headers>()?;
        let mut cursor_canonical = db.cursor_mut::<tables::CanonicalHeaders>()?;
        let mut cursor_td = db.cursor_mut::<tables::HeaderTD>()?;
        let mut td: U256 = cursor_td.last()?.map(|(_, v)| v).unwrap().into();

        let mut latest = None;
        // Since the headers were returned in descending order,
        // iterate them in the reverse order
        for header in headers.into_iter() {
            if header.number == 0 {
                continue
            }

            let block_hash = header.hash();
            let key: BlockNumHash = (header.number, block_hash).into();
            let header = header.unseal();
            latest = Some(header.number);

            td += header.difficulty;

            // NOTE: HeaderNumbers are not sorted and can't be inserted with cursor.
            db.put::<tables::HeaderNumbers>(block_hash, header.number)?;
            cursor_header.append(key, header)?;
            cursor_canonical.append(key.number(), key.hash())?;
            cursor_td.append(key, td.into())?;
        }

        Ok(latest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{
        stage_test_suite, ExecuteStageTestRunner, UnwindStageTestRunner, PREV_STAGE_ID,
    };
    use assert_matches::assert_matches;
    use reth_interfaces::p2p::error::RequestError;
    use test_runner::HeadersTestRunner;

    stage_test_suite!(HeadersTestRunner);

    /// Check that the execution errors on empty database or
    /// prev progress missing from the database.
    #[tokio::test]
    // Validate that the execution does not fail on timeout
    async fn execute_timeout() {
        let (previous_stage, stage_progress) = (500, 100);
        let mut runner = HeadersTestRunner::default();
        let input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };
        runner.seed_execution(input).expect("failed to seed execution");
        runner.client.set_error(RequestError::Timeout).await;
        let rx = runner.execute(input);
        runner.consensus.update_tip(H256::from_low_u64_be(1));
        let result = rx.await.unwrap();
        assert_matches!(
            result,
            Ok(ExecOutput { done: false, reached_tip: false, stage_progress: 100 })
        );
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "validation failed");
    }

    /// Check that validation error is propagated during the execution.
    #[tokio::test]
    async fn execute_validation_error() {
        let mut runner = HeadersTestRunner::default();
        runner.consensus.set_fail_validation(true);
        let (stage_progress, previous_stage) = (1000, 1200);
        let input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };
        let headers = runner.seed_execution(input).expect("failed to seed execution");
        let rx = runner.execute(input);
        runner.after_execution(headers).await.expect("failed to run after execution hook");
        let result = rx.await.unwrap();
        assert_matches!(result, Err(StageError::Validation { .. }));
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "validation failed");
    }

    /// Check that unexpected download errors are caught
    #[tokio::test]
    async fn executed_download_error() {
        let mut runner = HeadersTestRunner::default();
        let (stage_progress, previous_stage) = (1000, 1200);
        let input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };
        let headers = runner.seed_execution(input).expect("failed to seed execution");
        let rx = runner.execute(input);

        runner.client.set_error(RequestError::BadResponse).await;

        // Update tip
        let tip = headers.last().unwrap();
        runner.consensus.update_tip(tip.hash());

        let result = rx.await.unwrap();
        assert_matches!(result, Err(StageError::Download(_)));
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "validation failed");
    }

    /// Execute the stage with linear downloader
    #[tokio::test]
    async fn execute_with_linear_downloader() {
        let mut runner = HeadersTestRunner::with_linear_downloader();
        let (stage_progress, previous_stage) = (1000, 1200);
        let input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };
        let headers = runner.seed_execution(input).expect("failed to seed execution");
        let rx = runner.execute(input);

        runner.client.extend(headers.iter().rev().map(|h| h.clone().unseal())).await;

        // skip `after_execution` hook for linear downloader
        let tip = headers.last().unwrap();
        runner.consensus.update_tip(tip.hash());

        let result = rx.await.unwrap();
        assert_matches!(
            result,
            Ok(ExecOutput { done: true, reached_tip: true, stage_progress })
                if stage_progress == tip.number
        );
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "validation failed");
    }

    mod test_runner {
        use crate::{
            stages::headers::HeaderStage,
            test_utils::{
                ExecuteStageTestRunner, StageTestRunner, TestRunnerError, TestStageDB,
                UnwindStageTestRunner,
            },
            ExecInput, ExecOutput, UnwindInput,
        };
        use reth_headers_downloaders::linear::{LinearDownloadBuilder, LinearDownloader};
        use reth_interfaces::{
            db::{models::blocks::BlockNumHash, tables, DbTx},
            p2p::headers::downloader::HeaderDownloader,
            test_utils::{
                generators::{random_header, random_header_range},
                TestConsensus, TestHeaderDownloader, TestHeadersClient,
            },
        };
        use reth_primitives::{BlockNumber, SealedHeader, U256};
        use std::sync::Arc;

        pub(crate) struct HeadersTestRunner<D: HeaderDownloader> {
            pub(crate) consensus: Arc<TestConsensus>,
            pub(crate) client: Arc<TestHeadersClient>,
            downloader: Arc<D>,
            db: TestStageDB,
        }

        impl Default for HeadersTestRunner<TestHeaderDownloader> {
            fn default() -> Self {
                let client = Arc::new(TestHeadersClient::default());
                let consensus = Arc::new(TestConsensus::default());
                Self {
                    client: client.clone(),
                    consensus: consensus.clone(),
                    downloader: Arc::new(TestHeaderDownloader::new(client, consensus, 1000)),
                    db: TestStageDB::default(),
                }
            }
        }

        impl<D: HeaderDownloader + 'static> StageTestRunner for HeadersTestRunner<D> {
            type S = HeaderStage<Arc<D>, TestConsensus, TestHeadersClient>;

            fn db(&self) -> &TestStageDB {
                &self.db
            }

            fn stage(&self) -> Self::S {
                HeaderStage {
                    consensus: self.consensus.clone(),
                    client: self.client.clone(),
                    downloader: self.downloader.clone(),
                }
            }
        }

        #[async_trait::async_trait]
        impl<D: HeaderDownloader + 'static> ExecuteStageTestRunner for HeadersTestRunner<D> {
            type Seed = Vec<SealedHeader>;

            fn seed_execution(&mut self, input: ExecInput) -> Result<Self::Seed, TestRunnerError> {
                let start = input.stage_progress.unwrap_or_default();
                let head = random_header(start, None);
                self.db.insert_headers(std::iter::once(&head))?;

                // use previous progress as seed size
                let end = input.previous_stage.map(|(_, num)| num).unwrap_or_default() + 1;

                if start + 1 >= end {
                    return Ok(Vec::default())
                }

                let mut headers = random_header_range(start + 1..end, head.hash());
                headers.insert(0, head);
                Ok(headers)
            }

            /// Validate stored headers
            fn validate_execution(
                &self,
                input: ExecInput,
                output: Option<ExecOutput>,
            ) -> Result<(), TestRunnerError> {
                let initial_stage_progress = input.stage_progress.unwrap_or_default();
                match output {
                    Some(output) if output.stage_progress > initial_stage_progress => {
                        self.db.query(|tx| {
                            for block_num in (initial_stage_progress..output.stage_progress).rev() {
                                // look up the header hash
                                let hash = tx
                                    .get::<tables::CanonicalHeaders>(block_num)?
                                    .expect("no header hash");
                                let key: BlockNumHash = (block_num, hash).into();

                                // validate the header number
                                assert_eq!(tx.get::<tables::HeaderNumbers>(hash)?, Some(block_num));

                                // validate the header
                                let header = tx.get::<tables::Headers>(key)?;
                                assert!(header.is_some());
                                let header = header.unwrap().seal();
                                assert_eq!(header.hash(), hash);

                                // validate td consistency in the database
                                if header.number > initial_stage_progress {
                                    let parent_td = tx.get::<tables::HeaderTD>(
                                        (header.number - 1, header.parent_hash).into(),
                                    )?;
                                    let td: U256 = *tx.get::<tables::HeaderTD>(key)?.unwrap();
                                    assert_eq!(
                                        parent_td.map(|td| *td + header.difficulty),
                                        Some(td)
                                    );
                                }
                            }
                            Ok(())
                        })?;
                    }
                    _ => self.check_no_header_entry_above(initial_stage_progress)?,
                };
                Ok(())
            }

            async fn after_execution(&self, headers: Self::Seed) -> Result<(), TestRunnerError> {
                self.client.extend(headers.iter().map(|h| h.clone().unseal())).await;
                let tip = if !headers.is_empty() {
                    headers.last().unwrap().hash()
                } else {
                    let tip = random_header(0, None);
                    self.db.insert_headers(std::iter::once(&tip))?;
                    tip.hash()
                };
                self.consensus.update_tip(tip);
                Ok(())
            }
        }

        impl<D: HeaderDownloader + 'static> UnwindStageTestRunner for HeadersTestRunner<D> {
            fn validate_unwind(&self, input: UnwindInput) -> Result<(), TestRunnerError> {
                self.check_no_header_entry_above(input.unwind_to)
            }
        }

        impl HeadersTestRunner<LinearDownloader<TestConsensus, TestHeadersClient>> {
            #[allow(unused)]
            pub(crate) fn with_linear_downloader() -> Self {
                let client = Arc::new(TestHeadersClient::default());
                let consensus = Arc::new(TestConsensus::default());
                let downloader = Arc::new(
                    LinearDownloadBuilder::default().build(consensus.clone(), client.clone()),
                );
                Self { client, consensus, downloader, db: TestStageDB::default() }
            }
        }

        impl<D: HeaderDownloader> HeadersTestRunner<D> {
            pub(crate) fn check_no_header_entry_above(
                &self,
                block: BlockNumber,
            ) -> Result<(), TestRunnerError> {
                self.db
                    .check_no_entry_above_by_value::<tables::HeaderNumbers, _>(block, |val| val)?;
                self.db.check_no_entry_above::<tables::CanonicalHeaders, _>(block, |key| key)?;
                self.db.check_no_entry_above::<tables::Headers, _>(block, |key| key.number())?;
                self.db.check_no_entry_above::<tables::HeaderTD, _>(block, |key| key.number())?;
                Ok(())
            }
        }
    }
}
