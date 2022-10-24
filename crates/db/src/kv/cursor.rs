//! Cursor wrapper for libmdbx-sys.

use std::marker::PhantomData;

use crate::utils::*;
use libmdbx::{self, TransactionKind, WriteFlags, RO, RW};
use reth_interfaces::db::{
    DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW, DupSort, DupWalker, Encode, Error, Table,
    Walker,
};

/// Alias type for a `(key, value)` result coming from a cursor.
pub type PairResult<T> = Result<Option<(<T as Table>::Key, <T as Table>::Value)>, Error>;
/// Alias type for a `(key, value)` result coming from an iterator.
pub type IterPairResult<T> = Option<Result<(<T as Table>::Key, <T as Table>::Value), Error>>;
/// Alias type for a value result coming from a cursor without its key.
pub type ValueOnlyResult<T> = Result<Option<<T as Table>::Value>, Error>;

/// Read only Cursor.
pub type CursorRO<'tx, T> = Cursor<'tx, RO, T>;
/// Read write cursor.
pub type CursorRW<'tx, T> = Cursor<'tx, RW, T>;

/// Cursor wrapper to access KV items.
#[derive(Debug)]
pub struct Cursor<'tx, K: TransactionKind, T: Table> {
    /// Inner `libmdbx` cursor.
    pub inner: libmdbx::Cursor<'tx, K>,
    /// Table name as is inside the database.
    pub table: &'static str,
    /// Phantom data to enforce encoding/decoding.
    pub _dbi: std::marker::PhantomData<T>,
}

/// Takes `(key, value)` from the database and decodes it appropriately.
#[macro_export]
macro_rules! decode {
    ($v:expr) => {
        $v.map_err(|e| Error::Decode(e.into()))?.map(decoder::<T>).transpose()
    };
}

impl<'tx, K: TransactionKind, T: Table> DbCursorRO<'tx, T> for Cursor<'tx, K, T> {
    fn first(&mut self) -> reth_interfaces::db::PairResult<T> {
        decode!(self.inner.first())
    }

    fn seek(&mut self, key: <T as Table>::SeekKey) -> reth_interfaces::db::PairResult<T> {
        decode!(self.inner.set_range(key.encode().as_ref()))
    }

    fn seek_exact(&mut self, key: <T as Table>::Key) -> reth_interfaces::db::PairResult<T> {
        decode!(self.inner.set_key(key.encode().as_ref()))
    }

    fn next(&mut self) -> reth_interfaces::db::PairResult<T> {
        decode!(self.inner.next())
    }

    fn prev(&mut self) -> reth_interfaces::db::PairResult<T> {
        decode!(self.inner.prev())
    }

    fn last(&mut self) -> reth_interfaces::db::PairResult<T> {
        decode!(self.inner.last())
    }

    fn current(&mut self) -> reth_interfaces::db::PairResult<T> {
        decode!(self.inner.get_current())
    }

    fn walk<'cursor>(
        &'cursor mut self,
        start_key: T::Key,
    ) -> Result<Walker<'cursor, 'tx, T, Self>, Error>
    where
        Self: Sized,
    {
        let start = self
            .inner
            .set_range(start_key.encode().as_ref())
            .map_err(|e| Error::Internal(e.into()))?
            .map(decoder::<T>);

        Ok(Walker::<'cursor, 'tx, T, Self> { cursor: self, start, _tx_phantom: PhantomData {} })
    }
}

impl<'tx, K: TransactionKind, T: DupSort> DbDupCursorRO<'tx, T> for Cursor<'tx, K, T> {
    /// Returns the next `(key, value)` pair of a DUPSORT table.
    fn next_dup(&mut self) -> PairResult<T> {
        decode!(self.inner.next_dup())
    }

    /// Returns the next `(key, value)` pair skipping the duplicates.
    fn next_no_dup(&mut self) -> PairResult<T> {
        decode!(self.inner.next_nodup())
    }

    /// Returns the next `value` of a duplicate `key`.
    fn next_dup_val(&mut self) -> ValueOnlyResult<T> {
        self.inner
            .next_dup()
            .map_err(|e| Error::Internal(e.into()))?
            .map(decode_value::<T>)
            .transpose()
    }

    /// Returns an iterator starting at a key greater or equal than `start_key` of a DUPSORT table.
    fn walk_dup<'cursor>(
        &'cursor mut self,
        key: T::Key,
        subkey: T::SubKey,
    ) -> Result<DupWalker<'cursor, 'tx, T, Self>, Error> {
        let start = self
            .inner
            .get_both_range(key.encode().as_ref(), subkey.encode().as_ref())
            .map_err(|e| Error::Internal(e.into()))?
            .map(decode_one::<T>);

        Ok(DupWalker::<'cursor, 'tx, T, Self> { cursor: self, start, _tx_phantom: PhantomData {} })
    }
}

impl<'tx, T: Table> DbCursorRW<'tx, T> for Cursor<'tx, RW, T> {
    /// Database operation that will update an existing row if a specified value already
    /// exists in a table, and insert a new row if the specified value doesn't already exist
    fn upsert(&mut self, key: T::Key, value: T::Value) -> Result<(), Error> {
        self.inner
            .put(key.encode().as_ref(), value.encode().as_ref(), WriteFlags::UPSERT)
            .map_err(|e| Error::Internal(e.into()))
    }

    fn append(&mut self, key: T::Key, value: T::Value) -> Result<(), Error> {
        self.inner
            .put(key.encode().as_ref(), value.encode().as_ref(), WriteFlags::APPEND)
            .map_err(|e| Error::Internal(e.into()))
    }

    fn delete_current(&mut self) -> Result<(), Error> {
        self.inner.del(WriteFlags::CURRENT).map_err(|e| Error::Internal(e.into()))
    }
}

impl<'tx, T: DupSort> DbDupCursorRW<'tx, T> for Cursor<'tx, RW, T> {
    fn delete_current_duplicates(&mut self) -> Result<(), Error> {
        self.inner.del(WriteFlags::NO_DUP_DATA).map_err(|e| Error::Internal(e.into()))
    }

    fn append_dup(&mut self, key: T::Key, value: T::Value) -> Result<(), Error> {
        self.inner
            .put(key.encode().as_ref(), value.encode().as_ref(), WriteFlags::APPEND_DUP)
            .map_err(|e| Error::Internal(e.into()))
    }
}
