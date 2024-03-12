use super::{
    schema::{
        account, account_balance, block, chain, component_balance, component_tvl, contract_code,
        contract_storage, extraction_state, protocol_component, protocol_component_holds_contract,
        protocol_component_holds_token, protocol_state, protocol_system, protocol_type, token,
        transaction,
    },
    versioning::{DeltaVersionedRow, StoredDeltaVersionedRow, StoredVersionedRow, VersionedRow},
    PostgresError,
};
use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel::{
    dsl::sql,
    pg::Pg,
    prelude::*,
    query_builder::{BoxedSqlQuery, SqlQuery},
    sql_query,
    sql_types::{self, BigInt, Bool, Bytea, Double},
};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use diesel_derive_enum::DbEnum;
use std::collections::{HashMap, HashSet};
use tycho_core::{
    models,
    models::{
        Address, AttrStoreKey, Balance, BlockHash, Code, CodeHash, ComponentId, ContractId,
        StoreVal, TxHash,
    },
    storage::{BlockIdentifier, StorageError},
    Bytes,
};

#[derive(Identifiable, Queryable, Selectable)]
#[diesel(table_name = chain)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Chain {
    pub id: i64,
    pub name: String,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

/// Represents the state of an extractor.
///
/// Note that static extraction parameters are usually defined through
/// infrastructure configuration tools (e.g., terraform). This struct only
/// maintains dynamic state that changes during runtime and has to be persisted
/// between restarts.
#[derive(Identifiable, Queryable, Associations, Selectable)]
#[diesel(belongs_to(Chain))]
#[diesel(table_name = extraction_state)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ExtractionState {
    /// Unique identifier for the extraction state.
    pub id: i64,

    /// Name of the extractor.
    pub name: String,

    // Version of the extractor.
    pub version: String,

    /// Chain identifier that the extractor instance is scoped to.
    pub chain_id: i64,

    /// Last fully extracted cursor for the corresponding substream.
    /// Can be null, indicating no cursor has been extracted yet.
    pub cursor: Option<Vec<u8>>,

    /// Additional attributes that the extractor needs to persist.
    /// Stored as a JSON binary object.
    pub attributes: Option<serde_json::Value>,

    /// Timestamp when this entry was inserted into the table.
    pub inserted_ts: NaiveDateTime,

    /// Timestamp when this entry was last modified.
    pub modified_ts: NaiveDateTime,
}

impl ExtractionState {
    /// Retrieves an `ExtractionState` based on the provided extractor name and chain ID.
    ///
    /// This method performs a join operation with the `chain` table and filters the results
    /// based on the given extractor name and chain ID. It then selects the matching
    /// `ExtractionState` and fetches the first result.
    ///
    /// # Parameters
    /// - `extractor`: The name of the extractor to filter by.
    /// - `chain_id`: The ID of the chain to filter by.
    /// - `conn`: A mutable reference to an asynchronous PostgreSQL connection.
    ///
    /// # Returns
    /// - `Ok(Some(ExtractionState))` if a matching `ExtractionState` is found.
    /// - `Ok(None)` if no matching entry is found in the database.
    /// - `Err(DieselError)` if a Diesel error occurs during the query.
    pub async fn by_name(
        extractor: &str,
        chain_id: i64,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Option<ExtractionState>> {
        extraction_state::table
            .inner_join(chain::table)
            .filter(extraction_state::name.eq(extractor))
            .filter(chain::id.eq(chain_id))
            .select(ExtractionState::as_select())
            .first::<ExtractionState>(conn)
            .await
            .optional()
    }
}

#[derive(Insertable)]
#[diesel(table_name = extraction_state)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewExtractionState<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub chain_id: i64,
    pub cursor: Option<&'a [u8]>,
    pub attributes: Option<&'a serde_json::Value>,
    pub modified_ts: NaiveDateTime,
}

#[derive(AsChangeset, Debug)]
#[diesel(table_name = extraction_state)]
pub struct ExtractionStateForm<'a> {
    pub cursor: Option<&'a [u8]>,
    pub attributes: Option<&'a serde_json::Value>,
    pub modified_ts: Option<NaiveDateTime>,
}

#[derive(Identifiable, Queryable, Associations, Selectable)]
#[diesel(belongs_to(Chain))]
#[diesel(table_name = block)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Block {
    pub id: i64,
    pub hash: BlockHash,
    pub parent_hash: BlockHash,
    pub chain_id: i64,
    pub main: bool,
    pub number: i64,
    pub ts: NaiveDateTime,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl Block {
    pub async fn by_number(
        chain: models::Chain,
        number: i64,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Block> {
        block::table
            .inner_join(chain::table)
            .filter(block::number.eq(number))
            .filter(chain::name.eq(chain.to_string()))
            .select(Block::as_select())
            .first::<Block>(conn)
            .await
    }

    pub async fn by_hash(block_hash: &[u8], conn: &mut AsyncPgConnection) -> QueryResult<Block> {
        block::table
            .filter(block::hash.eq(block_hash))
            .select(Block::as_select())
            .first::<Block>(conn)
            .await
    }

    pub async fn most_recent(
        chain: models::Chain,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Block> {
        block::table
            .inner_join(chain::table)
            .filter(chain::name.eq(chain.to_string()))
            .order(block::number.desc())
            .select(Block::as_select())
            .first::<Block>(conn)
            .await
    }

    pub async fn by_id(id: &BlockIdentifier, conn: &mut AsyncPgConnection) -> QueryResult<Block> {
        match id {
            BlockIdentifier::Hash(hash) => Self::by_hash(hash, conn).await,
            BlockIdentifier::Number((chain, number)) => {
                Self::by_number(*chain, *number, conn).await
            }
            BlockIdentifier::Latest(chain) => Self::most_recent(*chain, conn).await,
        }
    }
}

#[derive(Insertable)]
#[diesel(table_name = block)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewBlock {
    pub hash: BlockHash,
    pub parent_hash: BlockHash,
    pub chain_id: i64,
    pub main: bool,
    pub number: i64,
    pub ts: NaiveDateTime,
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug)]
#[diesel(belongs_to(Block))]
#[diesel(table_name = transaction)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Transaction {
    pub id: i64,
    pub hash: TxHash,
    pub block_id: i64,
    pub from: Address,
    pub to: Address,
    pub index: i64,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl Transaction {
    pub async fn by_hash(hash: &[u8], conn: &mut AsyncPgConnection) -> QueryResult<Self> {
        transaction::table
            .filter(transaction::hash.eq(hash))
            .select(Self::as_select())
            .first::<Self>(conn)
            .await
    }

    pub async fn ids_by_hash(
        hashes: &[TxHash],
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<TxHash, i64>, StorageError> {
        use super::schema::transaction::dsl::*;

        let results = transaction
            .filter(hash.eq_any(hashes))
            .select((hash, id))
            .load::<(TxHash, i64)>(conn)
            .await
            .map_err(PostgresError::from)?;

        Ok(results.into_iter().collect())
    }

    // fetches the transaction id, hash, index and block timestamp for a given set of hashes
    pub async fn ids_and_ts_by_hash(
        hashes: &[&TxHash],
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(i64, Bytes, i64, NaiveDateTime)>> {
        transaction::table
            .inner_join(block::table)
            .filter(transaction::hash.eq_any(hashes))
            .select((transaction::id, transaction::hash, transaction::index, block::ts))
            .get_results::<(i64, Bytes, i64, NaiveDateTime)>(conn)
            .await
    }
}

#[derive(Insertable)]
#[diesel(table_name = transaction)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewTransaction {
    pub hash: TxHash,
    pub block_id: i64,
    pub from: Address,
    pub to: Address,
    pub index: i64,
}

#[derive(Identifiable, Queryable, Selectable)]
#[diesel(table_name = protocol_system)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolSystem {
    pub id: i64,
    pub name: String,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Insertable, Debug)]
#[diesel(table_name=protocol_system)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolSystem {
    pub name: String,
}

#[derive(Debug, DbEnum, Clone, PartialEq)]
#[ExistingTypePath = "crate::postgres::schema::sql_types::FinancialType"]
pub enum FinancialType {
    Swap,
    Psm,
    Debt,
    Leverage,
}

impl From<models::FinancialType> for FinancialType {
    fn from(value: models::FinancialType) -> Self {
        match value {
            models::FinancialType::Swap => Self::Swap,
            models::FinancialType::Psm => Self::Psm,
            models::FinancialType::Debt => Self::Debt,
            models::FinancialType::Leverage => Self::Leverage,
        }
    }
}

#[derive(Debug, DbEnum, Clone, PartialEq)]
#[ExistingTypePath = "crate::postgres::schema::sql_types::ImplementationType"]
pub enum ImplementationType {
    Custom,
    Vm,
}

impl From<models::ImplementationType> for ImplementationType {
    fn from(value: models::ImplementationType) -> Self {
        match value {
            models::ImplementationType::Vm => Self::Vm,
            models::ImplementationType::Custom => Self::Custom,
        }
    }
}

#[derive(Identifiable, Queryable, Selectable)]
#[diesel(table_name = protocol_type)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolType {
    pub id: i64,
    pub name: String,
    pub financial_type: FinancialType,
    pub attribute_schema: Option<serde_json::Value>,
    pub implementation: ImplementationType,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Identifiable, Queryable, Selectable)]
#[diesel(table_name = component_balance)]
#[diesel(belongs_to(ProtocolComponent))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ComponentBalance {
    pub id: i64,
    pub token_id: i64,
    pub new_balance: Balance,
    pub balance_float: f64,
    pub previous_value: Balance,
    pub modify_tx: i64,
    pub protocol_component_id: i64,
    pub inserted_ts: NaiveDateTime,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
}
impl ComponentBalance {
    pub fn update_many(
        new_balances_values: &HashMap<i64, (Bytes, Bytes, f64)>,
    ) -> BoxedSqlQuery<'_, Pg, SqlQuery> {
        let bind_params = (1..=new_balances_values.len() * 4)
            .collect::<Vec<_>>()
            .chunks(4)
            .map(|chunk| format!("(${}, ${}, ${}, ${})", chunk[0], chunk[1], chunk[2], chunk[3])) // Format each chunk as a tuple
            .collect::<Vec<_>>()
            .join(", ");
        let query_tmpl = format!(
            r#"
            UPDATE component_balance
            SET 
                new_balance = new_values.new_balance,
                previous_value = new_values.previous_value,
                balance_float = new_values.balance_float
            FROM (VALUES
                {}
            ) AS new_values(id, new_balance, previous_value, balance_float)
            WHERE component_balance.id = new_values.id;
            "#,
            bind_params
        );
        let mut q = sql_query(query_tmpl).into_boxed();
        for (k, (b, pb, bf)) in new_balances_values.iter() {
            q = q.bind::<BigInt, _>(*k);
            q = q.bind::<Bytea, _>(b);
            q = q.bind::<Bytea, _>(pb);
            q = q.bind::<Double, _>(bf);
        }
        q
    }
}

#[async_trait]
impl StoredVersionedRow for ComponentBalance {
    type EntityId = (i64, i64);
    type PrimaryKey = i64;
    type Version = NaiveDateTime;

    fn get_pk(&self) -> Self::PrimaryKey {
        self.id
    }

    fn get_entity_id(&self) -> Self::EntityId {
        (self.protocol_component_id, self.token_id)
    }

    // Clippy false positive
    #[allow(clippy::mutable_key_type)]
    async fn latest_versions_by_ids<I: IntoIterator<Item = Self::EntityId> + Send + Sync>(
        ids: I,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Box<Self>>, StorageError> {
        let (component_ids, token_ids): (Vec<_>, Vec<_>) = ids.into_iter().unzip();

        let tuple_ids = component_ids
            .iter()
            .zip(token_ids.iter())
            .collect::<HashSet<_>>();

        Ok(component_balance::table
            .select(ComponentBalance::as_select())
            .into_boxed()
            .filter(
                component_balance::protocol_component_id
                    .eq_any(&component_ids)
                    .and(component_balance::token_id.eq_any(&token_ids))
                    .and(component_balance::valid_to.is_null()),
            )
            .get_results(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .filter(|cs| tuple_ids.contains(&(&cs.protocol_component_id, &cs.token_id)))
            .map(Box::new)
            .collect())
    }

    fn table_name() -> &'static str {
        "component_balance"
    }
}

#[derive(AsChangeset, Insertable, Debug)]
#[diesel(table_name = component_balance)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewComponentBalance {
    pub token_id: i64,
    pub new_balance: Balance,
    pub previous_value: Balance,
    pub balance_float: f64,
    pub modify_tx: i64,
    pub protocol_component_id: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
}

impl NewComponentBalance {
    pub fn new(
        token_id: i64,
        new_balance: Balance,
        balance_float: f64,
        previous_value: Option<Balance>,
        modify_tx: i64,
        protocol_component_id: i64,
        valid_from: NaiveDateTime,
    ) -> Self {
        Self {
            token_id,
            new_balance,
            previous_value: previous_value.unwrap_or_else(|| Bytes::from("0x00")),
            balance_float,
            modify_tx,
            protocol_component_id,
            valid_from,
            valid_to: None,
        }
    }
}

impl VersionedRow for NewComponentBalance {
    type SortKey = (i64, i64, NaiveDateTime);
    type EntityId = (i64, i64);
    type Version = NaiveDateTime;

    fn get_entity_id(&self) -> Self::EntityId {
        (self.protocol_component_id, self.token_id)
    }

    fn get_sort_key(&self) -> Self::SortKey {
        (self.protocol_component_id, self.token_id, self.valid_from)
    }

    fn set_valid_to(&mut self, end_version: Self::Version) {
        self.valid_to = Some(end_version);
    }

    fn get_valid_from(&self) -> Self::Version {
        self.valid_from
    }
}

impl DeltaVersionedRow for NewComponentBalance {
    type Value = Balance;

    fn get_value(&self) -> Self::Value {
        self.new_balance.clone()
    }

    fn set_previous_value(&mut self, previous_value: Self::Value) {
        self.previous_value = previous_value
    }
}

impl DeltaVersionedRow for ComponentBalance {
    type Value = Balance;

    fn get_value(&self) -> Self::Value {
        self.new_balance.clone()
    }

    fn set_previous_value(&mut self, previous_value: Self::Value) {
        self.previous_value = previous_value
    }
}

impl StoredDeltaVersionedRow for ComponentBalance {
    type Value = Balance;

    fn get_value(&self) -> Self::Value {
        self.new_balance.clone()
    }
}

#[derive(AsChangeset, Insertable)]
#[diesel(table_name = protocol_type)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolType {
    pub name: String,
    pub financial_type: FinancialType,
    pub attribute_schema: Option<serde_json::Value>,
    pub implementation: ImplementationType,
}
impl ProtocolType {
    pub async fn id_by_name(name: &String, conn: &mut AsyncPgConnection) -> QueryResult<i64> {
        protocol_type::table
            .filter(protocol_type::name.eq(name))
            .select(protocol_type::id)
            .first::<i64>(conn)
            .await
    }
}
#[derive(Identifiable, Queryable, Associations, Selectable, Clone, Debug, PartialEq)]
#[diesel(belongs_to(Chain))]
#[diesel(belongs_to(ProtocolType))]
#[diesel(belongs_to(ProtocolSystem))]
#[diesel(belongs_to(Transaction, foreign_key = creation_tx))]
#[diesel(table_name = protocol_component)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolComponent {
    pub id: i64,
    pub chain_id: i64,
    pub external_id: String,
    pub protocol_type_id: i64,
    pub protocol_system_id: i64,
    pub attributes: Option<serde_json::Value>,
    pub created_at: NaiveDateTime,
    pub deleted_at: Option<NaiveDateTime>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
    pub creation_tx: i64,
    pub deletion_tx: Option<i64>,
}

#[derive(Insertable, AsChangeset, Debug)]
#[diesel(belongs_to(Chain))]
#[diesel(belongs_to(ProtocolType))]
#[diesel(belongs_to(ProtocolSystem))]
#[diesel(table_name = protocol_component)]
pub struct NewProtocolComponent {
    pub external_id: String,
    pub chain_id: i64,
    pub protocol_type_id: i64,
    pub protocol_system_id: i64,
    pub creation_tx: i64,
    pub created_at: NaiveDateTime,
    pub attributes: Option<serde_json::Value>,
}

impl NewProtocolComponent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        external_id: &str,
        chain_id: i64,
        protocol_type_id: i64,
        protocol_system_id: i64,
        creation_tx: i64,
        created_at: NaiveDateTime,
        attributes: &HashMap<String, Bytes>,
    ) -> Self {
        let attributes =
            (!attributes.is_empty()).then(|| serde_json::to_value(attributes).unwrap());
        Self {
            external_id: external_id.to_string(),
            chain_id,
            protocol_type_id,
            protocol_system_id,
            creation_tx,
            created_at,
            attributes,
        }
    }
}

impl ProtocolComponent {
    pub async fn ids_by_external_ids(
        external_ids: &[&str],
        chain_db_id: i64,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(i64, String)>> {
        protocol_component::table
            .filter(protocol_component::external_id.eq_any(external_ids))
            .filter(protocol_component::chain_id.eq(chain_db_id))
            .select((protocol_component::id, protocol_component::external_id))
            .get_results::<(i64, String)>(conn)
            .await
    }
}

#[derive(Insertable)]
#[diesel(table_name = protocol_component_holds_contract)]
pub struct NewProtocolComponentHoldsContract {
    pub protocol_component_id: i64,
    pub contract_code_id: i64,
}

#[derive(Identifiable, Queryable, Associations, Selectable, Clone, Debug)]
#[diesel(belongs_to(ProtocolComponent))]
#[diesel(table_name = protocol_state)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolState {
    pub id: i64,
    pub protocol_component_id: i64,
    pub attribute_name: String,
    pub attribute_value: Bytes,
    pub previous_value: Option<Bytes>,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl ProtocolState {
    /// Used to fetch the full state of a component at a given version, filtered by component ids.
    ///
    /// Retrieves all matching protocol states, filtered by component id, along with their linked
    /// component ids and transaction hashes. If no version is provided, the latest state is
    /// returned. The results are grouped by component id and ordered by transaction to allow for
    /// easy state reconstruction. It can be trusted that all state updates for a given component
    /// are together and the updates can be applied in order with the latest update last.
    pub async fn by_id(
        component_ids: &[&str],
        chain_id: i64,
        version_ts: Option<NaiveDateTime>,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(Self, ComponentId, TxHash)>> {
        let mut query = protocol_state::table
            .inner_join(protocol_component::table)
            .inner_join(transaction::table.on(transaction::id.eq(protocol_state::modify_tx)))
            .filter(protocol_component::external_id.eq_any(component_ids))
            .filter(protocol_component::chain_id.eq(chain_id))
            .filter(
                protocol_state::valid_to
                    .gt(version_ts) // if version_ts is None, diesel equates this expression to "False"
                    .or(protocol_state::valid_to.is_null()),
            )
            .into_boxed();

        // if a version timestamp is provided, we want to filter by valid_from <= version_ts
        if let Some(ts) = version_ts {
            query = query.filter(protocol_state::valid_from.le(ts));
        }

        query
            .order_by((protocol_component::external_id, transaction::block_id, transaction::index))
            .select((Self::as_select(), protocol_component::external_id, transaction::hash))
            .get_results::<(Self, String, Bytes)>(conn)
            .await
    }

    /// Used to fetch the full state of a component at a given version, filtered by protocol system.
    ///
    /// Retrieves all matching protocol states, filtered by protocol system, along with their linked
    /// component ids and transaction hashes. If no version is provided, the latest state is
    /// returned. The results are grouped by component id and ordered by transaction to allow for
    /// easy state reconstruction. It can be trusted that all state updates for a given component
    /// are together and the updates can be applied in order with the latest update last.
    ///
    /// Note - follows the same logic as by_ids, but filters by protocol system instead of component
    /// ids.
    pub async fn by_protocol_system(
        system: String,
        chain_id: i64,
        version_ts: Option<NaiveDateTime>,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(Self, ComponentId, TxHash)>> {
        let mut query = protocol_state::table
            .inner_join(protocol_component::table)
            .inner_join(
                protocol_system::table
                    .on(protocol_component::protocol_system_id.eq(protocol_system::id)),
            )
            .inner_join(transaction::table.on(transaction::id.eq(protocol_state::modify_tx)))
            .filter(protocol_system::name.eq(system.to_string()))
            .filter(protocol_component::chain_id.eq(chain_id))
            .filter(
                protocol_state::valid_to
                    .gt(version_ts)
                    .or(protocol_state::valid_to.is_null()),
            )
            .into_boxed();

        // if a version timestamp is provided, we want to filter by valid_from <= version_ts
        if let Some(ts) = version_ts {
            query = query.filter(protocol_state::valid_from.le(ts));
        }

        query
            .order_by((
                protocol_state::protocol_component_id,
                transaction::block_id,
                transaction::index,
            ))
            .select((Self::as_select(), protocol_component::external_id, transaction::hash))
            .get_results::<(Self, String, Bytes)>(conn)
            .await
    }

    /// Used to fetch the full state of a component at a given version, filtered by chain.
    ///
    /// Retrieves all matching protocol states, filtered by chain, along with their linked
    /// component ids and transaction hashes. If no version is provided, the latest state is
    /// returned. The results are grouped by component id and ordered by transaction to allow for
    /// easy state reconstruction. It can be trusted that all state updates for a given component
    /// are together and the updates can be applied in order with the latest update last.
    ///
    /// Note - follows the same logic as by_ids, but filters by chain instead of component ids.
    pub async fn by_chain(
        chain_id: i64,
        version_ts: Option<NaiveDateTime>,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(Self, ComponentId, TxHash)>> {
        let mut query = protocol_state::table
            .inner_join(protocol_component::table)
            .inner_join(transaction::table.on(transaction::id.eq(protocol_state::modify_tx)))
            .filter(protocol_component::chain_id.eq(chain_id))
            .filter(
                protocol_state::valid_to
                    .gt(version_ts)
                    .or(protocol_state::valid_to.is_null()),
            )
            .into_boxed();

        // if a version timestamp is provided, we want to filter by valid_from <= version_ts
        if let Some(ts) = version_ts {
            query = query.filter(protocol_state::valid_from.le(ts));
        }

        query
            .order_by((
                protocol_state::protocol_component_id,
                transaction::block_id,
                transaction::index,
            ))
            .select((Self::as_select(), protocol_component::external_id, transaction::hash))
            .get_results::<(Self, String, Bytes)>(conn)
            .await
    }

    /// Used to fetch all protocol state changes within the given timeframe.
    ///
    /// Retrieves all state updates applied after start_ts and still valid by end_ts, filtered by
    /// chain. Please note - this function is intended to be used to fetch forward changes/deltas.
    /// The results are grouped by component id and ordered by transaction to allow for easy state
    /// reconstruction. It can be trusted that all state updates for a given component are together
    /// and the updates can be applied in order with the latest update last.
    pub async fn forward_deltas_by_chain(
        chain_id: i64,
        start_ts: NaiveDateTime,
        end_ts: NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(Self, ComponentId, TxHash)>> {
        protocol_state::table
            .inner_join(protocol_component::table)
            .inner_join(transaction::table.on(transaction::id.eq(protocol_state::modify_tx)))
            .filter(protocol_component::chain_id.eq(chain_id))
            .filter(
                protocol_state::valid_to
                    .gt(end_ts)
                    .or(protocol_state::valid_to.is_null()),
            )
            .filter(protocol_state::valid_from.le(end_ts))
            .filter(protocol_state::valid_from.gt(start_ts))
            .order_by((
                protocol_state::protocol_component_id,
                transaction::block_id,
                transaction::index,
            ))
            .select((Self::as_select(), protocol_component::external_id, transaction::hash))
            .get_results::<(Self, String, Bytes)>(conn)
            .await
    }

    /// Used to detect attributes that were deleted within a given timeframe.
    ///
    /// Retrieves all component-attribute pairs that have a valid version at start_ts and have no
    /// valid version at end_ts. The results are grouped by component id and ordered by
    /// transaction to allow for easy state reconstruction. It can be trusted that all state updates
    /// for a given component are together and the updates can be applied in order with the latest
    /// update last.
    pub async fn deleted_attributes_by_chain(
        chain_id: i64,
        start_ts: NaiveDateTime,
        end_ts: NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(ComponentId, AttrStoreKey)>> {
        // subquery to exclude entities that have a valid version at end_ts (haven't been deleted)
        // TODO: use parameter binding instead of string interpolation
        let sub_query = format!("NOT EXISTS (
                                SELECT 1 FROM protocol_state ps2
                                WHERE ps2.protocol_component_id = protocol_state.protocol_component_id
                                AND ps2.attribute_name = protocol_state.attribute_name
                                AND ps2.valid_from <= '{}'
                                AND (ps2.valid_to > '{}' OR ps2.valid_to IS NULL)
                            )", end_ts, end_ts);

        // query for all state updates that have a valid_to between start_ts and end_ts (potentially
        // have been deleted) and filter it by the subquery for not deleted attributes.
        // i.e. potentially_deleted - not_deleted = deleted
        protocol_state::table
            .inner_join(protocol_component::table)
            .inner_join(transaction::table.on(transaction::id.eq(protocol_state::modify_tx)))
            .filter(protocol_component::chain_id.eq(chain_id))
            // validity ends during the timeframe (potentially deleted)
            .filter(protocol_state::valid_to.le(end_ts))
            .filter(protocol_state::valid_to.gt(start_ts))
            // subquery to remove those that weren't deleted (valid version exists at end_ts)
            .filter(sql::<Bool>(&sub_query))
            .order_by((
                protocol_state::protocol_component_id,
                transaction::block_id,
                transaction::index,
            ))
            .select((protocol_component::external_id, protocol_state::attribute_name))
            .get_results::<(String, String)>(conn)
            .await
    }

    /// Used to retrieve the original state of all component attributes that were updated within the
    /// given timeframe.
    ///
    /// Retrieves the previous values (reverse deltas) of all state updates that were applied after
    /// target_ts and before start_ts, filtered by chain. Please note - this function is intended to
    /// be used to fetch beckwards changes/revert deltas. Start_ts represents the more recent ts
    /// and target_ts represents the ts of the block to be reverted to. The results are grouped by
    /// component id to allow for easy state reconstruction.
    pub async fn reverse_delta_by_chain(
        chain_id: i64,
        start_ts: NaiveDateTime,
        target_ts: NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(ComponentId, AttrStoreKey, Option<StoreVal>)>> {
        let query = protocol_state::table
            .inner_join(protocol_component::table)
            .inner_join(transaction::table.on(transaction::id.eq(protocol_state::modify_tx)))
            .filter(protocol_component::chain_id.eq(chain_id));

        // We query all states that were added between the start and target timestamps, filtered by
        // chain. We group it by component and attribute and order it by tx. Then we deduplicate by
        // taking the first row per group. This gives us the first state update for each
        // component-attribute pair. Finally, we return the component id, attribute name and
        // previous value for each component-attribute pair. Note, previous values are null for
        // state updates where they are the first update of that attribute (attribute
        // creation).
        let reverted_query = query
            .filter(protocol_state::valid_from.gt(target_ts))
            .filter(protocol_state::valid_from.le(start_ts))
            .order_by((
                protocol_state::protocol_component_id,
                protocol_state::attribute_name,
                transaction::block_id,
                transaction::index,
            ))
            .select(
                sql::<(sql_types::Text, sql_types::Text, sql_types::Nullable<sql_types::Bytea>)>(
                    "external_id, attribute_name, previous_value AS value",
                ),
            )
            .distinct_on((protocol_state::protocol_component_id, protocol_state::attribute_name));

        // subquery to exclude entities that have a valid version at start_ts (weren't deleted)
        // TODO: use parameter binding instead of string interpolation
        let sub_query = format!("NOT EXISTS (
                                SELECT 1 FROM protocol_state ps2
                                WHERE ps2.protocol_component_id = protocol_state.protocol_component_id
                                AND ps2.attribute_name = protocol_state.attribute_name
                                AND ps2.valid_from <= '{}'
                                AND (ps2.valid_to > '{}' OR ps2.valid_to IS NULL)
                            )", start_ts, start_ts);

        // We query all states that were deleted between the start and target timestamps. Deleted
        // states need to be reinstated so we return the component id, attribute name and latest
        // value of each component-attribute pair here.
        let deleted_query = query
            // validity ends during the timeframe (potentially deleted)
            .filter(protocol_state::valid_to.le(start_ts))
            .filter(protocol_state::valid_to.gt(target_ts))
            // validity starts before the timeframe (is valid at target_ts)
            .filter(protocol_state::valid_from.le(target_ts))
            // subquery to remove those that weren't deleted (valid version exists at start_ts)
            .filter(sql::<Bool>(&sub_query))
            .order_by((
                protocol_state::protocol_component_id,
                protocol_state::attribute_name,
                transaction::block_id,
                transaction::index,
            ))
            .select(
                sql::<(sql_types::Text, sql_types::Text, sql_types::Nullable<sql_types::Bytea>)>(
                    "external_id, attribute_name, attribute_value AS value",
                ),
            );

        // query and merge results for both reverted updates and deleted states
        reverted_query
            .union_all(deleted_query)
            .get_results::<(String, String, Option<Bytes>)>(conn)
            .await
    }
}

#[async_trait]
impl StoredVersionedRow for ProtocolState {
    type EntityId = (i64, String);
    type PrimaryKey = i64;
    type Version = NaiveDateTime;

    fn get_pk(&self) -> Self::PrimaryKey {
        self.id
    }

    fn get_entity_id(&self) -> Self::EntityId {
        (self.protocol_component_id, self.attribute_name.clone())
    }

    // Clippy false positive
    #[allow(clippy::mutable_key_type)]
    async fn latest_versions_by_ids<I: IntoIterator<Item = Self::EntityId> + Send + Sync>(
        ids: I,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Box<Self>>, StorageError> {
        let (pc_id, attr_name): (Vec<_>, Vec<_>) = ids.into_iter().unzip();
        let tuple_ids = pc_id
            .iter()
            .zip(attr_name.iter())
            .collect::<HashSet<_>>();
        Ok(protocol_state::table
            .select(ProtocolState::as_select())
            .into_boxed()
            .filter(
                protocol_state::protocol_component_id
                    .eq_any(&pc_id)
                    .and(protocol_state::attribute_name.eq_any(&attr_name))
                    .and(protocol_state::valid_to.is_null()),
            )
            .get_results(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .filter(|cs| tuple_ids.contains(&(&cs.protocol_component_id, &cs.attribute_name)))
            .map(Box::new)
            .collect())
    }

    fn table_name() -> &'static str {
        "protocol_state"
    }
}

impl StoredDeltaVersionedRow for ProtocolState {
    type Value = Bytes;

    fn get_value(&self) -> Self::Value {
        self.attribute_value.clone()
    }
}

#[derive(Insertable, Clone, Debug)]
#[diesel(table_name = protocol_state)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolState {
    pub protocol_component_id: i64,
    pub attribute_name: Option<String>,
    pub attribute_value: Option<Bytes>,
    pub previous_value: Option<Bytes>,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
}

impl NewProtocolState {
    pub fn new(
        protocol_component_id: i64,
        attribute_name: &str,
        attribute_value: Option<&Bytes>,
        modify_tx: i64,
        valid_from: NaiveDateTime,
    ) -> Self {
        Self {
            protocol_component_id,
            attribute_name: Some(attribute_name.to_string()),
            attribute_value: attribute_value.cloned(),
            previous_value: None,
            modify_tx,
            valid_from,
            valid_to: None,
        }
    }
}

impl VersionedRow for NewProtocolState {
    type SortKey = (i64, String, NaiveDateTime, i64);
    type EntityId = (i64, String);
    type Version = NaiveDateTime;

    fn get_entity_id(&self) -> Self::EntityId {
        (
            self.protocol_component_id,
            self.attribute_name
                .clone()
                .expect("should have attribute name"),
        )
    }

    fn get_sort_key(&self) -> Self::SortKey {
        (
            self.protocol_component_id,
            self.attribute_name.clone().unwrap(),
            self.valid_from,
            self.modify_tx,
        )
    }

    fn set_valid_to(&mut self, end_version: Self::Version) {
        self.valid_to = Some(end_version);
    }

    fn get_valid_from(&self) -> Self::Version {
        self.valid_from
    }
}

impl DeltaVersionedRow for NewProtocolState {
    type Value = Bytes;

    fn get_value(&self) -> Self::Value {
        self.attribute_value
            .clone()
            .expect("should have attribute value")
    }

    fn set_previous_value(&mut self, previous_value: Self::Value) {
        self.previous_value = Some(previous_value)
    }
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug, PartialEq)]
#[diesel(belongs_to(Chain))]
#[diesel(belongs_to(Transaction, foreign_key = creation_tx))]
#[diesel(table_name = account)]
#[diesel(check_for_backend(diesel::pg::Pg))]
/// Represents an account on a blockchain.
///
/// An `Account` is identified by its blockchain (`Chain`) and address (`H160`). It may have a
/// descriptive `title` and contains information about storage slots, balance, associated code
/// (bytecode), code hash, and transaction hashes related to balance modification, code
/// modification, and optional creation. Additional information about accounts.
/// - A Contract is also an Account, but an Account is not necessarily a Contract.
/// - An account is considered a contract if it has associated code.
pub struct Account {
    pub id: i64,
    pub title: String,
    pub address: Address,
    pub chain_id: i64,
    pub creation_tx: Option<i64>,
    pub created_at: Option<NaiveDateTime>,
    pub deleted_at: Option<NaiveDateTime>,
    pub deletion_tx: Option<i64>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl Account {
    pub async fn by_id(
        account_id: &ContractId,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Account> {
        account::table
            .inner_join(chain::table)
            .filter(account::address.eq(&account_id.address))
            .filter(chain::name.eq(account_id.chain.to_string()))
            .select(Account::as_select())
            .first::<Account>(conn)
            .await
    }

    /// retrieves a account by address
    pub async fn by_address(
        address: &[u8],
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<Self>> {
        account::table
            .filter(account::address.eq(address))
            .select(Self::as_select())
            .get_results::<Self>(conn)
            .await
    }

    pub async fn get_addresses_by_id(
        ids: impl Iterator<Item = &i64>,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(i64, Address)>> {
        account::table
            .filter(account::id.eq_any(ids))
            .select((account::id, account::address))
            .get_results::<(i64, Address)>(conn)
            .await
    }
}

#[derive(AsChangeset, Insertable, Debug)]
#[diesel(table_name = account)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewAccount<'a> {
    pub title: &'a str,
    pub address: &'a [u8],
    pub chain_id: i64,
    pub creation_tx: Option<i64>,
    pub created_at: Option<NaiveDateTime>,
    pub deleted_at: Option<NaiveDateTime>,
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug, PartialEq)]
#[diesel(belongs_to(Account))]
#[diesel(table_name = token)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Token {
    pub id: i64,
    pub account_id: i64,
    pub symbol: String,
    pub decimals: i32,
    pub tax: i64,
    pub gas: Vec<Option<i64>>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
    pub quality: i32,
}

#[derive(AsChangeset, Insertable, Debug)]
#[diesel(table_name = token)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewToken {
    pub account_id: i64,
    pub symbol: String,
    pub decimals: i32,
    pub tax: i64,
    pub gas: Vec<Option<i64>>,
    pub quality: i32,
}

impl NewToken {
    pub fn from_token(account_id: i64, token: &models::token::CurrencyToken) -> Self {
        Self {
            account_id,
            symbol: token.symbol.clone(),
            decimals: token.decimals as i32,
            tax: token.tax as i64,
            gas: token
                .gas
                .iter()
                .map(|g| g.map(|u| u as i64))
                .collect(),
            quality: token.quality as i32,
        }
    }
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug)]
#[diesel(belongs_to(Account))]
#[diesel(table_name = account_balance)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct AccountBalance {
    pub id: i64,
    pub balance: Balance,
    pub account_id: i64,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl AccountBalance {
    /// retrieves all balances from a certain account
    pub async fn all_versions(
        address: &Address,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<Self>> {
        account_balance::table
            .inner_join(account::table)
            .filter(account::address.eq(address))
            .select(Self::as_select())
            .get_results::<Self>(conn)
            .await
    }
}

#[async_trait]
impl StoredVersionedRow for AccountBalance {
    type EntityId = i64;
    type PrimaryKey = i64;
    type Version = NaiveDateTime;

    fn get_pk(&self) -> Self::PrimaryKey {
        self.id
    }

    fn get_entity_id(&self) -> Self::EntityId {
        self.account_id
    }

    async fn latest_versions_by_ids<I: IntoIterator<Item = Self::EntityId> + Send + Sync>(
        ids: I,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Box<Self>>, StorageError> {
        Ok(account_balance::table
            .filter(
                account_balance::account_id
                    .eq_any(ids)
                    .and(account_balance::valid_to.is_null()),
            )
            .select(Self::as_select())
            .get_results::<Self>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .map(Box::new)
            .collect())
    }

    fn table_name() -> &'static str {
        "account_balance"
    }
}

#[derive(Insertable, Debug)]
#[diesel(table_name = account_balance)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewAccountBalance {
    pub balance: Balance,
    pub account_id: i64,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
}

impl VersionedRow for NewAccountBalance {
    type SortKey = (i64, NaiveDateTime, i64);
    type EntityId = i64;
    type Version = NaiveDateTime;

    fn get_entity_id(&self) -> Self::EntityId {
        self.account_id
    }

    fn get_sort_key(&self) -> Self::SortKey {
        (self.account_id, self.valid_from, self.modify_tx)
    }

    fn set_valid_to(&mut self, end_version: Self::Version) {
        self.valid_to = Some(end_version);
    }

    fn get_valid_from(&self) -> Self::Version {
        self.valid_from
    }
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug)]
#[diesel(belongs_to(Account))]
#[diesel(table_name = contract_code)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ContractCode {
    pub id: i64,
    pub code: Code,
    pub hash: CodeHash,
    pub account_id: i64,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl ContractCode {
    /// retrieves all codes from a certain account
    pub async fn all_versions(
        address: &[u8],
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<Self>> {
        contract_code::table
            .inner_join(account::table)
            .filter(account::address.eq(address))
            .select(Self::as_select())
            .get_results::<Self>(conn)
            .await
    }
}

#[async_trait]
impl StoredVersionedRow for ContractCode {
    type EntityId = i64;
    type PrimaryKey = i64;
    type Version = NaiveDateTime;

    fn get_pk(&self) -> Self::PrimaryKey {
        self.id
    }

    fn get_entity_id(&self) -> Self::EntityId {
        self.account_id
    }

    async fn latest_versions_by_ids<I: IntoIterator<Item = Self::EntityId> + Send + Sync>(
        ids: I,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Box<Self>>, StorageError> {
        Ok(contract_code::table
            .filter(
                contract_code::account_id
                    .eq_any(ids)
                    .and(contract_code::valid_to.is_null()),
            )
            .select(Self::as_select())
            .get_results::<Self>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .map(Box::new)
            .collect())
    }

    fn table_name() -> &'static str {
        "contract_code"
    }
}

#[derive(Insertable, Debug)]
#[diesel(table_name = contract_code)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewContractCode<'a> {
    pub code: &'a Code,
    pub hash: CodeHash,
    pub account_id: i64,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
}

impl<'a> VersionedRow for NewContractCode<'a> {
    type SortKey = (i64, NaiveDateTime, i64);
    type EntityId = i64;
    type Version = NaiveDateTime;

    fn get_entity_id(&self) -> Self::EntityId {
        self.account_id
    }

    fn get_sort_key(&self) -> Self::SortKey {
        (self.account_id, self.valid_from, self.modify_tx)
    }

    fn set_valid_to(&mut self, end_version: Self::Version) {
        self.valid_to = Some(end_version);
    }

    fn get_valid_from(&self) -> Self::Version {
        self.valid_from
    }
}

// theoretically this struct could also simply reference the original struct.
// Unfortunately that really doesn't play nicely with async_trait on the Gateway
// and makes the types a lot more complicted. Once the system is up and running
// this could be improved though.
pub struct NewContract {
    pub title: String,
    pub address: Address,
    pub chain_id: i64,
    pub creation_tx: Option<i64>,
    pub created_at: Option<NaiveDateTime>,
    pub deleted_at: Option<NaiveDateTime>,
    pub balance: Balance,
    pub code: Code,
    pub code_hash: CodeHash,
}

impl NewContract {
    pub fn new_account(&self) -> NewAccount {
        NewAccount {
            title: &self.title,
            address: &self.address,
            chain_id: self.chain_id,
            creation_tx: self.creation_tx,
            created_at: self.created_at,
            deleted_at: None,
        }
    }
    pub fn new_balance(
        &self,
        account_id: i64,
        modify_tx: i64,
        modify_ts: NaiveDateTime,
    ) -> NewAccountBalance {
        NewAccountBalance {
            balance: self.balance.clone(),
            account_id,
            modify_tx,
            valid_from: modify_ts,
            valid_to: None,
        }
    }
    pub fn new_code(
        &self,
        account_id: i64,
        modify_tx: i64,
        modify_ts: NaiveDateTime,
    ) -> NewContractCode {
        NewContractCode {
            code: &self.code,
            hash: self.code_hash.clone(),
            account_id,
            modify_tx,
            valid_from: modify_ts,
            valid_to: None,
        }
    }
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug)]
#[diesel(belongs_to(Account))]
#[diesel(table_name = contract_storage)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ContractStorage {
    pub id: i64,
    pub slot: Bytes,
    pub value: Option<Bytes>,
    pub previous_value: Option<Bytes>,
    pub account_id: i64,
    pub modify_tx: i64,
    pub ordinal: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[async_trait]
impl StoredVersionedRow for ContractStorage {
    type EntityId = (i64, Bytes);
    type PrimaryKey = i64;
    type Version = NaiveDateTime;

    fn get_pk(&self) -> Self::PrimaryKey {
        self.id
    }

    fn get_entity_id(&self) -> Self::EntityId {
        (self.account_id, self.slot.clone())
    }

    // Clippy false positive
    #[allow(clippy::mutable_key_type)]
    async fn latest_versions_by_ids<I: IntoIterator<Item = Self::EntityId> + Send + Sync>(
        ids: I,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Box<Self>>, StorageError> {
        let (accounts, slots): (Vec<_>, Vec<_>) = ids.into_iter().unzip();
        let tuple_ids = accounts
            .iter()
            .zip(slots.iter())
            .collect::<HashSet<_>>();
        Ok(contract_storage::table
            .select(ContractStorage::as_select())
            .into_boxed()
            .filter(
                contract_storage::account_id
                    .eq_any(&accounts)
                    .and(contract_storage::slot.eq_any(&slots))
                    .and(contract_storage::valid_to.is_null()),
            )
            .get_results(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .filter(|cs| tuple_ids.contains(&(&cs.account_id, &cs.slot)))
            .map(Box::new)
            .collect())
    }

    fn table_name() -> &'static str {
        "contract_storage"
    }
}

impl DeltaVersionedRow for ContractStorage {
    type Value = Option<Bytes>;

    fn get_value(&self) -> Self::Value {
        self.value.clone()
    }

    fn set_previous_value(&mut self, previous_value: Self::Value) {
        self.previous_value = previous_value
    }
}

impl StoredDeltaVersionedRow for ContractStorage {
    type Value = Option<Bytes>;

    fn get_value(&self) -> Self::Value {
        self.value.clone()
    }
}

#[derive(Insertable, Debug)]
#[diesel(table_name = contract_storage)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewSlot<'a> {
    pub slot: &'a Bytes,
    pub value: Option<Bytes>,
    pub previous_value: Option<Bytes>,
    pub account_id: i64,
    pub modify_tx: i64,
    pub ordinal: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
}

impl<'a> VersionedRow for NewSlot<'a> {
    type EntityId = (i64, Bytes);
    type SortKey = ((i64, Bytes), NaiveDateTime, i64);
    type Version = NaiveDateTime;

    fn get_entity_id(&self) -> Self::EntityId {
        (self.account_id, self.slot.clone())
    }

    fn get_sort_key(&self) -> Self::SortKey {
        (self.get_entity_id(), self.valid_from, self.ordinal)
    }

    fn set_valid_to(&mut self, end_version: Self::Version) {
        self.valid_to = Some(end_version);
    }

    fn get_valid_from(&self) -> Self::Version {
        self.valid_from
    }
}

impl<'a> DeltaVersionedRow for NewSlot<'a> {
    type Value = Option<Bytes>;

    fn get_value(&self) -> Self::Value {
        self.value.clone()
    }

    fn set_previous_value(&mut self, previous_value: Self::Value) {
        self.previous_value = previous_value;
    }
}

#[derive(Identifiable, Queryable, Associations, Selectable)]
#[diesel(primary_key(protocol_component_id, token_id))]
#[diesel(belongs_to(ProtocolComponent))]
#[diesel(belongs_to(Token))]
#[diesel(table_name = protocol_component_holds_token)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolHoldsToken {
    protocol_component_id: i64,
    token_id: i64,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Insertable)]
#[diesel(table_name = protocol_component_holds_token)]
pub struct NewProtocolComponentHoldsToken {
    pub protocol_component_id: i64,
    pub token_id: i64,
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug)]
#[diesel(belongs_to(ProtocolComponent))]
#[diesel(table_name = component_tvl)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ComponentTVL {
    id: i64,
    protocol_component_id: i64,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl ComponentTVL {
    pub fn upsert_many(new_tvl_values: &HashMap<i64, f64>) -> BoxedSqlQuery<'static, Pg, SqlQuery> {
        // Generate bind parameter 2-tuples the result will look like '($1, $2), ($3, $4), ...'
        // These are later subsituted with the primary key and valid to values.
        let bind_params = (1..=new_tvl_values.len() * 2)
            .map(|i| if i % 2 == 0 { format!("${}", i) } else { format!("(${}", i) })
            .collect::<Vec<String>>()
            .chunks(2)
            .map(|chunk| chunk.join(", ") + ")")
            .collect::<Vec<String>>()
            .join(", ");
        let query_tmpl = format!(
            r#"
            INSERT INTO component_tvl (protocol_component_id, tvl)
            VALUES {}
            ON CONFLICT (protocol_component_id) 
            DO UPDATE SET tvl = EXCLUDED.tvl;
            "#,
            bind_params
        );
        let mut q = sql_query(query_tmpl).into_boxed();
        for (k, v) in new_tvl_values.iter() {
            q = q.bind::<BigInt, _>(*k);
            q = q.bind::<Double, _>(*v);
        }
        q
    }
}
