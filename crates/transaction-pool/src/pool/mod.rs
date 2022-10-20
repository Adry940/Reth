//! Transaction Pool internals.
//!
//! Incoming transactions are before they enter the pool first. The validation outcome can have 3
//! states:
//!
//!  1. Transaction can _never_ be valid
//!  2. Transaction is _currently_ valid
//!  3. Transaction is _currently_ invalid, but could potentially become valid in the future
//!
//! However, (2.) and (3.) of a transaction can only be determined on the basis of the current
//! state, whereas (1.) holds indefinitely. This means once the state changes (2.) and (3.) the
//! state of a transaction needs to be reevaluated again.
//!
//! The transaction pool is responsible for storing new, valid transactions and providing the next
//! best transactions sorted by their priority. Where priority is determined by the transaction's
//! score ([`TransactionOrdering`]).
//!
//! Furthermore, the following characteristics fall under (3.):
//!
//!  a) Nonce of a transaction is higher than the expected nonce for the next transaction of its
//! sender. A distinction is made here whether multiple transactions from the same sender have
//! gapless nonce increments.
//!
//!  a)(1) If _no_ transaction is missing in a chain of multiple
//! transactions from the same sender (all nonce in row), all of them can in principle be executed
//! on the current state one after the other.
//!
//!  a)(2) If there's a nonce gap, then all
//! transactions after the missing transaction are blocked until the missing transaction arrives.
//!
//!  b) Transaction does not meet the dynamic fee cap requirement introduced by EIP-1559: The
//! fee cap of the transaction needs to be no less than the base fee of block.
//!
//!
//! In essence the transaction pool is made of three separate sub-pools:
//!
//!  - Pending Pool: Contains all transactions that are valid on the current state and satisfy
//! (3. a)(1): _No_ nonce gaps
//!
//!  - Queued Pool: Contains all transactions that are currently blocked by missing
//! transactions: (3. a)(2): _With_ nonce gaps or due to lack of funds.
//!
//!  - Basefee Pool: To account for the dynamic base fee requirement (3. b) which could render
//! an EIP-1559 and all subsequent transactions of the sender currently invalid.
//!
//! The classification of transactions is always dependent on the current state that is changed as
//! soon as a new block is mined. Once a new block is mined, the account changeset must be applied
//! to the transaction pool.
//!
//!
//! Depending on the use case, consumers of the [`TransactionPool`](crate::traits::TransactionPool)
//! are interested in (2.) and/or (3.).

//! A generic [`TransactionPool`](crate::traits::TransactionPool) that only handles transactions.
//!
//! This Pool maintains two separate sub-pools for (2.) and (3.)
//!
//! ## Terminology
//!
//!  - _Pending_: pending transactions are transactions that fall under (2.). Those transactions are
//!    _currently_ ready to be executed and are stored in the pending sub-pool
//!  - _Queued_: queued transactions are transactions that fall under category (3.). Those
//!    transactions are _currently_ waiting for state changes that eventually move them into
//!    category (2.) and become pending.

use crate::{
    error::{PoolError, PoolResult},
    identifier::{SenderId, SenderIdentifiers, TransactionId},
    pool::{listener::PoolEventListener, state::SubPool, txpool::TxPool},
    traits::{NewTransactionEvent, PoolStatus, PoolTransaction, TransactionOrigin},
    validate::{TransactionValidationOutcome, ValidPoolTransaction},
    PoolConfig, TransactionOrdering, TransactionValidator, U256,
};
use best::BestTransactions;
pub use events::TransactionEvent;
use futures::channel::mpsc::{channel, Receiver, Sender};
use parking_lot::{Mutex, RwLock};
use reth_primitives::{Address, TxHash};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};
use tracing::warn;

mod best;
mod events;
mod listener;
mod parked;
mod pending;
pub(crate) mod size;
pub(crate) mod state;
mod transaction;
pub mod txpool;

/// Transaction pool internals.
pub struct PoolInner<V: TransactionValidator, T: TransactionOrdering> {
    /// Internal mapping of addresses to plain ints.
    identifiers: RwLock<SenderIdentifiers>,
    /// Transaction validation.
    validator: Arc<V>,
    /// The internal pool that manages all transactions.
    pool: RwLock<TxPool<T>>,
    /// Pool settings.
    config: PoolConfig,
    /// Manages listeners for transaction state change events.
    event_listener: RwLock<PoolEventListener<TxHash>>,
    /// Listeners for new ready transactions.
    pending_transaction_listener: Mutex<Vec<Sender<TxHash>>>,
    /// Listeners for new transactions added to the pool.
    transaction_listener: Mutex<Vec<Sender<NewTransactionEvent<T::Transaction>>>>,
}

// === impl PoolInner ===

impl<V: TransactionValidator, T: TransactionOrdering> PoolInner<V, T>
where
    V: TransactionValidator,
    T: TransactionOrdering<Transaction = <V as TransactionValidator>::Transaction>,
{
    /// Create a new transaction pool instance.
    pub fn new(validator: Arc<V>, ordering: Arc<T>, config: PoolConfig) -> Self {
        Self {
            identifiers: Default::default(),
            validator,
            event_listener: Default::default(),
            pool: RwLock::new(TxPool::new(ordering, config.clone())),
            pending_transaction_listener: Default::default(),
            transaction_listener: Default::default(),
            config,
        }
    }

    /// Returns stats about the pool.
    pub(crate) fn status(&self) -> PoolStatus {
        self.pool.read().status()
    }

    /// Returns the internal `SenderId` for this address
    pub(crate) fn get_sender_id(&self, addr: Address) -> SenderId {
        self.identifiers.write().sender_id_or_create(addr)
    }

    /// Updates the pool
    pub(crate) fn update_base_fee(&self, base_fee: U256) {
        self.pool.write().update_base_fee(base_fee);
    }

    /// Get the validator reference.
    pub fn validator(&self) -> &V {
        &self.validator
    }

    /// Adds a new transaction listener to the pool that gets notified about every new _ready_
    /// transaction
    pub fn add_pending_listener(&self) -> Receiver<TxHash> {
        const TX_LISTENER_BUFFER_SIZE: usize = 2048;
        let (tx, rx) = channel(TX_LISTENER_BUFFER_SIZE);
        self.pending_transaction_listener.lock().push(tx);
        rx
    }

    /// Adds a new transaction listener to the pool that gets notified about every new transaction
    pub fn add_transaction_listener(&self) -> Receiver<NewTransactionEvent<T::Transaction>> {
        const TX_LISTENER_BUFFER_SIZE: usize = 1024;
        let (tx, rx) = channel(TX_LISTENER_BUFFER_SIZE);
        self.transaction_listener.lock().push(tx);
        rx
    }

    /// Resubmits transactions back into the pool.
    pub fn resubmit(&self, _transactions: HashMap<TxHash, ValidPoolTransaction<T::Transaction>>) {
        unimplemented!()
    }

    /// Add a single validated transaction into the pool.
    ///
    /// Note: this is only used internally by [`Self::add_transactions()`], all new transaction(s)
    /// come in through that function, either as a batch or `std::iter::once`.
    fn add_transaction(
        &self,
        origin: TransactionOrigin,
        tx: TransactionValidationOutcome<T::Transaction>,
    ) -> PoolResult<TxHash> {
        match tx {
            TransactionValidationOutcome::Valid { balance, state_nonce, transaction } => {
                let sender_id = self.get_sender_id(*transaction.sender());
                let transaction_id = TransactionId::new(sender_id, transaction.nonce());

                let tx = ValidPoolTransaction {
                    cost: transaction.cost(),
                    transaction,
                    transaction_id,
                    propagate: false,
                    timestamp: Instant::now(),
                    origin,
                };

                let added = self.pool.write().add_transaction(tx, balance, state_nonce)?;
                let hash = *added.hash();

                // Notify about new pending transactions
                if let Some(pending_hash) = added.as_pending() {
                    self.on_new_pending_transaction(pending_hash);
                }

                // Notify tx event listeners
                self.notify_event_listeners(&added);

                // Notify listeners for _all_ transactions
                self.on_new_transaction(added.into_new_transaction_event());

                Ok(hash)
            }
            TransactionValidationOutcome::Invalid(_tx, err) => {
                // TODO notify listeners about invalid
                Err(err)
            }
        }
    }

    /// Adds all transactions in the iterator to the pool, returning a list of results.
    pub fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: impl IntoIterator<Item = TransactionValidationOutcome<T::Transaction>>,
    ) -> Vec<PoolResult<TxHash>> {
        let added =
            transactions.into_iter().map(|tx| self.add_transaction(origin, tx)).collect::<Vec<_>>();

        // If at least one transaction was added successfully, then we enforce the pool size limits.
        let discarded =
            if added.iter().any(Result::is_ok) { self.discard_worst() } else { Default::default() };

        if discarded.is_empty() {
            return added
        }

        // It may happen that a newly added transaction is immediately discarded, so we need to
        // adjust the result here
        added
            .into_iter()
            .map(|res| match res {
                Ok(ref hash) if discarded.contains(hash) => {
                    Err(PoolError::DiscardedOnInsert(*hash))
                }
                other => other,
            })
            .collect()
    }

    /// Notify all listeners about a new pending transaction.
    fn on_new_pending_transaction(&self, ready: &TxHash) {
        let mut transaction_listeners = self.pending_transaction_listener.lock();
        transaction_listeners.retain_mut(|listener| match listener.try_send(*ready) {
            Ok(()) => true,
            Err(e) => {
                if e.is_full() {
                    warn!(
                        target: "txpool",
                        "[{:?}] dropping full ready transaction listener",
                        ready,
                    );
                    true
                } else {
                    false
                }
            }
        });
    }

    /// Notify all listeners about a new pending transaction.
    fn on_new_transaction(&self, event: NewTransactionEvent<T::Transaction>) {
        let mut transaction_listeners = self.transaction_listener.lock();

        transaction_listeners.retain_mut(|listener| match listener.try_send(event.clone()) {
            Ok(()) => true,
            Err(e) => {
                if e.is_full() {
                    warn!(
                        target: "txpool",
                        "dropping full transaction listener",
                    );
                    true
                } else {
                    false
                }
            }
        });
    }

    /// Fire events for the newly added transaction.
    fn notify_event_listeners(&self, tx: &AddedTransaction<T::Transaction>) {
        let mut listener = self.event_listener.write();

        match tx {
            AddedTransaction::Pending(tx) => {
                listener.ready(tx.transaction.hash(), None);
                // TODO  more listeners for discarded, removed etc...
            }
            AddedTransaction::Parked { transaction, .. } => {
                listener.queued(transaction.hash());
            }
        }
    }

    /// Returns an iterator that yields transactions that are ready to be included in the block.
    pub(crate) fn ready_transactions(&self) -> BestTransactions<T> {
        self.pool.read().best_transactions()
    }

    /// Returns the transaction by hash.
    pub(crate) fn get(
        &self,
        tx_hash: &TxHash,
    ) -> Option<Arc<ValidPoolTransaction<T::Transaction>>> {
        self.pool.read().get(tx_hash)
    }

    /// Returns all the transactions belonging to the hashes.
    ///
    /// If no transaction exists, it is skipped.
    pub(crate) fn get_all(
        &self,
        txs: impl IntoIterator<Item = TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<T::Transaction>>> {
        self.pool.read().get_all(txs).collect()
    }

    /// Number of transactions in the entire pool
    pub(crate) fn len(&self) -> usize {
        self.pool.read().len()
    }

    /// Whether the pool is empty
    pub(crate) fn is_empty(&self) -> bool {
        self.pool.read().is_empty()
    }

    /// Enforces the size limits of pool and returns the discarded transactions if violated.
    pub(crate) fn discard_worst(&self) -> HashSet<TxHash> {
        self.pool.write().discard_worst().into_iter().map(|tx| *tx.hash()).collect()
    }
}

/// Tracks an added transaction and all graph changes caused by adding it.
#[derive(Debug, Clone)]
pub struct AddedPendingTransaction<T: PoolTransaction> {
    /// Inserted transaction.
    transaction: Arc<ValidPoolTransaction<T>>,
    /// transactions promoted to the ready queue
    promoted: Vec<TxHash>,
    /// transaction that failed and became discarded
    discarded: Vec<TxHash>,
    /// Transactions removed from the Ready pool
    removed: Vec<Arc<ValidPoolTransaction<T>>>,
}

impl<T: PoolTransaction> AddedPendingTransaction<T> {
    /// Create a new, empty transaction.
    fn new(transaction: Arc<ValidPoolTransaction<T>>) -> Self {
        Self {
            transaction,
            promoted: Default::default(),
            discarded: Default::default(),
            removed: Default::default(),
        }
    }
}

/// Represents a transaction that was added into the pool and its state
#[derive(Debug, Clone)]
pub enum AddedTransaction<T: PoolTransaction> {
    /// Transaction was successfully added and moved to the pending pool.
    Pending(AddedPendingTransaction<T>),
    /// Transaction was successfully added but not yet ready for processing and moved to a
    /// parked pool instead.
    Parked {
        /// Inserted transaction.
        transaction: Arc<ValidPoolTransaction<T>>,
        /// The subpool it was moved to.
        subpool: SubPool,
    },
}

impl<T: PoolTransaction> AddedTransaction<T> {
    /// Returns the hash of the transaction if it's pending
    pub(crate) fn as_pending(&self) -> Option<&TxHash> {
        if let AddedTransaction::Pending(tx) = self {
            Some(tx.transaction.hash())
        } else {
            None
        }
    }

    /// Returns the hash of the transaction
    pub(crate) fn hash(&self) -> &TxHash {
        match self {
            AddedTransaction::Pending(tx) => tx.transaction.hash(),
            AddedTransaction::Parked { transaction, .. } => transaction.hash(),
        }
    }

    /// Converts this type into the event type for listeners
    pub(crate) fn into_new_transaction_event(self) -> NewTransactionEvent<T> {
        match self {
            AddedTransaction::Pending(tx) => {
                NewTransactionEvent { subpool: SubPool::Pending, transaction: tx.transaction }
            }
            AddedTransaction::Parked { transaction, subpool } => {
                NewTransactionEvent { transaction, subpool }
            }
        }
    }
}
