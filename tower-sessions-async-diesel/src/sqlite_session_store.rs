use std::error::Error;
use std::fmt;

use async_trait::async_trait;
use diesel::prelude::*;
use diesel::sqlite;
use diesel_async::{
    pooled_connection::deadpool::{Pool, PoolError},
    sync_connection_wrapper::SyncConnectionWrapper,
    RunQueryDsl,
};
use tower_sessions_core::{
    session::{Id, Record},
    session_store::{self, ExpiredDeletion},
    SessionStore,
};

type SqlitePool = Pool<SyncConnectionWrapper<sqlite::SqliteConnection>>;

// use crate::SqlxStoreError;
/// An error type for SQLx stores.
#[derive(thiserror::Error, Debug)]
pub enum DieselStoreError {
    /// A variant to map `diesel_async` deadpool errors.
    #[error(transparent)]
    Deadpool(#[from] PoolError),

    /// A variant to map `diesel` errors.
    #[error(transparent)]
    Diesel(#[from] diesel::result::Error),

    /// A variant to map `rmp_serde` encode errors.
    #[error(transparent)]
    Encode(#[from] rmp_serde::encode::Error),

    /// A variant to map `rmp_serde` decode errors.
    #[error(transparent)]
    Decode(#[from] rmp_serde::decode::Error),
}

impl From<DieselStoreError> for session_store::Error {
    fn from(err: DieselStoreError) -> Self {
        use DieselStoreError::*;
        match err {
            Deadpool(inner) => session_store::Error::Backend(inner.to_string()),
            Diesel(inner) => session_store::Error::Backend(inner.to_string()),
            Decode(inner) => session_store::Error::Decode(inner.to_string()),
            Encode(inner) => session_store::Error::Encode(inner.to_string()),
        }
    }
}

/// A SQLite session store.
#[derive(Clone)]
pub struct AsyncSqliteStore {
    pool: SqlitePool,
}

// Workaround until `SqliteConnection` and `SyncConnectionWrapper` implement `Debug`.
impl fmt::Debug for AsyncSqliteStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AsyncSqliteStore")
    }
}

diesel::table! {
    __tower_sessions {
        id -> Text,
        data -> Binary,
        expiry_date -> Timestamp,
    }
}

static CREATE_TABLE_SQL: &str = r#"
    CREATE TABLE IF NOT EXISTS __tower_sessions
    (
        id TEXT PRIMARY KEY NOT NULL,
        data BLOB NOT NULL,
        expiry_date TEXT NOT NULL
    )
"#;

impl AsyncSqliteStore {
    /// Create a new SQLite store with the provided connection pool.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use tower_sessions_async_diesel_store::SqliteStore;
    ///
    /// # tokio_test::block_on(async {
    /// let config = AsyncDieselConnectionManager::<SyncConnectionWrapper<SqliteConnection>>::new(":memory:");
    /// let pool: Pool<SyncConnectionWrapper<SqliteConnection>> = Pool::builder(config).build()?;
    /// let session_store = SqliteStore::new(pool);
    /// # })
    /// ```
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Migrate the session schema.
    pub async fn migrate(&self) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
        let mut conn = self.pool.get().await.map_err(DieselStoreError::Deadpool)?;
        diesel::sql_query(CREATE_TABLE_SQL)
            .execute(&mut conn)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ExpiredDeletion for AsyncSqliteStore {
    async fn delete_expired(&self) -> session_store::Result<()> {
        let mut conn = self.pool.get().await.map_err(DieselStoreError::Deadpool)?;
        diesel::delete(
            __tower_sessions::table.filter(__tower_sessions::expiry_date.lt(diesel::dsl::now)),
        )
        .execute(&mut conn)
        .await
        .map_err(DieselStoreError::Diesel)?;
        Ok(())
    }
}

#[async_trait]
impl SessionStore for AsyncSqliteStore {
    // TODO(tibbe): Implement an explicit create(). We would like to use "insert or abort into".

    async fn save(&self, session_record: &Record) -> session_store::Result<()> {
        let mut conn = self.pool.get().await.map_err(DieselStoreError::Deadpool)?;
        let expiry_date = session_record.expiry_date;
        let expiry_date = time::PrimitiveDateTime::new(expiry_date.date(), expiry_date.time());
        let data = rmp_serde::to_vec(session_record).map_err(DieselStoreError::Encode)?;
        diesel::insert_into(__tower_sessions::table)
            .values((
                __tower_sessions::id.eq(session_record.id.to_string()),
                __tower_sessions::data.eq(&data),
                __tower_sessions::expiry_date.eq(expiry_date),
            ))
            .on_conflict(__tower_sessions::id)
            .do_update()
            .set((
                __tower_sessions::data.eq(&data),
                __tower_sessions::expiry_date.eq(expiry_date),
            ))
            .execute(&mut conn)
            .await
            .map_err(DieselStoreError::Diesel)?;

        Ok(())
    }

    async fn load(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        let mut conn = self.pool.get().await.map_err(DieselStoreError::Deadpool)?;
        let res = __tower_sessions::table
            .limit(1)
            .select(__tower_sessions::data)
            .filter(
                __tower_sessions::id
                    .eq(session_id.to_string())
                    .and(__tower_sessions::expiry_date.gt(diesel::dsl::now)),
            )
            .get_result::<Vec<u8>>(&mut conn)
            .await
            .optional()
            .map_err(DieselStoreError::Diesel)?
            .map(|data| rmp_serde::from_slice(&data).map_err(DieselStoreError::Decode))
            .transpose()?;
        Ok(res)
    }

    async fn delete(&self, session_id: &Id) -> session_store::Result<()> {
        let mut conn = self.pool.get().await.map_err(DieselStoreError::Deadpool)?;
        diesel::delete(__tower_sessions::table.find(session_id.to_string()))
            .execute(&mut conn)
            .await
            .map_err(DieselStoreError::Diesel)?;
        Ok(())
    }
}
