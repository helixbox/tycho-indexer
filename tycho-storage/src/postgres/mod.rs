//! # Postgres based storage backend
//!
//! This postgres-based storage backend provides implementations for the
//! traits defined in the storage module.
//!
//! ## Design Decisions
//!
//! ### Representation of Enums as Tables
//!
//! Certain enums such as 'Chain' are modelled as tables in our implementation.
//! This decision stems from an understanding that while extending the Rust
//! codebase to include more enums is a straightforward task, modifying the type
//! of a SQL column can be an intricate process. By representing enums as
//! tables, we circumvent unnecessary migrations when modifying, for example the Chain enum.
//!
//! With this representation, it's important to synchronize them whenever the
//! enums members changed. This can be done automatically once at system
//! startup.
//!
//!
//! Note: A removed enum can be ignored safely even though it might instigate a
//! panic if an associated entity still exists in the database and retrieved
//! with a codebase which no longer presents the enum value.
//!
//! ### Timestamps
//!
//! We use naive timestamps throughout the code as it is assumed that the server
//! that will be running the application will always use UTC as it's local time.
//! Thus all naive timestamps on the application are implcitly in UTC. Be aware
//! that especially tests might run on machines that violate this assumption so
//! in tests make sure to create a timestamp aware timestamp and convert it to
//! UTC before using the naive value.
//!
//! #### Timestamp fields
//!
//! As the are multiple different timestamp columns below is a short summary how
//! these are used:
//!
//! * `inserted` and `modified_ts`: These are pure "book-keeping" values, used to track when the
//!   record was inserted or updated. They are not used in any business logic. These values are
//!   automatically set via Postgres triggers, so they don't need to be manually set.
//!
//! * `valid_from` and `valid_to`: These timestamps enable data versioning aka time-travel
//!   functionality. Hence, these should always be set correctly. `valid_from` must be set to the
//!   timestamp at which the entity was created
//!   - most often that will be the value of the corresponding `block.ts`. Same
//!   applies for `valid_to`. There are triggers in place to automatically set
//!   `valid_to` if you insert a new entity with the same identity (not primary
//!   key). But to delete a record, `valid_to` needs to be manually set as no
//!   automatic trigger exists for deletes yet.
//!
//! * `created_ts`: For entities that are immutable, this timestamp records when the entity was
//!   created and is used for time-travel functionality. For example, for contracts, this timestamp
//!   will be the block timestamp of its deployment.
//!
//! * `deleted_ts`: This serves a similar purpose to `created_ts`, but in reverse. It indicates when
//!   an entity was deleted.
//!
//! * `block.ts`: This is the timestamp attached to the block. Ideally, it should coincide with the
//!   validation/mining start time.
//!
//! ### Versioning
//!
//! This implementation utilizes temporal tables for recording the changes in
//! entities over time. In this model, `valid_from` and `valid_to` determine the
//! timeframe during which the facts provided by the record are regarded as
//! accurate (validity period). Typically, in temporal tables, a valid version
//! for a specific timestamp is found using the following predicate:
//!
//! ```sql
//! valid_from < version_ts AND (version_ts <= valid_to OR valid_to is NULL)
//! ```
//!
//! The `valid_to` can be set to null, signifying that the version remains
//! valid. However, as all alterations within a block happen simultaneously,
//! this predicate might yield multiple valid versions for a single entity.
//!
//! To further assign a temporal sequence to these entities, the transaction
//! index within the block is recorded, usually through a `modify_tx` foreign
//! key.
//!
//! ```sql
//! SELECT * FROM table
//! JOIN transaction
//! WHERE valid_from < version_ts
//!     AND (version_ts <= valid_to OR valid_to is NULL)
//! ORDER BY entity_id, transaction.index DESC
//! DISTINCT ON entity_id
//! ```
//!
//! Here we select a set of versions by timestamp, then arrange rows by their
//! transaction index (descending) and choose the first row, thus obtaining the
//! latest version within the block (aka version at end of block).
//!
//! #### Contract Storage Table
//!
//! Special attention must be given to the contract_storage table, which also
//! records the previous value with each modification. This simplifies the
//! generation of a delta change structure utilized during reorgs for informing
//! clients about the necessary updates. Deletions in this table are modeled
//! as simple updates; in the case of deletion, it's value is updated to null.
//! This technique simplifies querying for delta changes while maintaining
//! efficiency at the cost of requiring additional storage space. As
//! `valid_from` and `valid_to` are not entirely sufficient to find a single
//! valid state within blockchain systems, the contract_storage table
//! additionally maintains an `ordinal` column. This column is redundant with
//! the transaction's index that produced the respective changes. This
//! redundancy is to avoid additional joins and further optimize query
//! performance.
//!
//! ### Reverts
//! If a reorg is observed, we will be asked by the stream to revert to a previous
//! block number. This is handled using the `ON DELETE CASCADE` feature provided by
//! postgres. Each state change is tracked by a creation or modification transaction
//! if the parent transaction is deleted, postgres will delete the corresponding
//! entry in the child table for us.
//! Now all we have to do is to unset valid_to columns that point directly to our
//! last reverted block.
//!
//! ### Atomic Transactions
//!
//! In our design, direct connection to the database and consequently beginning,
//! committing, or rolling back transactions isn't handled within these
//! common-purpose implementations. Rather, each operation receives a connection
//! reference which can either be a simple DB connection, or a DB connection
//! within a transactional context.
//!
//! This approach enables us to chain multiple common-purpose CRUD operations
//! into a single transaction. This guarantees preservation of valid state
//! throughout the application lifetime, even if the process panics during
//! database operations.
use chrono::NaiveDateTime;
use diesel::prelude::*;
use diesel_async::{
    pooled_connection::{deadpool::Pool, AsyncDieselConnectionManager},
    AsyncPgConnection, RunQueryDsl,
};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use std::{collections::HashMap, hash::Hash, i64, ops::Deref, str::FromStr, sync::Arc};
use tracing::{debug, info};
use tycho_core::{
    models::{Chain, TxHash},
    storage::{BlockIdentifier, BlockOrTimestamp, StorageError, Version, VersionKind},
};

pub mod builder;
pub mod cache;
mod chain;
mod contract_state;
mod extraction_state;
mod orm;
mod protocol;
mod schema;
mod versioning;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("./migrations/");

pub(crate) struct ValueIdTableCache<E> {
    map_id: HashMap<E, i64>,
    map_enum: HashMap<i64, E>,
}

/// Provides caching for enum and its database ID relationships.
///
/// Uses a double sided hash map to provide quick lookups in both directions.
impl<E> ValueIdTableCache<E>
where
    E: Eq + Hash + Clone + FromStr + std::fmt::Debug,
    <E as FromStr>::Err: std::fmt::Debug,
{
    /// Creates a new cache from a slice of tuples.
    ///
    /// # Arguments
    ///
    /// * `entries` - A slice of tuples ideally obtained from a database query.
    pub fn from_tuples(entries: Vec<(i64, String)>) -> Self {
        let mut cache = Self { map_id: HashMap::new(), map_enum: HashMap::new() };
        for (id_, name_) in entries {
            let val = E::from_str(&name_).expect("valid enum value");
            cache.map_id.insert(val.clone(), id_);
            cache.map_enum.insert(id_, val);
        }
        cache
    }

    /// Fetches the associated database ID for an enum variant. Panics on cache
    /// miss.
    ///
    /// # Arguments
    ///
    /// * `val` - The enum variant to lookup.
    fn get_id(&self, val: &E) -> i64 {
        *self.map_id.get(val).unwrap_or_else(|| {
            panic!("Unexpected cache miss for enum {:?}, entries: {:?}", val, self.map_id)
        })
    }

    /// Retrieves the corresponding enum variant for a database ID. Panics on
    /// cache miss.
    ///
    /// # Arguments
    ///
    /// * `id` - The database ID to lookup.
    fn get_value(&self, id: &i64) -> E {
        self.map_enum
            .get(id)
            .unwrap_or_else(|| {
                panic!("Unexpected cache miss for id {}, entries: {:?}", id, self.map_enum)
            })
            .to_owned()
    }
}

type ChainEnumCache = ValueIdTableCache<Chain>;
/// ProtocolSystem is not handled as an Enum, because that would require us to restart the whole
/// application every time we want to add another System. Hence, to diverge from the implementation
/// of the Chain enum was a conscious decision.
type ProtocolSystemEnumCache = ValueIdTableCache<String>;

trait FromPool<T> {
    async fn from_pool(pool: Pool<AsyncPgConnection>) -> Result<T, StorageError>;
}

impl FromPool<ChainEnumCache> for ChainEnumCache {
    async fn from_pool(pool: Pool<AsyncPgConnection>) -> Result<ChainEnumCache, StorageError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|err| StorageError::Unexpected(format!("{}", err)))?;

        let results = async {
            use schema::chain::dsl::*;
            chain
                .select((id, name))
                .load(&mut conn)
                .await
                .expect("Failed to load chain ids!")
        }
        .await;
        Ok(Self::from_tuples(results))
    }
}

impl FromPool<ProtocolSystemEnumCache> for ProtocolSystemEnumCache {
    async fn from_pool(
        pool: Pool<AsyncPgConnection>,
    ) -> Result<ProtocolSystemEnumCache, StorageError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|err| StorageError::Unexpected(format!("{}", err)))?;

        let results = async {
            use schema::protocol_system::dsl::*;
            protocol_system
                .select((id, name))
                .load(&mut conn)
                .await
                .expect("Failed to load protocol system ids!")
        }
        .await;
        Ok(Self::from_tuples(results))
    }
}

// Helper type to retrieve entities with their associated tx hashes.
#[derive(Debug)]
struct WithTxHash<T> {
    entity: T,
    tx: Option<TxHash>,
}

impl<T> Deref for WithTxHash<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.entity
    }
}

struct PostgresError(StorageError);

impl From<diesel::result::Error> for PostgresError {
    fn from(value: diesel::result::Error) -> Self {
        PostgresError(StorageError::Unexpected(format!("DieselError: {}", value)))
    }
}

impl From<PostgresError> for StorageError {
    fn from(value: PostgresError) -> Self {
        value.0
    }
}

impl From<StorageError> for PostgresError {
    fn from(value: StorageError) -> Self {
        PostgresError(value)
    }
}

fn storage_error_from_diesel(
    err: diesel::result::Error,
    entity: &str,
    id: &str,
    fetch_args: Option<String>,
) -> PostgresError {
    let err_string = err.to_string();
    match err {
        diesel::result::Error::DatabaseError(
            diesel::result::DatabaseErrorKind::UniqueViolation,
            details,
        ) => {
            if let Some(col) = details.column_name() {
                if col == "id" {
                    return PostgresError(StorageError::DuplicateEntry(
                        entity.to_owned(),
                        id.to_owned(),
                    ));
                }
            }
            PostgresError(StorageError::Unexpected(err_string))
        }
        diesel::result::Error::NotFound => {
            if let Some(related_entitiy) = fetch_args {
                return PostgresError(StorageError::NoRelatedEntity(
                    entity.to_owned(),
                    id.to_owned(),
                    related_entitiy,
                ));
            }
            PostgresError(StorageError::NotFound(entity.to_owned(), id.to_owned()))
        }
        _ => PostgresError(StorageError::Unexpected(err_string)),
    }
}

async fn maybe_lookup_block_ts(
    block: &BlockOrTimestamp,
    conn: &mut AsyncPgConnection,
) -> Result<NaiveDateTime, StorageError> {
    match block {
        BlockOrTimestamp::Block(BlockIdentifier::Hash(h)) => Ok(orm::Block::by_hash(h, conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "Block", &hex::encode(h), None))?
            .ts),
        BlockOrTimestamp::Block(BlockIdentifier::Number((chain, no))) => {
            Ok(orm::Block::by_number(*chain, *no, conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, "Block", &format!("{}", no), None))?
                .ts)
        }
        BlockOrTimestamp::Block(BlockIdentifier::Latest(chain)) => {
            Ok(orm::Block::most_recent(*chain, conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, "Block", "latest", None))?
                .ts)
        }
        BlockOrTimestamp::Timestamp(ts) => Ok(*ts),
    }
}

async fn maybe_lookup_version_ts(
    version: &Version,
    conn: &mut AsyncPgConnection,
) -> Result<NaiveDateTime, StorageError> {
    if !matches!(version.1, VersionKind::Last) {
        return Err(StorageError::Unsupported(format!("Unsupported version kind: {:?}", version.1)));
    }
    maybe_lookup_block_ts(&version.0, conn).await
}

#[derive(Clone)]
pub(crate) struct PostgresGateway {
    protocol_system_id_cache: Arc<ProtocolSystemEnumCache>,
    chain_id_cache: Arc<ChainEnumCache>,
}

impl PostgresGateway {
    pub fn with_cache(
        cache: Arc<ChainEnumCache>,
        protocol_system_cache: Arc<ProtocolSystemEnumCache>,
    ) -> Self {
        Self { protocol_system_id_cache: protocol_system_cache, chain_id_cache: cache }
    }

    #[allow(dead_code)]
    pub async fn from_connection(conn: &mut AsyncPgConnection) -> Self {
        let chain_id_mapping: Vec<(i64, String)> = async {
            use schema::chain::dsl::*;
            chain
                .select((id, name))
                .load(conn)
                .await
                .expect("Failed to load chain ids!")
        }
        .await;

        let protocol_system_id_mapping: Vec<(i64, String)> = async {
            use schema::protocol_system::dsl::*;
            protocol_system
                .select((id, name))
                .load(conn)
                .await
                .expect("Failed to load protocol system!")
        }
        .await;

        let cache = Arc::new(ChainEnumCache::from_tuples(chain_id_mapping));
        let protocol_system_cache =
            Arc::new(ProtocolSystemEnumCache::from_tuples(protocol_system_id_mapping));
        Self::with_cache(cache, protocol_system_cache)
    }

    fn get_chain_id(&self, chain: &Chain) -> i64 {
        self.chain_id_cache.get_id(chain)
    }

    fn get_chain(&self, id: &i64) -> Chain {
        self.chain_id_cache.get_value(id)
    }

    fn get_protocol_system_id(&self, protocol_system: &String) -> i64 {
        self.protocol_system_id_cache
            .get_id(protocol_system)
    }

    #[allow(dead_code)]
    fn get_protocol_system(&self, id: &i64) -> String {
        self.protocol_system_id_cache
            .get_value(id)
    }

    pub async fn new(pool: Pool<AsyncPgConnection>) -> Result<Self, StorageError> {
        let cache = ChainEnumCache::from_pool(pool.clone()).await?;
        let protocol_system_cache: ValueIdTableCache<String> =
            ProtocolSystemEnumCache::from_pool(pool.clone()).await?;
        let gw = PostgresGateway::with_cache(Arc::new(cache), Arc::new(protocol_system_cache));

        Ok(gw)
    }
}

/// Establishes a connection to the database and creates a connection pool.
///
/// This function takes in the URL of the database as an argument and returns a pool
/// of connections that the application can use to interact with the database. If there's
/// any error during the creation of this pool, it is converted into a `StorageError` for
/// uniform error handling across the application.
///
/// # Arguments
///
/// - `db_url`: A string slice that holds the URL of the database to connect to.
///
/// # Returns
///
/// A Result which is either:
///
/// - `Ok`: Contains a `Pool` of `AsyncPgConnection`s if the connection was established
///   successfully.
/// - `Err`: Contains a `StorageError` if there was an issue creating the connection pool.
async fn connect(db_url: &str) -> Result<Pool<AsyncPgConnection>, StorageError> {
    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(db_url);
    let pool = Pool::builder(config)
        .build()
        .map_err(|err| StorageError::Unexpected(format!("{}", err)))?;
    run_migrations(db_url);
    Ok(pool)
}

/// Ensures the `Chain` enum is present in the database, if not it inserts it.
///
/// This function serves as a way to ensure all chains found within the `chains`  
/// slice are present within the database. It does this by inserting each chain into
/// the `chain` table. If a conflict arises during this operation (indicating that
/// the chain already exists in the database), it simply does nothing for that
/// specific operation and moves on.
///
/// It uses a connection from the passed in `Pool<AsyncPgConnection>` asynchronously.
/// In case of any error during these operations, the function will panic with an
/// appropriate error message.
///
///
/// # Arguments
///
/// - `chains`: A slice containing chains which need to be ensured in the database.
/// - `pool`: An instance of `Pool` containing `AsyncPgConnection`s used to interact with the
///   database.
///
/// # Panics
///
/// This function will panic under two circumstances:
///
/// - If it failed to get a connection from the provided pool.
/// - If there was an issue ensuring the presence of chains in the database.
async fn ensure_chains(chains: &[Chain], pool: Pool<AsyncPgConnection>) {
    let mut conn = pool.get().await.expect("connection ok");
    diesel::insert_into(schema::chain::table)
        .values(
            chains
                .iter()
                .map(|c| schema::chain::name.eq(c.to_string()))
                .collect::<Vec<_>>(),
        )
        .on_conflict_do_nothing()
        .execute(&mut conn)
        .await
        .expect("chains ensured");
    debug!("Ensured chain enum presence for: {:?}", chains);
}

async fn ensure_protocol_systems(protocol_systems: &[String], pool: Pool<AsyncPgConnection>) {
    let mut conn = pool.get().await.expect("connection ok");

    diesel::insert_into(schema::protocol_system::table)
        .values(
            protocol_systems
                .iter()
                .map(|ps| schema::protocol_system::name.eq(ps))
                .collect::<Vec<_>>(),
        )
        .on_conflict_do_nothing()
        .execute(&mut conn)
        .await
        .expect("Could not ensure protocol system enum's in database");

    debug!("Ensured protocol system enum presence for: {:?}", protocol_systems);
}

fn run_migrations(db_url: &str) {
    info!("Upgrading database...");
    let mut conn = PgConnection::establish(db_url).expect("Connection to database should succeed");
    conn.run_pending_migrations(MIGRATIONS)
        .expect("migrations should execute without errors");
}

// TODO: add cfg(test) once we have better mocks to be used in indexer crate
pub mod testing {
    //! # Reusable components to write tests against the DB.
    use diesel::sql_query;
    use diesel_async::{
        pooled_connection::{deadpool::Pool, AsyncDieselConnectionManager},
        AsyncPgConnection, RunQueryDsl,
    };
    use std::future::Future;

    async fn setup_pool() -> Pool<AsyncPgConnection> {
        let database_url =
            std::env::var("DATABASE_URL").expect("Database URL must be set for testing");
        let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(database_url);
        Pool::builder(config).build().unwrap()
    }

    async fn teardown(conn: &mut AsyncPgConnection) {
        let tables = vec![
            // put block early so most FKs cascade, it would
            // be better to find the correct order tough.
            "block",
            "protocol_calls_contract",
            "contract_storage",
            "contract_code",
            "account_balance",
            "protocol_component_holds_token",
            "protocol_component_holds_contract",
            "component_balance",
            "token",
            "account",
            "protocol_state",
            "protocol_component",
            "extraction_state",
            "protocol_type",
            "protocol_system",
            "transaction",
            "chain",
            "audit_log",
        ];
        for t in tables.iter() {
            sql_query(format!("DELETE FROM {};", t))
                .execute(conn)
                .await
                .unwrap_or_else(|_| panic!("Error truncating {} table", t));
        }
        dbg!("Teardown completed");
    }

    /// Run tests that require committing data to the db.
    ///
    /// This function will run tests that are expected to commit data into the database, e.g.
    /// because the test setups are too complex for using `begin_test_transaction`. Please only use
    /// this as a last resort as these tests are slow and have to be run serially. Using a test
    /// transaction is preferred where possible.  
    ///
    /// The method will pass a connection pool to the actual test function, catch any panics and
    /// then purge all data in the tables so that the next test can run from a clean slate.
    ///
    /// ## Interference with other tests
    /// While this function runs, the db will actually contain data.
    ///
    /// This is likely to interfere with other tests using this same function. To mitigate this, the
    /// test name or the package should contain the string `serial_db`, this way nextest will
    /// automatically put these test into a separate group.
    /// Other tests that rely on a empty db (most tests unsing test_transactions) will likely
    /// be affected if run in parrallel with tests using this function. CI will automatically
    /// partition the serial and parallel tests into two separate groups.
    ///
    /// ## Example
    /// ```
    /// use tycho_indexer::storage::postgres::testing::run_against_db;
    ///
    /// #[tokio::test]
    /// async fn test_serial_db_mytest_name() {
    ///     run_against_db(|connection_pool| async move {
    ///         println!("here goes actual test code")
    ///     }).await;
    /// }
    /// ```
    ///
    /// ## Future
    /// We should consider moving these test to their own database. That would require running
    /// migrations on these databases though. For now tests run fast enough though.
    pub async fn run_against_db<F, Fut>(test_f: F)
    where
        F: FnOnce(Pool<AsyncPgConnection>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let connection_pool = setup_pool().await;
        let inner_pool = connection_pool.clone();
        let res = tokio::spawn(async move {
            test_f(inner_pool).await;
        })
        .await;

        let mut connection = connection_pool
            .get()
            .await
            .expect("Failed to get a connection from the pool");

        teardown(&mut connection).await;
        res.unwrap();
    }
}

// TODO: add cfg(test) once we have better mocks to be used in indexer crate
pub mod db_fixtures {
    //! # General Purpose Fixtures for Database State Modification
    //!
    //! The module contains fixtures that are designed to alter the database state
    //! for testing purposes.
    //!
    //! This module doesn't rely on any locally specific code from the Postgres
    //! packages, except for the autogenerated `schema` module. Given that `schema`
    //! is generated by examining our table schema, it's reasonable to assert that
    //! this module belongs to the `schema` and not the package itself.
    //!
    //! A key goal of these fixtures is to prevent reliance on application code when
    //! setting up test data, thereby avoiding cyclical dependencies. For example,
    //! if you're modifying how an entity is inserted, and this change affects the
    //! data setup for other tests, these tests would start failing – a situation we
    //! want to avoid. This could lead to complex, hard-to-resolve issues,
    //! particularly if you're using the insertion method to validate that a second
    //! insertion fails, while simultaneously working on the insertion method. In
    //! such cases, running your tests becomes impossible if the insertion method
    //! encounters bugs.
    //!
    //! # Heads Up
    //! We advise adding only general-purpose methods to this module, such as those
    //! for adding or removing a single row/entry, or maximum entries along with
    //! their child entities. More intricate setups should be localized where they
    //! are explicitly used.
    //!
    //! If you need to share more complex setups and decide to include them here,
    //! please think through whether this is the suitable location, or whether a
    //! local copy might serve your needs better. For instance, if the complete
    //! shared setup isn't necessary for your test case, copy it and keep only
    //! the entries that are crucial to your test case.
    use chrono::NaiveDateTime;
    use diesel::{prelude::*, sql_query};
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use ethers::types::{H160, H256, U256};
    use serde_json::Value;
    use std::str::FromStr;
    use tycho_core::{
        models::{Balance, Code, FinancialType, ImplementationType},
        Bytes,
    };

    use super::schema;
    use crate::postgres::orm;

    // Insert a new chain
    pub async fn insert_chain(conn: &mut AsyncPgConnection, name: &str) -> i64 {
        diesel::insert_into(schema::chain::table)
            .values(schema::chain::name.eq(name))
            .returning(schema::chain::id)
            .get_result(conn)
            .await
            .unwrap()
    }

    /// Inserts two sequential blocks
    pub async fn insert_blocks(conn: &mut AsyncPgConnection, chain_id: i64) -> Vec<i64> {
        let block_records = vec![
            (
                schema::block::hash.eq(Vec::from(
                    H256::from_str(
                        "0x88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6",
                    )
                    .unwrap()
                    .as_bytes(),
                )),
                schema::block::parent_hash.eq(Vec::from(
                    H256::from_str(
                        "0xd4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3",
                    )
                    .unwrap()
                    .as_bytes(),
                )),
                schema::block::number.eq(1),
                schema::block::ts.eq("2020-01-01T00:00:00"
                    .parse::<chrono::NaiveDateTime>()
                    .expect("timestamp")),
                schema::block::chain_id.eq(chain_id),
            ),
            (
                schema::block::hash.eq(Vec::from(
                    H256::from_str(
                        "0xb495a1d7e6663152ae92708da4843337b958146015a2802f4193a410044698c9",
                    )
                    .unwrap()
                    .as_bytes(),
                )),
                schema::block::parent_hash.eq(Vec::from(
                    H256::from_str(
                        "0x88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6",
                    )
                    .unwrap()
                    .as_bytes(),
                )),
                schema::block::number.eq(2),
                schema::block::ts.eq("2020-01-01T01:00:00"
                    .parse::<chrono::NaiveDateTime>()
                    .unwrap()),
                schema::block::chain_id.eq(chain_id),
            ),
        ];
        diesel::insert_into(schema::block::table)
            .values(&block_records)
            .returning(schema::block::id)
            .get_results(conn)
            .await
            .unwrap()
    }

    /// Insert a bunch of transactions using (block_id, index, hash)
    pub async fn insert_txns(conn: &mut AsyncPgConnection, txns: &[(i64, i64, &str)]) -> Vec<i64> {
        let from_val = H160::from_str("0x4648451b5F87FF8F0F7D622bD40574bb97E25980").unwrap();
        let to_val = H160::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap();
        let data: Vec<_> = txns
            .iter()
            .map(|(b, i, h)| {
                use schema::transaction::dsl::*;
                (
                    block_id.eq(b),
                    index.eq(i),
                    hash.eq(H256::from_str(h)
                        .expect("valid txhash")
                        .as_bytes()
                        .to_owned()),
                    from.eq(from_val.as_bytes()),
                    to.eq(to_val.as_bytes()),
                )
            })
            .collect();
        diesel::insert_into(schema::transaction::table)
            .values(&data)
            .returning(schema::transaction::id)
            .get_results(conn)
            .await
            .unwrap()
    }

    pub async fn insert_account(
        conn: &mut AsyncPgConnection,
        address: &str,
        title: &str,
        chain_id: i64,
        tx_id: Option<i64>,
    ) -> i64 {
        let ts: Option<NaiveDateTime> = if let Some(id) = tx_id {
            Some(
                schema::transaction::table
                    .inner_join(schema::block::table)
                    .filter(schema::transaction::id.eq(id))
                    .select(schema::block::ts)
                    .first::<NaiveDateTime>(conn)
                    .await
                    .expect("setup tx id not found"),
            )
        } else {
            None
        };

        let query = diesel::insert_into(schema::account::table).values((
            schema::account::title.eq(title),
            schema::account::chain_id.eq(chain_id),
            schema::account::creation_tx.eq(tx_id),
            schema::account::created_at.eq(ts),
            schema::account::address.eq(hex::decode(address).unwrap()),
        ));
        query
            .returning(schema::account::id)
            .get_result(conn)
            .await
            .unwrap()
    }

    pub async fn insert_slots(
        conn: &mut AsyncPgConnection,
        contract_id: i64,
        modify_tx: i64,
        valid_from: &str,
        valid_to: Option<&str>,
        slots: &[(u64, u64, Option<u64>)],
    ) -> Vec<i64> {
        let ts = valid_from
            .parse::<chrono::NaiveDateTime>()
            .unwrap();
        let end_ts = valid_to.map(|s| {
            s.parse::<chrono::NaiveDateTime>()
                .unwrap()
        });
        let data = slots
            .iter()
            .enumerate()
            .map(|(idx, (k, v, pv))| {
                let previous_value =
                    pv.map(|pv| hex::decode(format!("{:064x}", U256::from(pv))).unwrap());
                (
                    schema::contract_storage::slot.eq(hex::decode(format!(
                        "{:064x}",
                        U256::from(*k)
                    ))
                    .unwrap()),
                    schema::contract_storage::value.eq(hex::decode(format!(
                        "{:064x}",
                        U256::from(*v)
                    ))
                    .unwrap()),
                    schema::contract_storage::previous_value.eq(previous_value),
                    schema::contract_storage::account_id.eq(contract_id),
                    schema::contract_storage::modify_tx.eq(modify_tx),
                    schema::contract_storage::valid_from.eq(ts),
                    schema::contract_storage::valid_to.eq(end_ts),
                    schema::contract_storage::ordinal.eq(idx as i64),
                )
            })
            .collect::<Vec<_>>();

        diesel::insert_into(schema::contract_storage::table)
            .values(&data)
            .returning(schema::contract_storage::id)
            .get_results(conn)
            .await
            .unwrap()
    }

    pub async fn insert_account_balance(
        conn: &mut AsyncPgConnection,
        new_balance: u64,
        tx_id: i64,
        valid_to: Option<&str>,
        account: i64,
    ) {
        let ts = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq(tx_id))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .expect("setup tx id not found");
        let end_ts = valid_to.map(|s| {
            s.parse::<chrono::NaiveDateTime>()
                .unwrap()
        });
        let mut b0 = [0; 32];
        U256::from(new_balance).to_big_endian(&mut b0);
        {
            use schema::account_balance::dsl::*;
            diesel::insert_into(account_balance)
                .values((
                    account_id.eq(account),
                    balance.eq(b0.as_slice()),
                    modify_tx.eq(tx_id),
                    valid_from.eq(ts),
                    valid_to.eq(end_ts),
                ))
                .execute(conn)
                .await
                .expect("balance insert ok");
        }
    }

    pub async fn insert_contract_code(
        conn: &mut AsyncPgConnection,
        account_id: i64,
        modify_tx: i64,
        code: Code,
    ) -> i64 {
        let ts = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq(modify_tx))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .expect("setup tx id not found");

        let code_hash = H256::from_slice(&ethers::utils::keccak256(&code));
        let data = (
            schema::contract_code::code.eq(code),
            schema::contract_code::hash.eq(code_hash.as_bytes()),
            schema::contract_code::account_id.eq(account_id),
            schema::contract_code::modify_tx.eq(modify_tx),
            schema::contract_code::valid_from.eq(ts),
        );

        diesel::insert_into(schema::contract_code::table)
            .values(data)
            .returning(schema::contract_code::id)
            .get_result(conn)
            .await
            .unwrap()
    }

    pub async fn delete_account(conn: &mut AsyncPgConnection, target_id: i64, ts: &str) {
        let ts = ts
            .parse::<NaiveDateTime>()
            .expect("timestamp valid");
        {
            use schema::account::dsl::*;
            diesel::update(account.filter(id.eq(target_id)))
                .set(deleted_at.eq(ts))
                .execute(conn)
                .await
                .expect("delete account table ok");
        }
        {
            use schema::account_balance::dsl::*;
            diesel::update(account_balance.filter(account_id.eq(target_id)))
                .set(valid_to.eq(ts))
                .execute(conn)
                .await
                .expect("delete balance table ok");
        }
        {
            use schema::contract_code::dsl::*;
            diesel::update(contract_code.filter(account_id.eq(target_id)))
                .set(valid_to.eq(ts))
                .execute(conn)
                .await
                .expect("delete code table ok");
        }
        {
            use schema::contract_storage::dsl::*;
            diesel::update(contract_storage.filter(account_id.eq(target_id)))
                .set(valid_to.eq(ts))
                .execute(conn)
                .await
                .expect("delete storage table ok");
        }
    }

    // Insert a new Component Balance
    pub async fn insert_component_balance(
        conn: &mut AsyncPgConnection,
        balance: Balance,
        previous_balance: Balance,
        balance_float: f64,
        token_id: i64,
        tx_id: i64,
        protocol_component_id: i64,
    ) {
        let ts: NaiveDateTime = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq(tx_id))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .expect("setup tx id not found");
        diesel::insert_into(schema::component_balance::table)
            .values((
                schema::component_balance::protocol_component_id.eq(protocol_component_id),
                schema::component_balance::token_id.eq(token_id),
                schema::component_balance::modify_tx.eq(tx_id),
                schema::component_balance::new_balance.eq(balance),
                schema::component_balance::balance_float.eq(balance_float),
                schema::component_balance::previous_value.eq(previous_balance),
                schema::component_balance::valid_from.eq(ts),
            ))
            .execute(conn)
            .await
            .expect("component balance insert failed");
    }

    // Insert a new Protocol System
    pub async fn insert_protocol_system(conn: &mut AsyncPgConnection, name: String) -> i64 {
        diesel::insert_into(schema::protocol_system::table)
            .values(schema::protocol_system::name.eq(name))
            .returning(schema::protocol_system::id)
            .get_result(conn)
            .await
            .unwrap()
    }

    // Insert a new Protocol Type
    pub async fn insert_protocol_type(
        conn: &mut AsyncPgConnection,
        name: &str,
        financial_type: Option<FinancialType>,
        attribute: Option<Value>,
        implementation_type: Option<ImplementationType>,
    ) -> i64 {
        let financial_type: orm::FinancialType = financial_type
            .unwrap_or(FinancialType::Swap)
            .into();
        let implementation_type: orm::ImplementationType = implementation_type
            .unwrap_or(ImplementationType::Custom)
            .into();
        let query = diesel::insert_into(schema::protocol_type::table).values((
            schema::protocol_type::name.eq(name),
            schema::protocol_type::financial_type.eq(financial_type),
            schema::protocol_type::attribute_schema.eq(attribute),
            schema::protocol_type::implementation.eq(implementation_type),
        ));
        query
            .returning(schema::protocol_type::id)
            .get_result(conn)
            .await
            .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    // Insert a new Protocol Component
    pub async fn insert_protocol_component(
        conn: &mut AsyncPgConnection,
        id: &str,
        chain_id: i64,
        system_id: i64,
        type_id: i64,
        tx_id: i64,
        token_ids: Option<Vec<i64>>,
        contract_code_ids: Option<Vec<i64>>,
    ) -> i64 {
        let ts: NaiveDateTime = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq(tx_id))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .expect("setup tx id not found");

        let query = diesel::insert_into(schema::protocol_component::table).values((
            schema::protocol_component::external_id.eq(id),
            schema::protocol_component::chain_id.eq(chain_id),
            schema::protocol_component::protocol_type_id.eq(type_id),
            schema::protocol_component::protocol_system_id.eq(system_id),
            schema::protocol_component::creation_tx.eq(tx_id),
            schema::protocol_component::created_at.eq(ts),
        ));
        let component_id = query
            .returning(schema::protocol_component::id)
            .get_result(conn)
            .await
            .unwrap();

        if let Some(t_ids) = token_ids {
            diesel::insert_into(schema::protocol_component_holds_token::table)
                .values(
                    t_ids
                        .iter()
                        .map(|t_id| {
                            (
                                schema::protocol_component_holds_token::protocol_component_id
                                    .eq(component_id),
                                schema::protocol_component_holds_token::token_id.eq(t_id),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
                .execute(conn)
                .await
                .expect("protocol component holds token insert ok");
        }

        if let Some(cc_ids) = contract_code_ids {
            diesel::insert_into(schema::protocol_component_holds_contract::table)
                .values(
                    cc_ids
                        .iter()
                        .map(|cc_id| {
                            (
                                schema::protocol_component_holds_contract::protocol_component_id
                                    .eq(component_id),
                                schema::protocol_component_holds_contract::contract_code_id
                                    .eq(cc_id),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
                .execute(conn)
                .await
                .expect("protocol component holds contract code insert ok");
        }
        component_id
    }

    // Insert a new Protocol State
    pub async fn insert_protocol_state(
        conn: &mut AsyncPgConnection,
        component_id: i64,
        tx_id: i64,
        attribute_name: String,
        attribute_value: Bytes,
        previous_value: Option<Bytes>,
        valid_to_tx: Option<i64>,
    ) {
        let ts: NaiveDateTime = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq(tx_id))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .expect("setup tx id not found");
        let valid_to_ts: Option<NaiveDateTime> = match &valid_to_tx {
            Some(tx) => Some(
                schema::transaction::table
                    .inner_join(schema::block::table)
                    .filter(schema::transaction::id.eq(tx))
                    .select(schema::block::ts)
                    .first::<NaiveDateTime>(conn)
                    .await
                    .expect("setup tx id not found"),
            ),
            None => None,
        };

        let query = diesel::insert_into(schema::protocol_state::table).values((
            schema::protocol_state::protocol_component_id.eq(component_id),
            schema::protocol_state::modify_tx.eq(tx_id),
            schema::protocol_state::modified_ts.eq(ts),
            schema::protocol_state::valid_from.eq(ts),
            schema::protocol_state::valid_to.eq(valid_to_ts),
            schema::protocol_state::attribute_name.eq(attribute_name),
            schema::protocol_state::attribute_value.eq(attribute_value),
            schema::protocol_state::previous_value.eq(previous_value),
        ));
        query
            .execute(conn)
            .await
            .expect("protocol state insert ok");
    }

    pub async fn insert_token(
        conn: &mut AsyncPgConnection,
        chain_id: i64,
        address: &str,
        symbol: &str,
        decimals: i32,
    ) -> (i64, i64) {
        let title = &format!("token_{}", symbol);
        let account_id = insert_account(conn, address, title, chain_id, None).await;

        let query = diesel::insert_into(schema::token::table).values((
            schema::token::account_id.eq(account_id),
            schema::token::symbol.eq(symbol),
            schema::token::decimals.eq(decimals),
            schema::token::tax.eq(10),
            schema::token::gas.eq(vec![10]),
        ));
        (
            account_id,
            query
                .returning(schema::token::id)
                .get_result(conn)
                .await
                .unwrap(),
        )
    }

    pub async fn get_token_by_symbol(conn: &mut AsyncPgConnection, symbol: String) -> orm::Token {
        schema::token::table
            .filter(schema::token::symbol.eq(symbol.clone()))
            .select(schema::token::all_columns)
            .first::<orm::Token>(conn)
            .await
            .unwrap()
    }

    pub async fn insert_token_prices(data: &[(i64, f64)], conn: &mut AsyncPgConnection) {
        diesel::insert_into(schema::token_price::table)
            .values(
                data.iter()
                    .map(|(tid, price)| {
                        (
                            schema::token_price::token_id.eq(tid),
                            schema::token_price::price.eq(price),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .execute(conn)
            .await
            .expect("Inserting token prices fixture failed");
    }

    pub async fn calculate_component_tvl(conn: &mut AsyncPgConnection) {
        sql_query(
            r#"
        INSERT INTO component_tvl (protocol_component_id, tvl)
        SELECT 
            bal.protocol_component_id as protocol_component_id,
            SUM(bal.balance_float * token_price.price / POWER(10.0, token.decimals)) as tvl
        FROM 
            component_balance AS bal 
        INNER JOIN 
            token_price ON bal.token_id = token_price.token_id 
        INNER JOIN
            token ON bal.token_id = token.id
        WHERE 
            bal.valid_to IS NULL 
        GROUP BY 
            bal.protocol_component_id
        ON CONFLICT (protocol_component_id) 
        DO UPDATE SET 
            tvl = EXCLUDED.tvl;
        "#,
        )
        .execute(conn)
        .await
        .expect("calculating fixture component tvl failed");
    }
}