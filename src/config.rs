use alloy::primitives::Address;
use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
    util::map,
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
    pub subgraph_url_prefix: String,

    // The subgraph to get accounts from.
    pub subgraph_url_path: String,

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
    // Fetch what chain id we should be loading the config for.
    let chain_id = std::env::var("CHAIN_ID")?;

    // Fetch the RPC url for this chain id.
    let rpc = match std::env::var(format!("RPC_URL_{}", chain_id)) {
        Ok(url) => map!["rpc_url" => url],
        Err(_) => map![],
    };

    // The file that will be used as the config file.
    let config_file = format!("Config.{}.toml", chain_id);

    let config: Config = Figment::new()
        .merge(figment::providers::Serialized::from(&rpc, "default"))
        .merge(Toml::file(config_file))
        .merge(Env::raw())
        .extract()?;

    // Do a sanity check on the sugraph URL to make sure the two parts form a url.
    Url::parse(&config.subgraph_url_prefix)?.join(&config.subgraph_url_path)?;

    Ok(config)
}
