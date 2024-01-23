#![allow(unused_variables)]

use std::{cmp::Ordering, collections::HashMap};

use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use ethers::types::H256;
use tracing::warn;

use crate::{
    extractor::evm::{utils::TryDecode, ProtocolComponent, ProtocolState, ProtocolStateDelta},
    hex_bytes::Bytes,
    models::{Chain, ProtocolType},
    storage::{
        postgres::{orm, schema, PostgresGateway},
        Address, BlockIdentifier, BlockOrTimestamp, ChangeType, ContractDelta, ProtocolGateway,
        StorableBlock, StorableContract, StorableProtocolComponent, StorableProtocolState,
        StorableProtocolStateDelta, StorableProtocolType, StorableToken, StorableTransaction,
        StorageError, TxHash, Version,
    },
};

// Private methods
impl<B, TX, A, D, T> PostgresGateway<B, TX, A, D, T>
where
    B: StorableBlock<orm::Block, orm::NewBlock, i64>,
    TX: StorableTransaction<orm::Transaction, orm::NewTransaction, i64>,
    D: ContractDelta + From<A>,
    A: StorableContract<orm::Contract, orm::NewContract, i64>,
    T: StorableToken<orm::Token, orm::NewToken, i64>,
{
    /// # Decoding ProtocolStates from database results.
    ///
    /// This function takes as input the database result for querying protocol states and their
    /// linked component id and transaction hash.
    ///
    /// ## Assumptions:
    /// - It is assumed that the rows in the result are ordered by:
    ///     1. Component ID,
    ///     2. Transaction block, and then
    ///     3. Transaction index.
    ///
    /// The function processes these individual `ProtocolState` entities and combines all entities
    /// with matching component IDs into a single `ProtocolState`. The final output is a list
    /// where each element is a `ProtocolState` representing a unique component.
    ///
    /// ## Returns:
    /// - A Result containing a vector of `ProtocolState`, otherwise, it will return a StorageError.
    fn _decode_protocol_states<F, P>(
        &self,
        result: Result<Vec<(orm::ProtocolState, String, Bytes)>, diesel::result::Error>,
        context: &str,
        mut from_storage_fn: F,
    ) -> Result<Vec<P>, StorageError>
    where
        F: FnMut(Vec<orm::ProtocolState>, String, &Bytes) -> Result<P, StorageError>,
    {
        match result {
            Ok(data_vec) => {
                let mut protocol_states = Vec::new();

                let mut i = 0;
                while i < data_vec.len() {
                    let stakeholder_start = i;
                    let current_component_id = &data_vec[i].1;

                    // Iterate until the component_id changes
                    while i < data_vec.len() && &data_vec[i].1 == current_component_id {
                        i += 1;
                    }

                    let states_slice = &data_vec[stakeholder_start..i];
                    let tx_hash = &states_slice.last().unwrap().2; // Last element has the latest transaction

                    let protocol_state = from_storage_fn(
                        states_slice
                            .iter()
                            .map(|x| x.0.clone())
                            .collect(),
                        current_component_id.clone(),
                        tx_hash,
                    )?;

                    protocol_states.push(protocol_state);
                }

                Ok(protocol_states)
            }

            Err(err) => Err(StorageError::from_diesel(err, "ProtocolStates", context, None)),
        }
    }
}

#[async_trait]
impl<B, TX, A, D, T> ProtocolGateway for PostgresGateway<B, TX, A, D, T>
where
    B: StorableBlock<orm::Block, orm::NewBlock, i64>,
    TX: StorableTransaction<orm::Transaction, orm::NewTransaction, i64>,
    D: ContractDelta + From<A>,
    A: StorableContract<orm::Contract, orm::NewContract, i64>,
    T: StorableToken<orm::Token, orm::NewToken, i64>,
{
    type DB = AsyncPgConnection;
    type Token = T;
    type ProtocolState = ProtocolState;
    type ProtocolStateDelta = ProtocolStateDelta;
    type ProtocolType = ProtocolType;
    type ProtocolComponent = ProtocolComponent;

    async fn get_protocol_components(
        &self,
        chain: &Chain,
        system: Option<String>,
        ids: Option<&[&str]>,
    ) -> Result<Vec<ProtocolComponent>, StorageError> {
        todo!()
    }

    async fn add_protocol_components(
        &self,
        new: &[&Self::ProtocolComponent],
        conn: &mut Self::DB,
    ) -> Result<(), StorageError> {
        use super::schema::protocol_component::dsl::*;
        let mut values: Vec<orm::NewProtocolComponent> = Vec::with_capacity(new.len());
        let tx_hashes: Vec<TxHash> = new
            .iter()
            .map(|pc| pc.creation_tx.into())
            .collect();
        let tx_hash_id_mapping: HashMap<TxHash, i64> =
            orm::Transaction::id_by_hash(&tx_hashes, conn)
                .await
                .unwrap();

        for pc in new {
            let txh = tx_hash_id_mapping
                .get::<TxHash>(&pc.creation_tx.into())
                .ok_or(StorageError::DecodeError("TxHash not found".to_string()))?;

            let new_pc = pc
                .to_storage(
                    self.get_chain_id(&pc.chain),
                    self.get_protocol_system_id(&pc.protocol_system.to_string()),
                    txh.to_owned(),
                    pc.created_at,
                )
                .unwrap();
            values.push(new_pc);
        }

        diesel::insert_into(protocol_component)
            .values(&values)
            .on_conflict((chain_id, protocol_system_id, external_id))
            .do_nothing()
            .execute(conn)
            .await
            .map_err(|err| StorageError::from_diesel(err, "ProtocolComponent", "", None))
            .unwrap();

        Ok(())
    }

    async fn upsert_protocol_type(
        &self,
        new: &Self::ProtocolType,
        conn: &mut Self::DB,
    ) -> Result<(), StorageError> {
        use super::schema::protocol_type::dsl::*;

        let values: orm::NewProtocolType = new.to_storage();

        diesel::insert_into(protocol_type)
            .values(&values)
            .on_conflict(name)
            .do_update()
            .set(&values)
            .execute(conn)
            .await
            .map_err(|err| StorageError::from_diesel(err, "ProtocolType", &values.name, None))?;

        Ok(())
    }

    // Gets all protocol states from the db filtered by chain, component ids and/or protocol system.
    // The filters are applied in the following order: component ids, protocol system, chain. If
    // component ids are provided, the protocol system filter is ignored. The chain filter is
    // always applied.
    async fn get_protocol_states(
        &self,
        chain: &Chain,
        at: Option<Version>,
        system: Option<String>,
        ids: Option<&[&str]>,
        conn: &mut Self::DB,
    ) -> Result<Vec<Self::ProtocolState>, StorageError> {
        let chain_db_id = self.get_chain_id(chain);
        let version_ts = match &at {
            Some(version) => Some(version.to_ts(conn).await?),
            None => None,
        };

        match (ids, system) {
            (Some(ids), Some(system)) => {
                warn!("Both protocol IDs and system were provided. System will be ignored.");
                self._decode_protocol_states(
                    orm::ProtocolState::by_id(ids, chain_db_id, None, version_ts, conn).await,
                    ids.join(",").as_str(),
                    ProtocolState::from_storage,
                )
            }
            (Some(ids), _) => self._decode_protocol_states(
                orm::ProtocolState::by_id(ids, chain_db_id, None, version_ts, conn).await,
                ids.join(",").as_str(),
                ProtocolState::from_storage,
            ),
            (_, Some(system)) => self._decode_protocol_states(
                orm::ProtocolState::by_protocol_system(
                    system.clone(),
                    chain_db_id,
                    None,
                    version_ts,
                    conn,
                )
                .await,
                system.to_string().as_str(),
                ProtocolState::from_storage,
            ),
            _ => self._decode_protocol_states(
                orm::ProtocolState::by_chain(chain_db_id, None, version_ts, conn).await,
                chain.to_string().as_str(),
                ProtocolState::from_storage,
            ),
        }
    }

    async fn update_protocol_states(
        &self,
        chain: &Chain,
        new: &[ProtocolStateDelta],
        conn: &mut Self::DB,
    ) -> Result<(), StorageError> {
        let chain_db_id = self.get_chain_id(chain);
        let txns: HashMap<H256, (i64, i64, NaiveDateTime)> = orm::Transaction::id_by_hashes(
            new.iter()
                .map(|state| state.modify_tx.as_bytes())
                .collect::<Vec<&[u8]>>()
                .as_slice(),
            conn,
        )
        .await?
        .into_iter()
        .map(|(id, hash, index, ts)| {
            (H256::try_decode(&hash, "tx hash").expect("Failed to decode tx hash"), (id, index, ts))
        })
        .collect();

        let components: HashMap<String, i64> = orm::ProtocolComponent::ids_by_external_ids(
            new.iter()
                .map(|state| state.component_id.as_str())
                .collect::<Vec<&str>>()
                .as_slice(),
            conn,
        )
        .await?
        .into_iter()
        .map(|(id, external_id)| (external_id, id))
        .collect();

        let mut state_data: Vec<(orm::NewProtocolState, i64)> = Vec::new();

        for state in new {
            let tx_db = txns
                .get(&state.modify_tx)
                .expect("Failed to find tx");
            let component_db_id = *components
                .get(&state.component_id)
                .expect("Failed to find component");
            let mut new_states: Vec<(orm::NewProtocolState, i64)> =
                ProtocolStateDelta::to_storage(state, component_db_id, tx_db.0, tx_db.2)
                    .into_iter()
                    .map(|state| (state, tx_db.1))
                    .collect();

            state_data.append(&mut new_states);
        }

        // Sort state_data by protocol_component_id, attribute_name, and transaction index
        state_data.sort_by(|a, b| {
            let order =
                a.0.protocol_component_id
                    .cmp(&b.0.protocol_component_id);
            if order == Ordering::Equal {
                let sub_order =
                    a.0.attribute_name
                        .cmp(&b.0.attribute_name);

                if sub_order == Ordering::Equal {
                    // Sort by block ts and tx_index as well
                    a.1.cmp(&b.1)
                } else {
                    sub_order
                }
            } else {
                order
            }
        });

        // Invalidate older states
        let mut i = 0;
        while i + 1 < state_data.len() {
            let next_state = &state_data[i + 1].0.clone();
            let (current_state, _) = &mut state_data[i];

            // Check if next_state has same protocol_component_id and attribute_name
            if current_state.protocol_component_id == next_state.protocol_component_id &&
                current_state.attribute_name == next_state.attribute_name
            {
                // Invalidate the current state
                current_state.valid_to = Some(next_state.valid_from);
            }

            i += 1;
        }

        let state_data: Vec<orm::NewProtocolState> = state_data
            .into_iter()
            .map(|(state, _index)| state)
            .collect();

        // TODO: invalidate newly outdated protocol states already in the db (ENG-2682)

        // insert the prepared protocol state deltas
        if !state_data.is_empty() {
            diesel::insert_into(schema::protocol_state::table)
                .values(&state_data)
                .execute(conn)
                .await?;
        }
        Ok(())
    }

    async fn get_tokens(
        &self,
        chain: Chain,
        address: Option<&[&Address]>,
        conn: &mut Self::DB,
    ) -> Result<Vec<Self::Token>, StorageError> {
        todo!()
    }

    async fn add_tokens(
        &self,
        chain: Chain,
        token: &[&Self::Token],
        conn: &mut Self::DB,
    ) -> Result<(), StorageError> {
        todo!()
    }

    async fn get_protocol_state_deltas(
        &self,
        chain: &Chain,
        system: Option<String>,
        ids: Option<&[&str]>,
        start_version: Option<&BlockOrTimestamp>,
        end_version: &BlockOrTimestamp,
        conn: &mut Self::DB,
    ) -> Result<Vec<ProtocolStateDelta>, StorageError> {
        let chain_db_id = self.get_chain_id(chain);
        let start_ts = match start_version {
            Some(version) => Some(version.to_ts(conn).await?),
            None => None,
        };
        let end_ts = Some(end_version.to_ts(conn).await?);

        match (ids, system) {
            (Some(ids), Some(system)) => {
                warn!("Both protocol IDs and system were provided. System will be ignored.");
                self._decode_protocol_states(
                    orm::ProtocolState::by_id(ids, chain_db_id, start_ts, end_ts, conn).await,
                    ids.join(",").as_str(),
                    |states, id, hash| {
                        ProtocolStateDelta::from_storage(states, id, hash, ChangeType::Update)
                    },
                )
            }
            (Some(ids), _) => self._decode_protocol_states(
                orm::ProtocolState::by_id(ids, chain_db_id, start_ts, end_ts, conn).await,
                ids.join(",").as_str(),
                |states, id, hash| {
                    ProtocolStateDelta::from_storage(states, id, hash, ChangeType::Update)
                },
            ),
            (_, Some(system)) => self._decode_protocol_states(
                orm::ProtocolState::by_protocol_system(
                    system.clone(),
                    chain_db_id,
                    start_ts,
                    end_ts,
                    conn,
                )
                .await,
                system.as_str(),
                |states, id, hash| {
                    ProtocolStateDelta::from_storage(states, id, hash, ChangeType::Update)
                },
            ),
            _ => self._decode_protocol_states(
                orm::ProtocolState::by_chain(chain_db_id, start_ts, end_ts, conn).await,
                chain.to_string().as_str(),
                |states, id, hash| {
                    ProtocolStateDelta::from_storage(states, id, hash, ChangeType::Update)
                },
            ),
        }
    }

    async fn revert_protocol_state(
        &self,
        to: &BlockIdentifier,
        conn: &mut Self::DB,
    ) -> Result<(), StorageError> {
        todo!()
    }

    async fn _get_or_create_protocol_system_id(
        &self,
        new: String,
        conn: &mut Self::DB,
    ) -> Result<i64, StorageError> {
        use super::schema::protocol_system::dsl::*;

        let existing_entry = protocol_system
            .filter(name.eq(new.to_string().clone()))
            .first::<orm::ProtocolSystem>(conn)
            .await;

        if let Ok(entry) = existing_entry {
            return Ok(entry.id);
        } else {
            let new_entry = orm::NewProtocolSystem { name: new.to_string() };

            let inserted_protocol_system = diesel::insert_into(protocol_system)
                .values(&new_entry)
                .get_result::<orm::ProtocolSystem>(conn)
                .await
                .map_err(|err| {
                    StorageError::from_diesel(err, "ProtocolSystem", &new.to_string(), None)
                })?;
            Ok(inserted_protocol_system.id)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        extractor::{evm, evm::ContractId},
        storage::ChangeType,
    };
    use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
    use diesel_async::AsyncConnection;
    use ethers::types::U256;
    use rstest::rstest;
    use serde_json::json;

    use crate::{
        hex_bytes::Bytes,
        models,
        models::{FinancialType, ImplementationType},
        storage::postgres::{db_fixtures, orm, schema, PostgresGateway},
    };
    use ethers::prelude::H256;
    use std::{collections::HashMap, str::FromStr};

    type EVMGateway = PostgresGateway<
        evm::Block,
        evm::Transaction,
        evm::Account,
        evm::AccountUpdate,
        evm::ERC20Token,
    >;

    async fn setup_db() -> AsyncPgConnection {
        let db_url = std::env::var("DATABASE_URL").unwrap();
        let mut conn = AsyncPgConnection::establish(&db_url)
            .await
            .unwrap();
        conn.begin_test_transaction()
            .await
            .unwrap();

        conn
    }

    /// This sets up the data needed to test the gateway. The setup is structured such that each
    /// protocol state's historical changes are kept together this makes it easy to reason about
    /// that change an account should have at each version Please not that if you change
    /// something here, also update the state fixtures right below, which contain protocol states
    /// at each version.
    async fn setup_data(conn: &mut AsyncPgConnection) {
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        let blk = db_fixtures::insert_blocks(conn, chain_id).await;
        let txn = db_fixtures::insert_txns(
            conn,
            &[
                (
                    blk[0],
                    1i64,
                    "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945",
                ),
                (
                    blk[0],
                    2i64,
                    "0x794f7df7a3fe973f1583fbb92536f9a8def3a89902439289315326c04068de54",
                ),
                // ----- Block 01 LAST
                (
                    blk[1],
                    1i64,
                    "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7",
                ),
                (
                    blk[1],
                    2i64,
                    "0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388",
                ),
                // ----- Block 02 LAST
            ],
        )
        .await;
        let protocol_system_id =
            db_fixtures::insert_protocol_system(conn, "ambient".to_owned()).await;
        let protocol_type_id = db_fixtures::insert_protocol_type(
            conn,
            "Pool",
            Some(orm::FinancialType::Swap),
            None,
            Some(orm::ImplementationType::Custom),
        )
        .await;
        let protocol_component_id = db_fixtures::insert_protocol_component(
            conn,
            "state1",
            chain_id,
            protocol_system_id,
            protocol_type_id,
            txn[0],
        )
        .await;
        let protocol_component_id2 = db_fixtures::insert_protocol_component(
            conn,
            "state2",
            chain_id,
            protocol_system_id,
            protocol_type_id,
            txn[0],
        )
        .await;

        // protocol state for state1-reserve1
        db_fixtures::insert_protocol_state(
            conn,
            protocol_component_id,
            txn[0],
            "reserve1".to_owned(),
            Bytes::from(U256::from(1100)),
            Some(txn[2]),
        )
        .await;

        // protocol state for state1-reserve2
        db_fixtures::insert_protocol_state(
            conn,
            protocol_component_id,
            txn[0],
            "reserve2".to_owned(),
            Bytes::from(U256::from(500)),
            None,
        )
        .await;

        // protocol state update for state1-reserve1
        db_fixtures::insert_protocol_state(
            conn,
            protocol_component_id,
            txn[3],
            "reserve1".to_owned(),
            Bytes::from(U256::from(1000)),
            None,
        )
        .await;
    }

    fn protocol_state() -> ProtocolState {
        let attributes: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(U256::from(1000))),
            ("reserve2".to_owned(), Bytes::from(U256::from(500))),
        ]
        .into_iter()
        .collect();
        ProtocolState::new(
            "state1".to_owned(),
            attributes,
            "0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388"
                .parse()
                .unwrap(),
        )
    }

    #[rstest]
    #[case::by_chain(None, None)]
    #[case::by_system(Some("ambient".to_string()), None)]
    #[case::by_ids(None, Some(vec!["state1"]))]
    #[tokio::test]
    async fn test_get_protocol_states(
        #[case] system: Option<String>,
        #[case] ids: Option<Vec<&str>>,
    ) {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;

        let expected = vec![protocol_state()];

        let gateway = EVMGateway::from_connection(&mut conn).await;

        let result = gateway
            .get_protocol_states(&Chain::Ethereum, None, system, ids.as_deref(), &mut conn)
            .await
            .unwrap();

        assert_eq!(result, expected)
    }

    #[tokio::test]
    async fn test_get_protocol_states_at() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;

        let gateway = EVMGateway::from_connection(&mut conn).await;

        let mut protocol_state = protocol_state();
        let attributes: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(U256::from(1100))),
            ("reserve2".to_owned(), Bytes::from(U256::from(500))),
        ]
        .into_iter()
        .collect();
        protocol_state.attributes = attributes;
        protocol_state.modify_tx =
            "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945"
                .parse()
                .unwrap();
        let expected = vec![protocol_state];

        let result = gateway
            .get_protocol_states(
                &Chain::Ethereum,
                Some(Version::from_block_number(Chain::Ethereum, 1)),
                None,
                None,
                &mut conn,
            )
            .await
            .unwrap();

        assert_eq!(result, expected)
    }

    fn protocol_state_delta() -> ProtocolStateDelta {
        let attributes: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(U256::from(1000))),
            ("reserve2".to_owned(), Bytes::from(U256::from(500))),
        ]
        .into_iter()
        .collect();
        ProtocolStateDelta::new(
            "state2".to_owned(),
            attributes,
            "0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388"
                .parse()
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn test_update_protocol_states() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;

        let gateway = EVMGateway::from_connection(&mut conn).await;
        let chain = Chain::Ethereum;

        // update
        let mut new_state1 = protocol_state_delta();
        let attributes1: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(U256::from(700))),
            ("reserve2".to_owned(), Bytes::from(U256::from(700))),
        ]
        .into_iter()
        .collect();
        new_state1.updated_attributes = attributes1.clone();
        new_state1.modify_tx = "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7"
            .parse()
            .unwrap();

        // newer update
        let mut new_state2 = protocol_state_delta();
        let attributes2: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(U256::from(800))),
            ("reserve2".to_owned(), Bytes::from(U256::from(800))),
        ]
        .into_iter()
        .collect();
        new_state2.updated_attributes = attributes2.clone();

        // update the protocol state
        gateway
            .update_protocol_states(&chain, &[new_state1.clone(), new_state2.clone()], &mut conn)
            .await
            .expect("Failed to update protocol states");

        // check the correct state is considered the valid one
        let db_states = gateway
            .get_protocol_states(
                &chain,
                None,
                None,
                Some(&[new_state1.component_id.as_str()]),
                &mut conn,
            )
            .await
            .expect("Failed ");
        let mut expected_state = protocol_state();
        expected_state.attributes = attributes2;
        expected_state.component_id = new_state1.component_id.clone();
        assert_eq!(db_states[0], expected_state);

        // fetch the older state from the db and check it's valid_to is set correctly
        let tx_hash1: Bytes = new_state1.modify_tx.as_bytes().into();
        let older_state = schema::protocol_state::table
            .inner_join(schema::protocol_component::table)
            .inner_join(schema::transaction::table)
            .filter(schema::transaction::hash.eq(tx_hash1))
            .filter(schema::protocol_component::external_id.eq(new_state1.component_id.as_str()))
            .select(orm::ProtocolState::as_select())
            .first::<orm::ProtocolState>(&mut conn)
            .await
            .expect("Failed to fetch protocol state");
        assert_eq!(older_state.attribute_value, Some(Bytes::from(U256::from(700))));
        // fetch the newer state from the db to compare the valid_from
        let tx_hash2: Bytes = new_state2.modify_tx.as_bytes().into();
        let newer_state = schema::protocol_state::table
            .inner_join(schema::protocol_component::table)
            .inner_join(schema::transaction::table)
            .filter(schema::transaction::hash.eq(tx_hash2))
            .filter(schema::protocol_component::external_id.eq(new_state1.component_id.as_str()))
            .select(orm::ProtocolState::as_select())
            .first::<orm::ProtocolState>(&mut conn)
            .await
            .expect("Failed to fetch protocol state");
        assert_eq!(older_state.valid_to, Some(newer_state.valid_from));
    }

    #[tokio::test]
    async fn test_get_or_create_protocol_system_id() {
        let mut conn = setup_db().await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let first_id = gw
            ._get_or_create_protocol_system_id("ambient".to_string(), &mut conn)
            .await
            .unwrap();

        let second_id = gw
            ._get_or_create_protocol_system_id("ambient".to_string(), &mut conn)
            .await
            .unwrap();
        assert_eq!(first_id, second_id);
    }

    #[tokio::test]
    async fn test_add_protocol_type() {
        let mut conn = setup_db().await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let d = NaiveDate::from_ymd_opt(2015, 6, 3).unwrap();
        let t = NaiveTime::from_hms_milli_opt(12, 34, 56, 789).unwrap();
        let dt = NaiveDateTime::new(d, t);

        let protocol_type = models::ProtocolType {
            name: "Protocol".to_string(),
            financial_type: FinancialType::Debt,
            attribute_schema: Some(json!({"attribute": "schema"})),
            implementation: ImplementationType::Custom,
        };

        gw.upsert_protocol_type(&protocol_type, &mut conn)
            .await
            .unwrap();

        let inserted_data = schema::protocol_type::table
            .filter(schema::protocol_type::name.eq("Protocol"))
            .select(schema::protocol_type::all_columns)
            .first::<orm::ProtocolType>(&mut conn)
            .await
            .unwrap();

        assert_eq!(inserted_data.name, "Protocol".to_string());
        assert_eq!(inserted_data.financial_type, orm::FinancialType::Debt);
        assert_eq!(inserted_data.attribute_schema, Some(json!({"attribute": "schema"})));
        assert_eq!(inserted_data.implementation, orm::ImplementationType::Custom);

        let updated_protocol_type = models::ProtocolType {
            name: "Protocol".to_string(),
            financial_type: FinancialType::Leverage,
            attribute_schema: Some(json!({"attribute": "another_schema"})),
            implementation: ImplementationType::Vm,
        };

        gw.upsert_protocol_type(&updated_protocol_type, &mut conn)
            .await
            .unwrap();

        let newly_inserted_data = schema::protocol_type::table
            .filter(schema::protocol_type::name.eq("Protocol"))
            .select(schema::protocol_type::all_columns)
            .load::<orm::ProtocolType>(&mut conn)
            .await
            .unwrap();

        assert_eq!(newly_inserted_data.len(), 1);
        assert_eq!(newly_inserted_data[0].name, "Protocol".to_string());
        assert_eq!(newly_inserted_data[0].financial_type, orm::FinancialType::Leverage);
        assert_eq!(
            newly_inserted_data[0].attribute_schema,
            Some(json!({"attribute": "another_schema"}))
        );
        assert_eq!(newly_inserted_data[0].implementation, orm::ImplementationType::Vm);
    }

    #[tokio::test]
    async fn test_add_protocol_components() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let protocol_type_id_1 =
            db_fixtures::insert_protocol_type(&mut conn, "Test_Type_1", None, None, None).await;
        let protocol_type_id_2 =
            db_fixtures::insert_protocol_type(&mut conn, "Test_Type_2", None, None, None).await;
        let protocol_system = "ambient".to_string();
        let chain = Chain::Ethereum;
        let original_component = ProtocolComponent {
            id: ContractId("test_contract_id".to_string()),
            protocol_system,
            protocol_type_id: protocol_type_id_1.to_string(),
            chain,
            tokens: vec![],
            contract_ids: vec![],
            static_attributes: HashMap::new(),
            change: ChangeType::Creation,
            creation_tx: H256::from_str(
                "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945",
            )
            .unwrap(),
            created_at: Default::default(),
        };

        let result = gw
            .add_protocol_components(&[&original_component.clone()], &mut conn)
            .await;

        assert!(result.is_ok());

        let inserted_data = schema::protocol_component::table
            .filter(schema::protocol_component::external_id.eq("test_contract_id".to_string()))
            .select(orm::ProtocolComponent::as_select())
            .first::<orm::ProtocolComponent>(&mut conn)
            .await;

        assert!(inserted_data.is_ok());
        let inserted_data: orm::ProtocolComponent = inserted_data.unwrap();
        assert_eq!(
            original_component.protocol_type_id,
            inserted_data
                .protocol_type_id
                .to_string()
        );
        assert_eq!(
            original_component.protocol_type_id,
            inserted_data
                .protocol_type_id
                .to_string()
        );
        assert_eq!(
            gw.get_protocol_system_id(
                &original_component
                    .protocol_system
                    .to_string()
            ),
            inserted_data.protocol_system_id
        );
        assert_eq!(gw.get_chain_id(&original_component.chain), inserted_data.chain_id);
        assert_eq!(original_component.id.0, inserted_data.external_id);
    }
}
