use std::collections::HashMap;

use alloy::{
    primitives::{Address, FixedBytes, U256},
    providers::{DynProvider, Provider, ProviderBuilder},
};
use itertools::Itertools;
use reqwest::Url;
use tokio::sync::mpsc::{self};

use crate::{
    accounts::AccountsTracker,
    config::get_config,
    lens::fetch_account,
    oracles::{OracleChange, poll_oracles},
    prices::Prices,
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
mod oracles;
mod prices;
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

    let mut accounts = AccountsTracker::new();
    let mut prices = Prices::new();

    // Fetch all accounts that have debt.
    let accounts_to_fetch = fetch_list_of_accounts(config.subgraph_url).await.unwrap();

    // For each account fetch all their positions in vaults.
    for account in accounts_to_fetch.iter().take(50) {
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

    let (s, mut r) = mpsc::channel::<Vec<OracleChange>>(100);

    let initial_oracles = accounts.get_oracle_identifiers();
    tokio::spawn(async move {
        poll_oracles(provider.erased(), initial_oracles, s)
            .await
            .unwrap();
    });

    loop {
        if let Some(oracle_updates) = r.recv().await {
            // Update our prices with the new ones.
            prices.update_bulk(oracle_updates.clone());

            // Figure out what accounts are affected by this change.
            let accounts_affected = accounts.get_bulk_impacted_accounts(
                oracle_updates.iter().map(|oc| oc.oracle.clone()).collect(),
            );

            dbg!(accounts_affected.len());

            let a: Vec<_> = accounts_affected
                .iter()
                // NOTE: Errors regarding missing oracles get hidden here by the `.ok()`
                .flat_map(|a| a.calculate_health(&prices).ok())
                .filter(|solvency| solvency.is_unhealthy())
                .collect();

            dbg!(a);

            // Calculate the asset and debt values.
        }
    }

    // dbg!(accounts.get_impacted_accounts(&OracleIdentifier {
    //     base_asset: address!("0x4200000000000000000000000000000000000006"),
    //     quote_asset: address!("0x0000000000000000000000000000000000000348"),
    //     adapter: address!("0x6e183458600e66047a0f4d356d9daa480da1ca59")
    // }));
}

pub async fn fetch_list_of_accounts(url: Url) -> Result<Vec<Address>> {
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
                dbg!("found assets");
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
