use alloy::primitives::Address;
use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use reqwest::Url;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    // Used as a sanity check for the RPC_URL.
    pub chain_id: u64,

    // The RPC url of the chain.
    pub rpc_url: Url,

    // The subgraph to get accounts from.
    pub subgraph_url: Url,

    // The url of the Euler swap api.
    pub swap_url: Url,

    // The evc contract address.
    pub evc_address: Address,

    // The address of the pyth contracts.
    pub pyth_address: Address,

    // The wrapped version of the native asset.
    pub wrapped_native_asset_address: Address,

    // The oracle lens contract.
    pub oracle_lens_address: Address,

    // The account lens contract.
    pub account_lens_address: Address,

    // The vault lens contract.
    pub vault_lens_address: Address,

    // The liquidator contract.
    pub liquidator_address: Address,

    // The address that should be receiving the profit from the liquidations.
    pub profit_receiver: Address,
}

pub fn get_config() -> Result<Config> {
    Ok(Figment::new()
        .merge(Toml::file("Config.toml"))
        .merge(Env::prefixed("BOT_"))
        .extract()?)
}
