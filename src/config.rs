use alloy::{
    primitives::Address,
    providers::{Provider, ProviderBuilder},
};
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

    // The url of the Euler pricing api.
    pub pricing_url: Url,

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

    // If enabled we will be forking the chain and processing the liquidations on the fork.
    #[serde(default)]
    pub simulation_mode: bool,

    #[serde(default)]
    // Lets the config specify in what mode the filter is operating and what to filter.
    pub vault_filter: VaultFilter,

    // Observability settings
    #[serde(default)]
    pub enable_observability_api: bool,
}

impl Config {
    /// Attempts to ensure the config is valid
    pub async fn validate_config(&self) -> Result<()> {
        // Build the provider.
        let provider = ProviderBuilder::new().connect_http(self.rpc_url.clone());

        // Get the chain id, this both ensures the RPC url is valid and we check that its the
        // correct RPC for this config.
        let chain_id = provider.get_chain_id().await?;
        if chain_id != self.chain_id {
            anyhow::bail!(
                "The configured RPC reports a chain_id ({}) that is not the same as the chain_id in the configuration file ({})",
                chain_id,
                self.chain_id
            );
        }

        // Perform a sanity check on all contracts that are part of the configuration.
        check_address(&provider, self.evc_address, "EVC").await?;
        check_address(&provider, self.pyth_address, "Pyth").await?;
        check_address(&provider, self.swapper_address, "swapper").await?;
        check_address(
            &provider,
            self.wrapped_native_asset_address,
            "wrapped native asset",
        )
        .await?;
        check_address(&provider, self.oracle_lens_address, "oracle lens").await?;
        check_address(&provider, self.account_lens_address, "account lens").await?;
        check_address(&provider, self.vault_lens_address, "vault lens").await?;
        check_address(&provider, self.liquidator_address, "liquidator").await?;

        Ok(())
    }
}

async fn check_address<P: Provider>(provider: &P, address: Address, label: &str) -> Result<()> {
    if provider.get_code_at(address).await?.is_empty() {
        let chain = provider.get_chain_id().await?;
        anyhow::bail!(
            "The {} address ({}) for this chain ({}) does not contain any bytecode, this is a misconfiguration",
            label,
            address,
            chain
        );
    }

    Ok(())
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

#[cfg(test)]
mod test {
    use std::{env, path::Path, str::FromStr};

    use alloy::signers::local::PrivateKeySigner;

    use crate::config::get_config;

    #[tokio::test]
    /// Validates the configuration files against public rpcs.
    async fn validate_configuration_files() {
        env::set_current_dir(Path::new("./configs")).expect("failed to cd into ./configs");

        validate_configuration_file("https://eth.drpc.org", 1).await;
        validate_configuration_file("https://bsc.drpc.org", 56).await;
        validate_configuration_file("https://unichain.drpc.org", 130).await;
        validate_configuration_file("https://rpc4.monad.xyz", 143).await;
        validate_configuration_file("https://sonic.drpc.org", 146).await;
        validate_configuration_file("https://base.drpc.org", 8453).await;
        validate_configuration_file("https://plasma.drpc.org", 9745).await;
        validate_configuration_file("https://arbitrum.drpc.org", 42161).await;
        validate_configuration_file("https://avalanche.drpc.org", 43114).await;
        validate_configuration_file("https://linea.drpc.org", 59144).await;
        validate_configuration_file("https://berachain.drpc.org", 80094).await;
    }

    async fn validate_configuration_file(rpc_url: &str, chain_id: u64) {
        // Generate an EOA wallet.
        // NOTE: This is not a private key that is ever used, it holds no funds, it is just a
        // placeholder here to pass some checks about the validity of the configuration file.
        let private_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let public_address = PrivateKeySigner::from_str(private_key)
            .unwrap()
            .address()
            .to_string();

        // We need to set some environment variables to act like the production environment.
        unsafe {
            env::set_var("CHAIN_ID", chain_id.to_string());
            env::set_var(format!("RPC_URL_{}", chain_id), rpc_url);
            env::set_var("SUBGRAPH_URL_PREFIX", "https://mock-subgraph-url.com/");
            env::set_var("EOA_ADDRESS", public_address);
            env::set_var("EOA_PRIVATE_KEY", private_key);
        }

        let config = get_config().unwrap();
        config.validate_config().await.unwrap();
    }
}
