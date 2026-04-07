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
    pub rpc_url: Url,
    pub subgraph_url: Url,
    pub evc_address: Address,
    pub oracle_lens_address: Address,
    pub vault_lens_address: Address,
    pub account_lens_address: Address,
}

pub fn get_config() -> Result<Config> {
    Ok(Figment::new()
        .merge(Toml::file("Config.toml"))
        .merge(Env::prefixed("BOT_"))
        .extract()?)
}
