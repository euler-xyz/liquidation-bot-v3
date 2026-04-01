use alloy::{
    primitives::address,
    providers::{Provider, ProviderBuilder},
};

use crate::{config::get_config, lens::fetch_account, vaults::Vaults};

mod accounts;
mod config;
mod lens;
mod subgraph;
mod types;
mod vaults;

#[tokio::main]
async fn main() {
    // Load the bot configuration.
    let config = get_config().expect("Could not load the configuration for the bot");

    // Build the provider.
    let provider = ProviderBuilder::new().connect_http(config.rpc_url);

    // Our singleton vault store.
    let vaults = &mut Vaults::new(config.vault_lens_address);

    dbg!(
        fetch_account(
            provider.erased(),
            vaults,
            config.account_lens_address,
            config.evc_address,
            address!("0x5DaC9ccC215b9aF65b486066786F79d9aA0043DB"),
        )
        .await
    );
}
