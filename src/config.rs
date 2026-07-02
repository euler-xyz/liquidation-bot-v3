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

use crate::{config, liquidation::Liquidator};

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

    // The RPC url to use to send transactions, mostly used for sending to MEV protected RPCs.
    pub transaction_rpc_url: Option<Url>,

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

    pub pyth: Option<PythConfig>,

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

#[derive(Deserialize, Clone, Debug)]
pub struct PythConfig {
    pub address: Address,
    pub endpoint: String,
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

        // Make sure the transaction RPC if set, is also valid and that its for the correct chain.
        if let Some(transaction_rpc_url) = self.transaction_rpc_url.clone() {
            let transaction_provider = ProviderBuilder::new().connect_http(transaction_rpc_url);
            let transaction_provider_chain_id = transaction_provider.get_chain_id().await?;

            if transaction_provider_chain_id != chain_id {
                anyhow::bail!(
                    "The configured Transaction RPC reports a chain_id ({}) that is not the same as the chain_id in the configuration file ({})",
                    chain_id,
                    self.chain_id
                );
            }
        }

        // Perform a sanity check on all contracts that are part of the configuration.
        check_address(&provider, self.evc_address, "EVC").await?;

        // Not all chains (specifically TAC) have a pyth deployment.
        if let Some(pyth) = &self.pyth {
            // TODO: Check endpoint.
            check_address(&provider, pyth.address, "Pyth").await?;

            // Check that the pyth endpoints ends with a `/`.
            if !pyth.endpoint.ends_with("/") {
                anyhow::bail!(
                    "The configured endpoint for Pyth is either incorrect or is missing a trailing '/', the URL that is configured is {}",
                    pyth.endpoint.clone()
                );
            }
        }

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

        // Ensure the liquidator is configured correctly.
        let liquidator = Liquidator::new(self.liquidator_address, provider);
        let liquidator_swapper = liquidator.swapperAddress().call().await?;

        if liquidator_swapper != self.swapper_address {
            anyhow::bail!(
                "On chain {} the swapper address configured for the liquidator is different than the one that is configured for the bot, this is a misconfiguration. contract={}, bot={}",
                self.chain_id,
                liquidator_swapper,
                self.swapper_address
            );
        }

        let liquidator_evc = liquidator.evcAddress().call().await?;
        if liquidator_evc != self.evc_address {
            anyhow::bail!(
                "On chain {} the evc address configured for the liquidator is different than the one that is configured for the bot, this is a misconfiguration. contract={}, bot={}",
                self.chain_id,
                liquidator_evc,
                self.swapper_address
            );
        }

        let liquidator_pyth = liquidator.PYTH().call().await?;
        match (liquidator_pyth, self.pyth.clone()) {
            (pyth, Some(local_pyth)) if pyth == local_pyth.address => {}
            (pyth, None) if pyth == Address::ZERO => {}
            _ => {
                anyhow::bail!(
                    "On chain {} the pyth address configured for the liquidator is different than the one that is configured for the bot, this is a misconfiguration. contract={}, bot={:?}",
                    self.chain_id,
                    liquidator_pyth,
                    self.pyth
                );
            }
        };

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

pub fn get_config(config_folder_path: Option<String>) -> Result<Config> {
    // Fetch what chain id we should be loading the config for.
    let chain_id = std::env::var("CHAIN_ID")?;

    // Fetch the RPC url for this chain id.
    let rpc = match std::env::var(format!("RPC_URL_{}", chain_id)) {
        Ok(url) => map!["rpc_url" => url],
        Err(_) => map![],
    };

    // The file that will be used as the config file.
    let config_file = if let Some(path) = config_folder_path {
        format!("{}/Config.{}.toml", path, chain_id)
    } else {
        format!("Config.{}.toml", chain_id)
    };

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
pub fn load_configuration_file_for_test(rpc_url: &str, chain_id: u64) -> anyhow::Result<Config> {
    use crate::config::get_config;
    use alloy::signers::local::PrivateKeySigner;
    use std::{env, str::FromStr};

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

    get_config(Some("./configs/".to_string()))
}

#[cfg(test)]
mod test {
    use crate::config::load_configuration_file_for_test;

    #[tokio::test]
    /// Validates the configuration files against public rpcs.
    async fn validate_configuration_files() {
        validate_configuration_file("https://eth.blockrazor.xyz", 1).await;
        validate_configuration_file("https://binance-smart-chain-public.nodies.app", 56).await;
        validate_configuration_file("https://unichain-rpc.publicnode.com", 130).await;
        validate_configuration_file("https://rpc4.monad.xyz", 143).await;
        validate_configuration_file("https://rpc.soniclabs.com", 146).await;
        validate_configuration_file("https://rpc.tac.build", 239).await;
        validate_configuration_file("https://rpc.hypurrscan.io", 999).await;
        validate_configuration_file("https://base.api.pocket.network", 8453).await;
        validate_configuration_file("https://rpc.plasma.to", 9745).await;
        validate_configuration_file("https://public-arb-mainnet.fastnode.io", 42161).await;
        validate_configuration_file("https://avalanche-c-chain-rpc.publicnode.com", 43114).await;
        validate_configuration_file("https://linea.rpc.sentio.xyz", 59144).await;
        validate_configuration_file("https://rpc.gobob.xyz", 60808).await;
        validate_configuration_file("https://rpc.berachain.com", 80094).await;
    }

    async fn validate_configuration_file(rpc_url: &str, chain_id: u64) {
        println!("Validating {}", chain_id);
        load_configuration_file_for_test(rpc_url, chain_id)
            .unwrap()
            .validate_config()
            .await
            .unwrap();
    }
}
