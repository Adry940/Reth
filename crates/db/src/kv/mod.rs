//! Module that interacts with MDBX.

use crate::utils::default_page_size;
use libmdbx::{
    DatabaseFlags, Environment, EnvironmentFlags, EnvironmentKind, Geometry, Mode, PageSize,
    SyncMode, RO, RW,
};
use reth_interfaces::db::{
    tables::{TableType, TABLES},
    Database, Error,
};
use std::{ops::Deref, path::Path};

pub mod cursor;

pub mod tx;
use tx::Tx;

/// Environment used when opening a MDBX environment. RO/RW.
#[derive(Debug)]
pub enum EnvKind {
    /// Read-only MDBX environment.
    RO,
    /// Read-write MDBX environment.
    RW,
}

/// Wrapper for the libmdbx environment.
#[derive(Debug)]
pub struct Env<E: EnvironmentKind> {
    /// Libmdbx-sys environment.
    pub inner: Environment<E>,
}

impl<E: EnvironmentKind> Database for Env<E> {
    type TX<'a> = tx::Tx<'a, RO, E>;
    type TXMut<'a> = tx::Tx<'a, RW, E>;

    fn tx(&self) -> Result<Self::TX<'_>, Error> {
        Ok(Tx::new(self.inner.begin_ro_txn().map_err(|e| Error::Internal(e.into()))?))
    }

    fn tx_mut(&self) -> Result<Self::TXMut<'_>, Error> {
        Ok(Tx::new(self.inner.begin_rw_txn().map_err(|e| Error::Internal(e.into()))?))
    }
}

impl<E: EnvironmentKind> Env<E> {
    /// Opens the database at the specified path with the given `EnvKind`.
    ///
    /// It does not create the tables, for that call [`create_tables`].
    pub fn open(path: &Path, kind: EnvKind) -> Result<Env<E>, Error> {
        let mode = match kind {
            EnvKind::RO => Mode::ReadOnly,
            EnvKind::RW => Mode::ReadWrite { sync_mode: SyncMode::Durable },
        };

        let env = Env {
            inner: Environment::new()
                .set_max_dbs(TABLES.len())
                .set_geometry(Geometry {
                    size: Some(0..0x100000),     // TODO: reevaluate
                    growth_step: Some(0x100000), // TODO: reevaluate
                    shrink_threshold: None,
                    page_size: Some(PageSize::Set(default_page_size())),
                })
                .set_flags(EnvironmentFlags {
                    mode,
                    no_rdahead: true, // TODO: reevaluate
                    coalesce: true,
                    ..Default::default()
                })
                .open(path)
                .map_err(|e| Error::Internal(e.into()))?,
        };

        Ok(env)
    }

    /// Creates all the defined tables, if necessary.
    pub fn create_tables(&self) -> Result<(), Error> {
        let tx = self.inner.begin_rw_txn().map_err(|e| Error::Initialization(e.into()))?;

        for (table_type, table) in TABLES {
            let flags = match table_type {
                TableType::Table => DatabaseFlags::default(),
                TableType::DupSort => DatabaseFlags::DUP_SORT,
            };

            tx.create_db(Some(table), flags).map_err(|e| Error::Initialization(e.into()))?;
        }

        tx.commit().map_err(|e| Error::Initialization(e.into()))?;

        Ok(())
    }
}

impl<E: EnvironmentKind> Deref for Env<E> {
    type Target = libmdbx::Environment<E>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Collection of database test utilities
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils {
    use super::{Env, EnvKind, EnvironmentKind, Path};

    /// Error during database creation
    pub const ERROR_DB_CREATION: &str = "Not able to create the mdbx file.";
    /// Error during table creation
    pub const ERROR_TABLE_CREATION: &str = "Not able to create tables in the database.";
    /// Error during tempdir creation
    pub const ERROR_TEMPDIR: &str = "Not able to create a temporary directory.";

    /// Create database for testing
    pub fn create_test_db<E: EnvironmentKind>(kind: EnvKind) -> Env<E> {
        create_test_db_with_path(kind, &tempfile::TempDir::new().expect(ERROR_TEMPDIR).into_path())
    }

    /// Create database for testing with specified path
    pub fn create_test_db_with_path<E: EnvironmentKind>(kind: EnvKind, path: &Path) -> Env<E> {
        let env = Env::<E>::open(path, kind).expect(ERROR_DB_CREATION);
        env.create_tables().expect(ERROR_TABLE_CREATION);
        env
    }
}

#[cfg(test)]
mod tests {
    use super::{test_utils, Env, EnvKind};
    use libmdbx::{NoWriteMap, WriteMap};
    use reth_interfaces::db::{
        tables::{Headers, PlainState},
        Database, DbTx, DbTxMut,
    };
    use reth_primitives::{Account, Address, Header, H256, U256};
    use std::str::FromStr;
    use tempfile::TempDir;

    const ERROR_DB_CREATION: &str = "Not able to create the mdbx file.";
    const ERROR_PUT: &str = "Not able to insert value into table.";
    const ERROR_GET: &str = "Not able to get value from table.";
    const ERROR_COMMIT: &str = "Not able to commit transaction.";
    const ERROR_RETURN_VALUE: &str = "Mismatching result.";
    const ERROR_INIT_TX: &str = "Failed to create a MDBX transaction.";
    const ERROR_ETH_ADDRESS: &str = "Invalid address.";

    #[test]
    fn db_creation() {
        test_utils::create_test_db::<NoWriteMap>(EnvKind::RW);
    }

    #[test]
    fn db_manual_put_get() {
        let env = test_utils::create_test_db::<NoWriteMap>(EnvKind::RW);

        let value = Header::default();
        let key = (1u64, H256::zero());

        // PUT
        let tx = env.tx_mut().expect(ERROR_INIT_TX);
        tx.put::<Headers>(key.into(), value.clone()).expect(ERROR_PUT);
        tx.commit().expect(ERROR_COMMIT);

        // GET
        let tx = env.tx().expect(ERROR_INIT_TX);
        let result = tx.get::<Headers>(key.into()).expect(ERROR_GET);
        assert!(result.expect(ERROR_RETURN_VALUE) == value);
        tx.commit().expect(ERROR_COMMIT);
    }

    #[test]
    fn db_closure_put_get() {
        let path = TempDir::new().expect(test_utils::ERROR_TEMPDIR).into_path();

        let value = Account {
            nonce: 18446744073709551615,
            bytecode_hash: H256::random(),
            balance: U256::max_value(),
        };
        let key = Address::from_str("0xa2c122be93b0074270ebee7f6b7292c7deb45047")
            .expect(ERROR_ETH_ADDRESS);

        {
            let env = test_utils::create_test_db_with_path::<WriteMap>(EnvKind::RW, &path);

            // PUT
            let result = env.update(|tx| {
                tx.put::<PlainState>(key, value).expect(ERROR_PUT);
                200
            });
            assert!(result.expect(ERROR_RETURN_VALUE) == 200);
        }

        let env = Env::<WriteMap>::open(&path, EnvKind::RO).expect(ERROR_DB_CREATION);

        // GET
        let result = env.view(|tx| tx.get::<PlainState>(key).expect(ERROR_GET)).expect(ERROR_GET);

        assert!(result == Some(value))
    }
}
