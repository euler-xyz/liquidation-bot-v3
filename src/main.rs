use std::collections::HashMap;

use alloy::{
    primitives::{Address, FixedBytes, U256},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::types::error,
};
use itertools::Itertools;
use reqwest::Url;
use tokio::sync::mpsc::{self};
use tracing::info;

use crate::{
    account::watch_chain_for_accounts,
    accounts::AccountsTracker,
    config::get_config,
    lens::fetch_account,
    liquidation::prepare_liquidation,
    oracles::{OracleChange, OraclesCache, poll_oracles},
    prices::Prices,
    pyth::fetch_pyth_data,
    subgraph::{
        TrackingVaultBalancesArgs, fetch_latest_indexed_block, fetch_tracking_vault_balances,
    },
    types::{Account, VaultAssetPosition, VaultDebtPosition},
    vaults::Vaults,
};
use anyhow::{Result, anyhow};

mod account;
mod accounts;
mod config;
mod lens;
mod liquidation;
mod oracles;
mod prices;
mod pyth;
mod subgraph;
mod swap;
mod types;
mod vaults;

#[tokio::main]
async fn main() {
    // Configure tracing.
    tracing_subscriber::fmt::init();

    // Load the bot configuration.
    let config = get_config().expect("Could not load the configuration for the bot");

    // Build the provider.
    let provider = ProviderBuilder::new().connect_http(config.rpc_url);

    // Our singleton vault store.
    let vaults = &mut Vaults::new(config.vault_lens_address);

    let mut accounts = AccountsTracker::new();
    let mut prices = Prices::new();
    let mut oracles = OraclesCache::new(config.oracle_lens_address);

    // Fetch the latest indexed block.
    let starting_block = fetch_latest_indexed_block(config.subgraph_url.clone())
        .await
        .map_err(|e| anyhow!("Couldn't fetch the latest indexed block from the subgraph"))
        .unwrap();

    // Fetch all accounts that have debt.
    info!("Fetching accounts with debt at block {}", starting_block);
    let accounts_to_fetch = fetch_list_of_accounts(config.subgraph_url, starting_block)
        .await
        .unwrap();

    info!(
        "Found {} accounts, loading their assets and debts",
        accounts_to_fetch.len()
    );

    // For each account fetch all their positions in vaults.
    for account in accounts_to_fetch.iter() {
        accounts.add(
            fetch_account(
                provider.clone().erased(),
                vaults,
                config.account_lens_address,
                config.evc_address,
                *account,
            )
            .await
            .unwrap(),
        );
    }

    info!("All assets and debts have been loaded, start watching for changes.");

    let (account_events_sender, mut account_events_receiver) = mpsc::channel::<Address>(100);
    let account_provider = provider.clone();
    tokio::spawn(async move {
        watch_chain_for_accounts(
            account_provider.erased(),
            config.evc_address,
            account_events_sender,
            starting_block,
        )
        .await
    });

    let (oracles_sender, mut oracles_receiver) = mpsc::channel::<Vec<OracleChange>>(100);
    let initial_oracles = accounts.get_oracle_identifiers();
    let oracle_provider = provider.clone();
    {
        let oracles = oracles.clone();
        tokio::spawn(async move {
            poll_oracles(
                oracle_provider.erased(),
                oracles.clone(),
                initial_oracles,
                oracles_sender,
            )
            .await
            .unwrap();
        });
    }

    loop {
        tokio::select! {
            Some(oracle_updates) = oracles_receiver.recv() => {
                // Update our prices with the new ones.
                prices.update_bulk(oracle_updates.clone());

                // Figure out what accounts are affected by this change.
                let accounts_affected = accounts.get_bulk_impacted_accounts(
                    oracle_updates.iter().map(|oc| oc.oracle.clone()).collect(),
                );

                info!("Oracle price updates have occured that affect {} accounts", accounts_affected.len());

                let unhealthy_accounts: Vec<_> = accounts_affected
                    .iter()
                    // NOTE: Errors regarding missing oracles get hidden here by the `.ok()`
                    .filter(|a| {
                        match a.calculate_health(&prices) {
                            Ok(health) => {
                                health.is_unhealthy()
                            },
                            Err(err) => {
                                tracing::error!("Error while checking account health: {}", err);
                                false
                            },
                        }

                    })
                    .collect();

                let provider = &provider.clone().erased();
                for account in unhealthy_accounts.iter() {
                    // First we check if any of the oracles this account makes use of are Pyth.
                    // If so we need to fetch their most recent quotes.
                    let mut pyth_ids = Vec::new();
                    for oracle in account.dependent_on().iter() {
                        let oracle_type = oracles
                            .fetch(provider, oracle.clone())
                            .await;

                        let oracle_type = match oracle_type {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!("Issues with fetching oracle information, optimistically assuming this to not be a pyth oracle: {}", e);
                                continue;
                            }
                        };

                        oracle_type.pyth_ids()
                            .iter()
                            .for_each(|new_id| pyth_ids.push(*new_id));
                        }

                    // Fetch pyth data if needed.
                    let pyth = match !pyth_ids.is_empty() {
                        true => {
                            // Call the Pyth API to fetch the most recent data for these oracles.
                            Some(
                                fetch_pyth_data(provider, config.pyth_address, pyth_ids)
                                .await
                                .unwrap(),
                            )
                        }
                        false => None,
                    };

                    let liquidation = prepare_liquidation(provider, config.chain_id, pyth, config.wrapped_native_asset_address, config.liquidator_address, account.clone().clone()).await;
                    dbg!(liquidation);
                }

                // a.iter().for_each(|account| {
                //     info!("Account {} has become unhealthy, asset_value {}, debt {}, delta: {}", account.account, account.asset_value, account.debt_value, account.debt_value - account.asset_value);
                // });
            },
            // Track when an event happens on chain involving an account that potentially updates
            // its assets and debts, we (re)fetch the account and add it to our tracker.
            Some(account_event) = account_events_receiver.recv() => {
                // Fetch the account.
                let account = fetch_account(
                    provider.clone().erased(),
                    vaults,
                    config.account_lens_address,
                    config.evc_address,
                    account_event,
                )
                    .await
                    .unwrap();

                // Track its (new) state.
                accounts.add(account);

                info!("Received account event, now tracking account {}", account_event);
            },
        }
    }
}

pub async fn fetch_list_of_accounts(url: Url, at_block: u64) -> Result<Vec<Address>> {
    let mut rows = Vec::new();
    let mut last_id: FixedBytes<40> = FixedBytes::ZERO;

    // Fetch all rows from the subgraph.
    loop {
        let new = fetch_tracking_vault_balances(
            url.clone(),
            TrackingVaultBalancesArgs {
                id_gt: last_id,
                at_block,
            },
        )
        .await
        .map_err(|e| anyhow!("Error while fetching vault balances"))?;

        // We have reached the end.
        if new.len() < 1000 {
            rows.extend(new);
            break;
        }

        last_id = new.last().unwrap().id;
        rows.extend(new);
    }

    Ok(rows.into_iter().map(|a| a.account).collect())
}

pub async fn fetch_all_accounts(
    provider: &DynProvider,
    vaults: &mut Vaults,
    url: Url,
) -> anyhow::Result<Vec<Account>> {
    // Fetch the latest indexed block.
    let block = fetch_latest_indexed_block(url.clone())
        .await
        .map_err(|e| anyhow!("Couldn't fetch the latest indexed block from the subgraph"))?;

    let mut rows = Vec::new();
    let mut last_id: FixedBytes<40> = FixedBytes::ZERO;

    // Fetch all rows from the subgraph.
    loop {
        let new = fetch_tracking_vault_balances(
            url.clone(),
            TrackingVaultBalancesArgs {
                id_gt: last_id,
                at_block: block,
            },
        )
        .await
        .map_err(|e| anyhow!("Error while fetching vault balances"))?;

        // We have reached the end.
        if new.len() < 1000 {
            rows.extend(new);
            break;
        }

        last_id = new.last().unwrap().id;
        rows.extend(new);
    }

    // Sort the balances by account.
    let map: HashMap<_, Vec<_>> = rows.into_iter().into_group_map_by(|item| item.account);

    let mut accounts = Vec::new();
    for (account_address, balances) in map.into_iter() {
        let mut assets = Vec::new();
        let mut debts = Vec::new();

        for balance in balances.into_iter() {
            if balance.debt > U256::ZERO {
                debts.push(VaultDebtPosition {
                    amount: balance.debt,
                    vault: vaults.get_or_fetch(provider, balance.vault).await?,
                });
            }

            if balance.balance > U256::ZERO {
                assets.push(VaultAssetPosition {
                    amount: balance.balance,
                    vault: vaults.get_or_fetch(provider, balance.vault).await?,
                });
            }
        }

        accounts.push(Account {
            address: account_address,
            debt: debts,
            assets,
        });
    }

    // Fetch the current block
    Ok(accounts)
}

#[cfg(test)]
mod test {
    use alloy::{
        node_bindings::Anvil,
        primitives::address,
        providers::{Provider, ProviderBuilder},
    };

    use crate::{lens::fetch_account, liquidation::prepare_liquidation, vaults::Vaults};

    #[tokio::test]
    // The liquidation we are copying:
    // https://etherscan.io/tx/0x42533f3be6999ddeba1c3672d70c91f879ee1568ed61085293f7ff41a874a9d8
    async fn liquidation() {
        let block = 24790465;
        let violator = address!("0x65E30583c1939344d57bBCdf3A1Bbb28d41164f2");

        // Network (mainnet) specific configuration.
        let wrapped_native_asset = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));
        let liquidator_address = address!("0xAAF93d5475d092EA68a748137eE19D8130918392");

        // Fork the network at the block where this liquidation was present.
        let network = Anvil::new()
            .fork("")
            .fork_block_number(block)
            .try_spawn()
            .unwrap();

        let provider = ProviderBuilder::new()
            .connect_http(network.endpoint_url())
            .erased();

        // Fetch the account.
        let account = fetch_account(
            provider.clone(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            violator,
        )
        .await
        .unwrap();

        // TODO: have a way to mock the swap api so we can always provide the same fixed reply.
        prepare_liquidation(
            &provider,
            1,
            None, // This liquidation does not use any pyth oracles.
            wrapped_native_asset,
            liquidator_address,
            account,
        )
        .await
        .unwrap();
    }
}
