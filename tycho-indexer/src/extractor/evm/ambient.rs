#![allow(unused_variables)]

use std::{str::FromStr, sync::Arc};

use async_trait::async_trait;
use diesel_async::{
    pooled_connection::deadpool::Pool, scoped_futures::ScopedFutureExt, AsyncConnection,
    AsyncPgConnection,
};
use ethers::types::{H160, H256};
use mockall::automock;
use prost::Message;
use tokio::sync::Mutex;
use tracing::{debug, info, instrument};

use super::{AccountUpdate, Block};
use crate::{
    extractor::{evm, ExtractionError, Extractor, ExtractorMsg},
    hex_bytes::Bytes,
    models::{Chain, ExtractionState, ExtractorIdentity},
    pb::{
        sf::substreams::rpc::v2::{BlockScopedData, BlockUndoSignal, ModulesProgress},
        tycho::evm::v1::BlockContractChanges,
    },
    storage::{postgres::cache::CachedGateway, BlockIdentifier, BlockOrTimestamp, StorageError},
};

const AMBIENT_CONTRACT: [u8; 20] = hex_literal::hex!("aaaaaaaaa24eeeb8d57d431224f73832bc34f688");

struct Inner {
    cursor: Vec<u8>,
}

pub struct AmbientContractExtractor<G> {
    gateway: G,
    name: String,
    chain: Chain,
    protocol_system: String,
    // TODO: There is not reason this needs to be shared
    // try removing the Mutex
    inner: Arc<Mutex<Inner>>,
}

impl<DB> AmbientContractExtractor<DB> {
    async fn update_cursor(&self, cursor: String) {
        let cursor_bytes: Vec<u8> = cursor.into();
        let mut state = self.inner.lock().await;
        state.cursor = cursor_bytes;
    }
}

pub struct AmbientPgGateway {
    name: String,
    chain: Chain,
    pool: Pool<AsyncPgConnection>,
    state_gateway: CachedGateway,
}

#[automock]
#[async_trait]
pub trait AmbientGateway: Send + Sync {
    async fn get_cursor(&self) -> Result<Vec<u8>, StorageError>;
    async fn upsert_contract(
        &self,
        changes: &evm::BlockContractChanges,
        new_cursor: &str,
    ) -> Result<(), StorageError>;

    async fn revert(
        &self,
        to: &BlockIdentifier,
        new_cursor: &str,
    ) -> Result<evm::BlockAccountChanges, StorageError>;
}

impl AmbientPgGateway {
    pub fn new(name: &str, chain: Chain, pool: Pool<AsyncPgConnection>, gw: CachedGateway) -> Self {
        AmbientPgGateway { name: name.to_owned(), chain, pool, state_gateway: gw }
    }

    #[instrument(skip_all)]
    async fn save_cursor(&self, block: &Block, new_cursor: &str) -> Result<(), StorageError> {
        let state =
            ExtractionState::new(self.name.to_string(), self.chain, None, new_cursor.as_bytes());
        self.state_gateway
            .save_state(block, &state)
            .await?;
        Ok(())
    }

    #[instrument(skip_all, fields(chain = % self.chain, name = % self.name, block_number = % changes.block.number))]
    async fn forward(
        &self,
        changes: &evm::BlockContractChanges,
        new_cursor: &str,
    ) -> Result<(), StorageError> {
        debug!("Upserting block");
        self.state_gateway
            .upsert_block(&changes.block)
            .await?;
        for update in changes.tx_updates.iter() {
            debug!(tx_hash = ?update.tx.hash, "Processing transaction");
            self.state_gateway
                .upsert_tx(&changes.block, &update.tx)
                .await?;
            if update.is_creation() {
                let new: evm::Account = update.into();
                info!(block_number = ?changes.block.number, contract_address = ?new.address, "New contract found at {:#020x}", &new.address);
                self.state_gateway
                    .insert_contract(&changes.block, &new)
                    .await?;
            }
        }
        let collected_changes: Vec<(Bytes, AccountUpdate)> = changes
            .tx_updates
            .iter()
            .filter(|&u| u.is_update())
            .map(|u| (u.tx.hash.into(), u.update.clone()))
            .collect();
        let changes_slice: &[(Bytes, AccountUpdate)] = collected_changes.as_slice();

        self.state_gateway
            .update_contracts(&changes.block, changes_slice)
            .await?;
        self.save_cursor(&changes.block, new_cursor)
            .await?;

        Result::<(), StorageError>::Ok(())
    }

    #[instrument(skip_all, fields(chain = % self.chain, name = % self.name, block = ? to))]
    async fn backward(
        &self,
        to: &BlockIdentifier,
        new_cursor: &str,
        conn: &mut AsyncPgConnection,
    ) -> Result<evm::BlockAccountChanges, StorageError> {
        let block = self
            .state_gateway
            .get_block(to, conn)
            .await?;
        let target = BlockOrTimestamp::Block(to.clone());
        let address = H160(AMBIENT_CONTRACT);
        let account_updates = self
            .state_gateway
            .get_accounts_delta(&self.chain, None, &target, conn)
            .await?
            .into_iter()
            .filter_map(|u| if u.address == address { Some((u.address, u)) } else { None })
            .collect();
        self.state_gateway
            .revert_state(to)
            .await?;

        self.save_cursor(&block, new_cursor)
            .await?;

        let changes = evm::BlockAccountChanges::new(
            &self.name,
            self.chain,
            block,
            account_updates,
            // TODO: get protocol components from gateway (in ENG-2049)
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        Result::<evm::BlockAccountChanges, StorageError>::Ok(changes)
    }

    async fn get_last_cursor(&self, conn: &mut AsyncPgConnection) -> Result<Vec<u8>, StorageError> {
        let state = self
            .state_gateway
            .get_state(&self.name, &self.chain, conn)
            .await?;
        Ok(state.cursor)
    }
}

#[async_trait]
impl AmbientGateway for AmbientPgGateway {
    async fn get_cursor(&self) -> Result<Vec<u8>, StorageError> {
        let mut conn = self.pool.get().await.unwrap();
        self.get_last_cursor(&mut conn).await
    }

    #[instrument(skip_all, fields(chain = % self.chain, name = % self.name, block_number = % changes.block.number))]
    async fn upsert_contract(
        &self,
        changes: &evm::BlockContractChanges,
        new_cursor: &str,
    ) -> Result<(), StorageError> {
        let mut conn = self.pool.get().await.unwrap();
        conn.transaction(|_conn| {
            async move { self.forward(changes, new_cursor).await }.scope_boxed()
        })
        .await?;
        Ok(())
    }

    #[instrument(skip_all, fields(chain = % self.chain, name = % self.name, block_number = % to))]
    async fn revert(
        &self,
        to: &BlockIdentifier,
        new_cursor: &str,
    ) -> Result<evm::BlockAccountChanges, StorageError> {
        let mut conn = self.pool.get().await.unwrap();
        let res = conn
            .transaction(|conn| {
                async move {
                    self.backward(to, new_cursor, conn)
                        .await
                }
                .scope_boxed()
            })
            .await?;
        Ok(res)
    }
}

impl<G> AmbientContractExtractor<G>
where
    G: AmbientGateway,
{
    pub async fn new(name: &str, chain: Chain, gateway: G) -> Result<Self, ExtractionError> {
        // check if this extractor has state
        let res = match gateway.get_cursor().await {
            Err(StorageError::NotFound(_, _)) => AmbientContractExtractor {
                gateway,
                name: name.to_owned(),
                chain,
                inner: Arc::new(Mutex::new(Inner { cursor: Vec::new() })),
                protocol_system: "ambient".to_string(),
            },
            Ok(cursor) => AmbientContractExtractor {
                gateway,
                name: name.to_owned(),
                chain,
                inner: Arc::new(Mutex::new(Inner { cursor })),
                protocol_system: "ambient".to_string(),
            },
            Err(err) => return Err(ExtractionError::Setup(err.to_string())),
        };
        Ok(res)
    }
}

#[async_trait]
impl<G> Extractor for AmbientContractExtractor<G>
where
    G: AmbientGateway,
{
    fn get_id(&self) -> ExtractorIdentity {
        ExtractorIdentity::new(self.chain, &self.name)
    }

    async fn get_cursor(&self) -> String {
        String::from_utf8(self.inner.lock().await.cursor.clone()).expect("Cursor is utf8")
    }

    #[instrument(skip_all, fields(chain = % self.chain, name = % self.name))]
    async fn handle_tick_scoped_data(
        &self,
        inp: BlockScopedData,
    ) -> Result<Option<ExtractorMsg>, ExtractionError> {
        let _data = inp
            .output
            .as_ref()
            .unwrap()
            .map_output
            .as_ref()
            .unwrap();

        let raw_msg = BlockContractChanges::decode(_data.value.as_slice())?;

        debug!(?raw_msg, "Received message");

        // TODO: figure out how/where to get this ID from (in ENG-2049)
        let protocol_type_id = String::from("id-1");
        let msg = match evm::BlockContractChanges::try_from_message(
            raw_msg,
            &self.name,
            self.chain,
            self.protocol_system.clone(),
            protocol_type_id,
        ) {
            Ok(changes) => {
                tracing::Span::current().record("block_number", changes.block.number);
                changes
            }
            Err(ExtractionError::Empty) => {
                self.update_cursor(inp.cursor).await;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        self.gateway
            .upsert_contract(&msg, inp.cursor.as_ref())
            .await?;

        self.update_cursor(inp.cursor).await;
        let msg = Arc::new(msg.aggregate_updates()?);
        Ok(Some(msg))
    }

    #[instrument(skip_all, fields(chain = % self.chain, name = % self.name, block_number = % inp.last_valid_block.as_ref().unwrap().number))]
    async fn handle_revert(
        &self,
        inp: BlockUndoSignal,
    ) -> Result<Option<ExtractorMsg>, ExtractionError> {
        let block_ref = inp
            .last_valid_block
            .ok_or_else(|| ExtractionError::DecodeError("Revert without block ref".into()))?;
        let block_hash = H256::from_str(&block_ref.id).map_err(|err| {
            ExtractionError::DecodeError(format!(
                "Failed to parse {} as block hash: {}",
                block_ref.id, err
            ))
        })?;
        let changes = self
            .gateway
            .revert(&BlockIdentifier::Hash(block_hash.into()), inp.last_valid_cursor.as_ref())
            .await?;
        self.update_cursor(inp.last_valid_cursor)
            .await;

        Ok((!changes.account_updates.is_empty()).then_some(Arc::new(changes)))
    }

    #[instrument(skip_all)]
    async fn handle_progress(&self, _inp: ModulesProgress) -> Result<(), ExtractionError> {
        todo!()
    }
}

#[cfg(test)]
mod test {
    use crate::{extractor::evm, pb::sf::substreams::v1::BlockRef};

    use super::*;

    #[tokio::test]
    async fn test_get_cursor() {
        let mut gw = MockAmbientGateway::new();
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok("cursor".into()));
        let extractor = AmbientContractExtractor::new("vm:ambient", Chain::Ethereum, gw)
            .await
            .expect("extractor init ok");

        let res = extractor.get_cursor().await;

        assert_eq!(res, "cursor");
    }

    fn block_contract_changes_ok() -> BlockContractChanges {
        let mut data = evm::fixtures::pb_block_contract_changes();
        // TODO: make fixtures configurable through parameters so they can be
        // properly reused. Will need fixture to easily assemble contract
        // change objects.
        data.changes[0]
            .contract_changes
            .remove(1);
        data
    }

    #[tokio::test]
    async fn test_handle_tick_scoped_data() {
        let mut gw = MockAmbientGateway::new();
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok("cursor".into()));
        gw.expect_upsert_contract()
            .times(1)
            .returning(|_, _| Ok(()));
        let extractor = AmbientContractExtractor::new("vm:ambient", Chain::Ethereum, gw)
            .await
            .expect("extractor init ok");
        let inp = evm::fixtures::pb_block_scoped_data(block_contract_changes_ok());
        let exp = Ok(Some(()));

        let res = extractor
            .handle_tick_scoped_data(inp)
            .await
            .map(|o| o.map(|_| ()));

        assert_eq!(res, exp);
        assert_eq!(extractor.get_cursor().await, "cursor@420");
    }

    #[tokio::test]
    async fn test_handle_tick_scoped_data_skip() {
        let mut gw = MockAmbientGateway::new();
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok("cursor".into()));
        gw.expect_upsert_contract()
            .times(0)
            .returning(|_, _| Ok(()));
        let extractor = AmbientContractExtractor::new("vm:ambient", Chain::Ethereum, gw)
            .await
            .expect("extractor init ok");
        let inp = evm::fixtures::pb_block_scoped_data(());

        let _res = extractor
            .handle_tick_scoped_data(inp)
            .await;

        // TODO: fix this assert
        // assert_eq!(res, Ok(None));
        assert_eq!(extractor.get_cursor().await, "cursor@420");
    }

    fn undo_signal() -> BlockUndoSignal {
        BlockUndoSignal {
            last_valid_block: Some(BlockRef { id: evm::fixtures::HASH_256_0.into(), number: 400 }),
            last_valid_cursor: "cursor@400".into(),
        }
    }

    #[tokio::test]
    async fn test_handle_revert() {
        let mut gw = MockAmbientGateway::new();
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok("cursor".into()));
        gw.expect_revert()
            .withf(|v, cursor| {
                v == &BlockIdentifier::Hash(evm::fixtures::HASH_256_0.into()) &&
                    cursor == "cursor@400"
            })
            .times(1)
            .returning(|_, _| Ok(evm::BlockAccountChanges::default()));
        let extractor = AmbientContractExtractor::new("vm:ambient", Chain::Ethereum, gw)
            .await
            .expect("extractor init ok");
        let inp = undo_signal();

        let res = extractor.handle_revert(inp).await;

        assert!(matches!(res, Ok(None)));
        assert_eq!(extractor.get_cursor().await, "cursor@400");
    }
}

#[cfg(test)]
mod gateway_test {
    //! It is notoriously hard to mock postgres here, we would need to have traits and abstractions
    //! for the connection pooling as well as for transaction handling so the easiest way
    //! forward is to just run these tests against a real postgres instance.
    //!
    //! The challenge here is to leave the database empty. So we need to initiate a test transaction
    //! and should avoid calling the trait methods which start a transaction of their own. So we do
    //! that by moving the main logic of each trait method into a private method and test this
    //! method instead.
    //!
    //! Note that it is ok to use higher level db methods here as there is a layer of abstraction
    //! between this component and the actual db interactions
    use std::collections::HashMap;

    use crate::storage::{postgres::PostgresGateway, ChangeType, ContractId};
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use ethers::types::U256;
    use mpsc::channel;
    use tokio::sync::{
        mpsc,
        mpsc::{error::TryRecvError::Empty, Receiver},
    };

    use super::*;

    const TX_HASH_0: &str = "0x2f6350a292c0fc918afe67cb893744a080dacb507b0cea4cc07437b8aff23cdb";
    const TX_HASH_1: &str = "0x0d9e0da36cf9f305a189965b248fc79c923619801e8ab5ef158d4fd528a291ad";
    const BLOCK_HASH_0: &str = "0x98b4a4fef932b1862be52de218cc32b714a295fae48b775202361a6fa09b66eb";

    async fn setup_gw() -> (AmbientPgGateway, Receiver<StorageError>, Pool<AsyncPgConnection>) {
        let db_url = std::env::var("DATABASE_URL").expect("database url should be set for testing");
        let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(db_url);
        // We need a dedicated connection, so we don't use the pool as this would actually insert
        // data. For this, we create a pool of 1 connection.
        let pool = Pool::builder(config)
            .max_size(1)
            .build()
            .unwrap();
        let mut conn = pool
            .get()
            .await
            .expect("pool should get a connection");
        conn.begin_test_transaction()
            .await
            .expect("starting test transaction should succeed");
        postgres::db_fixtures::insert_chain(&mut conn, "ethereum").await;
        let evm_gw = Arc::new(
            PostgresGateway::<
                evm::Block,
                evm::Transaction,
                evm::Account,
                evm::AccountUpdate,
                evm::ERC20Token,
            >::from_connection(&mut conn)
            .await,
        );

        let (tx, rx) = channel(10);
        let (err_tx, err_rx) = channel(10);

        let write_executor = crate::storage::postgres::cache::DBCacheWriteExecutor::new(
            "ethereum".to_owned(),
            Chain::Ethereum,
            pool.clone(),
            evm_gw.clone(),
            rx,
            err_tx,
        );

        let handle = write_executor.run();
        let cached_gw = CachedGateway::new(tx, pool.clone(), evm_gw.clone());

        let gw = AmbientPgGateway::new("vm:ambient", Chain::Ethereum, pool.clone(), cached_gw);
        (gw, err_rx, pool)
    }

    #[tokio::test]
    async fn test_get_cursor() {
        let (gw, mut err_rx, pool) = setup_gw().await;
        let evm_gw = gw.state_gateway.clone();
        let state = ExtractionState::new(
            "vm:ambient".to_string(),
            Chain::Ethereum,
            None,
            "cursor@420".as_bytes(),
        );
        let mut conn = pool
            .get()
            .await
            .expect("pool should get a connection");
        evm_gw
            .save_state(&state, &mut conn)
            .await
            .expect("extaction state insertion succeeded");

        let maybe_err = err_rx
            .try_recv()
            .expect_err("Error channel should be empty");

        let cursor = gw
            .get_last_cursor(&mut conn)
            .await
            .expect("get cursor should succeed");

        assert_eq!(cursor, "cursor@420".as_bytes());
        // Assert no error happened
        assert_eq!(maybe_err, Empty);
    }

    fn ambient_account(at_version: u64) -> evm::Account {
        match at_version {
            0 => evm::Account::new(
                Chain::Ethereum,
                "0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688"
                    .parse()
                    .unwrap(),
                "0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688".to_owned(),
                evm::fixtures::evm_slots([(1, 200)]),
                U256::from(1000),
                vec![0, 0, 0, 0].into(),
                "0xe8e77626586f73b955364c7b4bbf0bb7f7685ebd40e852b164633a4acbd3244c"
                    .parse()
                    .unwrap(),
                "0x2f6350a292c0fc918afe67cb893744a080dacb507b0cea4cc07437b8aff23cdb"
                    .parse()
                    .unwrap(),
                H256::zero(),
                Some(H256::zero()),
            ),
            _ => panic!("Unkown version"),
        }
    }

    fn ambient_creation_and_update() -> evm::BlockContractChanges {
        evm::BlockContractChanges {
            extractor: "vm:ambient".to_owned(),
            chain: Chain::Ethereum,
            block: evm::Block::default(),
            tx_updates: vec![
                evm::AccountUpdateWithTx::new(
                    H160(AMBIENT_CONTRACT),
                    Chain::Ethereum,
                    HashMap::new(),
                    None,
                    Some(vec![0, 0, 0, 0].into()),
                    ChangeType::Creation,
                    evm::fixtures::transaction01(),
                ),
                evm::AccountUpdateWithTx::new(
                    H160(AMBIENT_CONTRACT),
                    Chain::Ethereum,
                    evm::fixtures::evm_slots([(1, 200)]),
                    Some(U256::from(1000)),
                    None,
                    ChangeType::Update,
                    evm::fixtures::transaction02(TX_HASH_0, evm::fixtures::HASH_256_0, 1),
                ),
            ],
            protocol_components: Vec::new(),
            tvl_changes: Vec::new(),
        }
    }

    fn ambient_update02() -> evm::BlockContractChanges {
        let block = evm::Block {
            number: 1,
            chain: Chain::Ethereum,
            hash: BLOCK_HASH_0.parse().unwrap(),
            parent_hash: H256::zero(),
            ts: "2020-01-01T01:00:00".parse().unwrap(),
        };
        evm::BlockContractChanges {
            extractor: "vm:ambient".to_owned(),
            chain: Chain::Ethereum,
            block,
            tx_updates: vec![evm::AccountUpdateWithTx::new(
                H160(AMBIENT_CONTRACT),
                Chain::Ethereum,
                evm::fixtures::evm_slots([(42, 0xbadbabe)]),
                Some(U256::from(2000)),
                None,
                ChangeType::Update,
                evm::fixtures::transaction02(TX_HASH_1, BLOCK_HASH_0, 1),
            )],
            protocol_components: Vec::new(),
            tvl_changes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_upsert_contract() {
        let (gw, mut err_rx, pool) = setup_gw().await;
        let msg = ambient_creation_and_update();
        let exp = ambient_account(0);

        gw.forward(&msg, "cursor@500")
            .await
            .expect("upsert should succeed");

        let cached_gw: CachedGateway = gw.state_gateway;
        cached_gw
            .flush()
            .await
            .expect("Received signal ok")
            .expect("Flush ok");

        let maybe_err = err_rx
            .try_recv()
            .expect_err("Error channel should be empty");

        let mut conn = pool
            .get()
            .await
            .expect("pool should get a connection");
        let res = cached_gw
            .get_contract(
                &ContractId::new(Chain::Ethereum, AMBIENT_CONTRACT.into()),
                None,
                true,
                &mut conn,
            )
            .await
            .expect("test successfully inserted ambient contract");
        assert_eq!(res, exp);
        // Assert no error happened
        assert_eq!(maybe_err, Empty);
    }

    // This test is stuck due to how we handle db lock during the test. TODO: fix this test. https://datarevenue.atlassian.net/browse/ENG-2635
    // #[tokio::test]
    // async fn test_revert() {
    //     let db_url = std::env::var("DATABASE_URL").expect("database url should be set for
    // testing");     let config =
    // AsyncDieselConnectionManager::<AsyncPgConnection>::new(db_url);     // We need a
    // dedicated connection, so we don't use the pool as this would actually insert     // data.
    // For this, we create a pool of 1 connection.     let pool = Pool::builder(config)
    //         .max_size(1)
    //         .build()
    //         .unwrap();
    //     let mut conn = pool
    //         .get()
    //         .await
    //         .expect("pool should get a connection");
    //     conn.begin_test_transaction()
    //         .await
    //         .expect("starting test transaction should succeed");
    //     postgres::db_fixtures::insert_chain(&mut conn, "ethereum").await;
    //     let evm_gw = Arc::new(
    //         PostgresGateway::<
    //             evm::Block,
    //             evm::Transaction,
    //             evm::Account,
    //             evm::AccountUpdate,
    //             evm::ERC20Token,
    //         >::from_connection(&mut conn)
    //         .await,
    //     );

    //     let (tx, rx) = channel(10);
    //     let (err_tx, mut err_rx) = channel(10);

    //     let write_executor = crate::storage::postgres::cache::DBCacheWriteExecutor::new(
    //         "ethereum".to_owned(),
    //         Chain::Ethereum,
    //         pool.clone(),
    //         evm_gw.clone(),
    //         rx,
    //         err_tx,
    //     );

    //     let handle = write_executor.run();
    //     let cached_gw = CachedGateway::new(tx, pool.clone(), evm_gw.clone());

    //     let gw = AmbientPgGateway::new("vm:ambient", Chain::Ethereum, pool.clone(), cached_gw);

    //     let msg0 = ambient_creation_and_update();
    //     let msg1 = ambient_update02();
    //     gw.forward(&msg0, "cursor@0")
    //         .await
    //         .expect("upsert should succeed");
    //     gw.forward(&msg1, "cursor@1")
    //         .await
    //         .expect("upsert should succeed");
    // let ambient_address = H160(AMBIENT_CONTRACT);
    // let exp_change = evm::AccountUpdate::new(
    //     ambient_address,
    //     Chain::Ethereum,
    //     evm::fixtures::evm_slots([(42, 0)]),
    //     Some(U256::from(1000)),
    //     None,
    //     ChangeType::Update,
    // );
    // let exp_account = ambient_account(0);

    // let mut conn = pool
    //     .get()
    //     .await
    //     .expect("pool should get a connection");

    // let changes = gw
    //     .backward(&BlockIdentifier::Number((Chain::Ethereum, 0)), "cursor@2", &mut conn)
    //     .await
    //     .expect("revert should succeed");

    // let maybe_err = err_rx
    //     .try_recv()
    //     .expect_err("Error channel should be empty");

    // assert_eq!(changes.account_updates.len(), 1);
    // assert_eq!(changes.account_updates[&ambient_address], exp_change);
    // let cached_gw: CachedGateway = gw.state_gateway;
    // let account = cached_gw
    //     .get_contract(
    //         &ContractId::new(Chain::Ethereum, AMBIENT_CONTRACT.into()),
    //         None,
    //         true,
    //         &mut conn,
    //     )
    //     .await
    //     .expect("test successfully retrieved ambient contract");
    // assert_eq!(account, exp_account);
    // // Assert no error happened
    // assert_eq!(maybe_err, Empty);
    // }
}
