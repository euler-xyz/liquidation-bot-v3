use alloy::{primitives::Address, signers::local::PrivateKeySigner};
use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use reqwest::Url;
use serde::Deserialize;

#[derive(Deserialize, Clone, Default)]
pub enum VaultFilterMode {
    #[default]
    None,
    Whitelist,
    Blacklist,
}

#[derive(Deserialize, Clone, Default)]
pub struct VaultFilter {
    pub mode: VaultFilterMode,
    pub items: Vec<Address>,
}

impl VaultFilter {
    /// If the vault should be filtered out.
    pub fn should_filter(&self, vault: Address) -> bool {
        match self.mode {
            VaultFilterMode::None => false,
            VaultFilterMode::Whitelist => !self.items.contains(&vault),
            VaultFilterMode::Blacklist => self.items.contains(&vault),
        }
    }
}

#[derive(Deserialize, Clone)]
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

    // The address of the swapper contract.
    pub swapper_address: Address,

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

    // The public address of the EOA, used as a sanity check.
    pub eoa_address: Address,

    // The private of the EOA that will perform the liquidations.
    pub eoa_private_key: String,

    // The address that should be receiving the profit from the liquidations.
    pub profit_receiver: Address,

    // At what interval should we poll the oracles to check for pricing changes.
    pub oracle_polling_interval_seconds: u64,

    // At what interval should we re-sync all accounts and check their health.
    pub full_resync_and_check_interval_seconds: u64,

    #[serde(default)]
    // Lets the config specify in what mode the filter is operating and what to filter.
    pub vault_filter: VaultFilter,
}

pub fn get_config() -> Result<Config> {
    Ok(Figment::new()
        .merge(Toml::file("Config.toml"))
        .merge(Env::prefixed("BOT_"))
        .extract()?)
}
