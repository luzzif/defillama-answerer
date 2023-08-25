use std::{env, sync::Arc};

mod api;
mod commons;
mod contracts;
mod db;
mod defillama;
mod http_client;
mod ipfs;
mod scanner;
mod signer;
mod specification;
mod telemetry;

use anyhow::Context;
use diesel::{
    pg::PgConnection,
    r2d2::{ConnectionManager, Pool},
};
use tokio::task::JoinSet;

use crate::{commons::ChainExecutionContext, http_client::HttpClient};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("./migrations");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init()?;

    let alt_config_path = env::var("CONFIG_PATH").ok();
    let config = commons::get_config(alt_config_path)?;

    let db_connection_manager =
        ConnectionManager::<PgConnection>::new(&config.db_connection_string);
    let db_connection_pool = Pool::builder().build(db_connection_manager)?;
    let mut db_connection = db_connection_pool.get()?;
    db_connection.run_pending_migrations(MIGRATIONS).unwrap();

    let ipfs_api_endpoint = reqwest::Url::parse(config.ipfs_api_endpoint.as_str())
        .context(format!("could not parse url {}", config.ipfs_api_endpoint))?;
    tracing::info!("ipfs api endpoint: {}", config.ipfs_api_endpoint);
    let ipfs_http_client = Arc::new(HttpClient::new(ipfs_api_endpoint.to_owned()));

    let web3_storage_http_client = config.web3_storage_api_key.map(|token| {
        Arc::new(HttpClient::new_with_bearer_auth(
            reqwest::Url::parse("https://api.web3.storage").unwrap(), // guaranteed to be a valid url
            token,
        ))
    });

    let mut join_set = JoinSet::new();
    for (chain_id, chain_config) in config.chain_configs.into_iter() {
        let ws_rpc_endpoint = chain_config.ws_rpc_endpoint.as_str();

        tracing::info!(
            "setting up listener for chain with id {} with ws rpc endpoint: {}",
            chain_id,
            ws_rpc_endpoint
        );

        let execution_context = Arc::new(ChainExecutionContext {
            chain_id,
            ws_rpc_endpoint: Arc::new(chain_config.ws_rpc_endpoint),
            logs_blocks_range: chain_config.logs_blocks_range,
            template_id: chain_config.template_id,
            answerer_private_key: Arc::new(chain_config.answerer_private_key),
            ipfs_http_client: ipfs_http_client.clone(),
            web3_storage_http_client: web3_storage_http_client.clone(),
            db_connection_pool: db_connection_pool.clone(),
            factory_config: chain_config.factory,
        });

        join_set.spawn(scanner::scan(execution_context));
    }

    join_set.spawn(api::serve(config.api.host, config.api.port));

    // wait forever unless some task stops with an error
    while let Some(res) = join_set.join_next().await {
        let _ = res.context("task unexpectedly stopped")?;
    }

    Ok(())
}
