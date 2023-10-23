mod commons;

use std::sync::Arc;

use async_trait::async_trait;
use diesel::{
    r2d2::{ConnectionManager, Pool},
    PgConnection,
};
use ethers::{
    middleware::SignerMiddleware,
    providers::{Http, Provider},
    signers::LocalWallet,
    types::Log,
};
use mibs::types::{Listener as MibsListener, Update};

use crate::{db::models, http_client::HttpClient};

use self::commons::{
    acknowledge_active_oracles, handle_active_oracles_answering, parse_kpi_token_creation_log,
};

pub struct Listener {
    chain_id: u64,
    template_id: u64,
    signer: Arc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    db_connection_pool: Pool<ConnectionManager<PgConnection>>,
    scanning_past: bool,
    ipfs_http_client: Arc<HttpClient>,
    defillama_http_client: Arc<HttpClient>,
    web3_storage_http_client: Option<Arc<HttpClient>>,
}

impl Listener {
    pub fn new(
        chain_id: u64,
        template_id: u64,
        signer: Arc<SignerMiddleware<Provider<Http>, LocalWallet>>,
        db_connection_pool: Pool<ConnectionManager<PgConnection>>,
        ipfs_http_client: Arc<HttpClient>,
        defillama_http_client: Arc<HttpClient>,
        web3_storage_http_client: Option<Arc<HttpClient>>,
    ) -> Self {
        Self {
            chain_id,
            template_id,
            signer,
            db_connection_pool,
            ipfs_http_client,
            defillama_http_client,
            web3_storage_http_client,
            scanning_past: true,
        }
    }

    async fn on_log(&self, log: Log) {
        let block_number = match log.block_number {
            Some(block_number) => block_number.as_u64(),
            None => {
                tracing::warn!("could not get block number from log {:?}", log);
                return;
            }
        };

        let oracles_data = match parse_kpi_token_creation_log(
            self.chain_id,
            self.signer.clone(),
            log,
            self.template_id,
        )
        .await
        {
            Ok(oracle_datas) => oracle_datas,
            Err(error) => {
                tracing::warn!(
                    "could not extract oracles data from log at block {}: {:#}",
                    block_number,
                    error
                );
                return;
            }
        };

        let oracles_data_len = oracles_data.len();
        if oracles_data_len > 0 {
            tracing::info!(
                "{} oracle creation(s) detected on block {}",
                oracles_data_len,
                block_number
            );
        }

        acknowledge_active_oracles(
            self.chain_id,
            oracles_data,
            self.db_connection_pool.clone(),
            self.ipfs_http_client.clone(),
            self.defillama_http_client.clone(),
            self.web3_storage_http_client.clone(),
        )
        .await;
    }

    async fn update_checkpoint_block_number(&self, block_number: u64) {
        let mut db_connection = match self.db_connection_pool.get() {
            Ok(db_connection) => db_connection,
            Err(err) => {
                tracing::error!("could not get new connection from pool: {:#}", err);
                return;
            }
        };
        if let Err(error) =
            models::Checkpoint::update(&mut db_connection, self.chain_id, block_number as i64)
        {
            tracing::error!("could not update snapshot block number - {:#}", error);
        }
    }
}

#[async_trait]
impl MibsListener for Listener {
    async fn on_update(&mut self, update: Update) {
        match update {
            Update::NewLog(log) => self.on_log(log).await,
            Update::PastBatchCompleted {
                from_block,
                to_block,
                progress_percentage,
            } => {
                if progress_percentage == 100f32 {
                    tracing::info!("finished scanning past blocks");
                    self.scanning_past = false;
                } else {
                    tracing::info!(
                        "{} -> {} - scanned {}% of past blocks",
                        from_block,
                        to_block,
                        progress_percentage
                    );
                }

                self.update_checkpoint_block_number(to_block).await;
            }
            Update::NewBlock(block_number) => {
                if let Err(error) = handle_active_oracles_answering(
                    self.chain_id,
                    self.signer.clone(),
                    self.db_connection_pool.clone(),
                    self.defillama_http_client.clone(),
                )
                .await
                {
                    tracing::error!("error while handling active oracles answering: {:#}", error);
                }

                if !self.scanning_past {
                    self.update_checkpoint_block_number(block_number).await;
                }
            }
        }
    }
}