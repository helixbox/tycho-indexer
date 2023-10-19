//! This module contains Tycho web services implementation

use std::{collections::HashMap, sync::Arc};

use actix_web::{dev::ServerHandle, web, App, HttpServer};
use actix_web_opentelemetry::RequestTracing;
use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection};
use tokio::task::JoinHandle;

use crate::{
    extractor::{evm, runner::ExtractorHandle, ExtractionError},
    models::NormalisedMessage,
    storage::postgres::PostgresGateway,
};

mod rpc;
mod ws;

pub type EvmPostgresGateway =
    PostgresGateway<evm::Block, evm::Transaction, evm::Account, evm::AccountUpdate>;

pub struct ServicesBuilder<M> {
    prefix: String,
    port: u16,
    bind: String,
    extractor_handles: ws::MessageSenderMap<M>,
    db_gateway: Arc<EvmPostgresGateway>,
    db_connection_pool: Pool<AsyncPgConnection>,
}

impl<M: NormalisedMessage> ServicesBuilder<M> {
    pub fn new(
        db_gateway: Arc<EvmPostgresGateway>,
        db_connection_pool: Pool<AsyncPgConnection>,
    ) -> Self {
        Self {
            prefix: "v1".to_owned(),
            port: 4242,
            bind: "0.0.0.0".to_owned(),
            extractor_handles: HashMap::new(),
            db_gateway,
            db_connection_pool,
        }
    }

    pub fn register_extractor(mut self, handle: ExtractorHandle<M>) -> Self {
        let id = handle.get_id();
        self.extractor_handles
            .insert(id, Arc::new(handle));
        self
    }

    pub fn prefix(mut self, v: &str) -> Self {
        self.prefix = v.to_owned();
        self
    }

    pub fn bind(mut self, v: &str) -> Self {
        self.bind = v.to_owned();
        self
    }

    pub fn port(mut self, v: u16) -> Self {
        self.port = v;
        self
    }

    pub fn run(
        self,
    ) -> Result<(ServerHandle, JoinHandle<Result<(), ExtractionError>>), ExtractionError> {
        let ws_data = web::Data::new(ws::WsData::<M>::new(self.extractor_handles));
        let rpc_data =
            web::Data::new(rpc::RpcHandler::new(self.db_gateway, self.db_connection_pool));
        let server = HttpServer::new(move || {
            App::new()
                .app_data(rpc_data.clone())
                .service(
                    web::resource(format!("/{}/contract_state", self.prefix))
                        .route(web::post().to(rpc::contract_state)),
                )
                .app_data(ws_data.clone())
                .service(
                    web::resource(format!("/{}/ws", self.prefix))
                        .route(web::get().to(ws::WsActor::<M>::ws_index)),
                )
                .wrap(RequestTracing::new())
        })
        .bind((self.bind, self.port))
        .map_err(|err| ExtractionError::ServiceError(err.to_string()))?
        .run();
        let handle = server.handle();
        let server = async move {
            let res = server.await;
            res.map_err(|err| ExtractionError::Unknown(err.to_string()))
        };
        let task = tokio::spawn(server);
        Ok((handle, task))
    }
}