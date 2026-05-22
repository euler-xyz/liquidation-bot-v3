#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]

use alloy::{
    node_bindings::Anvil,
    primitives::{Address, FixedBytes, U256},
    providers::{DynProvider, Provider, ProviderBuilder, ext::AnvilApi},
    signers::local::PrivateKeySigner,
};
use itertools::Itertools;
use reqwest::Url;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    account::watch_chain_for_accounts_from_latest,
    accounts::AccountsTracker,
    api::{BotHealth, BotState, serve},
    config::{Config, get_config},
    lens::fetch_account,
    liquidation::{PreparedLiquidation, prepare_liquidation},
    oracles::{OracleChange, OraclesCache, poll_oracles},
    prices::EulerPricingApi,
    pyth::fetch_pyth_data,
    subgraph::{
        TrackingVaultBalancesArgs, fetch_latest_indexed_block, fetch_tracking_vault_balances,
    },
    swap::{EulerSwapApi, SwapQuoteProvider},
    transactions::execute_liquidation_queue,
    types::{
        Account, LiquidationReasoning, LiquidationReasoningError, VaultBorrowPosition,
        VaultCollateralPosition,
    },
    vaults::Vaults,
};
use anyhow::{Result, anyhow};

mod account;
mod accounts;
mod api;
mod config;
mod lens;
mod liquidation;
mod oracles;
mod prices;
mod pyth;
mod subgraph;
mod swap;
mod transactions;
mod types;
mod vaults;

#[tokio::main]
async fn main() {
    // Configure tracing.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("warn,liquidation_bot_v3=info"))
        .init();

    // Load the bot configuration.
    let config = match get_config() {
        Ok(config) => config,
        Err(err) => {
            error!("Could not load the configuration for the bot, due to err: {err}");
            return;
        }
    };

    // Sanity check the configuration file.
    match config.validate_config().await {
        Ok(_) => {
            info!("Configuration for the chain has been validated successfully");
        }
        Err(err) => {
            error!("Issue while validating configuration of the chain: {err}");
            return;
        }
    }

    // Construct the signer.
    let pk_signer: PrivateKeySigner = match config.eoa_private_key.parse::<PrivateKeySigner>() {
        Ok(signer) => {
            // Do the sanity check and make sure the private key matches the public address.
            if signer.address() != config.eoa_address {
                error!(
                    pk_address =? signer.address(),
                    pb_address =? config.eoa_address,
                    "Configured EOA private key and configured EOA address do not match."
                );
                return;
            }

            signer
        }
        Err(_) => {
            error!("Could not turn the eoa_private_key into a signer.");
            return;
        }
    };

    // Build the provider.
    let provider = ProviderBuilder::new().connect_http(config.rpc_url.clone());

    // Do a sanity check that the configured chain id is also the chain id of the provider.
    match provider.get_chain_id().await {
        Ok(id) if id == config.chain_id => {
            // Valid!
        }
        Ok(id) => {
            error!(
                "The configured RPC for chain id {} is invalid, it is a RPC for {}",
                config.chain_id, id
            );
            return;
        }
        Err(err) => {
            error!(
                "Could not fetch the chain id for {} when attempting to do a sanity check, err: {:?}",
                config.chain_id, err
            );
            return;
        }
    };

    // Our singleton vault store.
    let vaults = Vaults::new(config.vault_lens_address);
    let accounts = Arc::new(AccountsTracker::new());
    let oracles = OraclesCache::new(config.oracle_lens_address, config.pyth_address);

    let (account_events_sender, account_events_receiver) = mpsc::channel::<Address>(100);
    let account_provider = provider.clone();
    tokio::spawn(async move {
        watch_chain_for_accounts_from_latest(
            account_provider.erased(),
            config.evc_address,
            account_events_sender,
        )
        .await
    });

    let (oracles_sender, oracles_receiver) = mpsc::channel::<Vec<OracleChange>>(100);
    let oracle_provider = provider.clone();
    {
        let oracles = oracles.clone();
        tokio::spawn(async move {
            let _ = poll_oracles(
                oracle_provider.erased(),
                oracles.clone(),
                tokio::time::Duration::from_secs(config.oracle_polling_interval_seconds),
                oracles_sender,
            )
            .await
            .inspect_err(|e| {
                error!(
                    "Polling of oracles had a critical error, it is no longer operating. err: {:?}",
                    e
                )
            });
        });
    }

    let (liquidation_sender, liquidation_receiver) = mpsc::channel::<PreparedLiquidation>(100);

    // If the config specifies we should be running in simulation mode then we configure an anvil
    // fork for transaction settlement, otherwise we use the mainnet rpc.
    let (liquidation_provider, _network) = match config.simulation_mode {
        true => {
            info!("Running in simulation mode, all transactions will be settled on an anvil fork");
            let network = match Anvil::new().fork(config.rpc_url.clone()).try_spawn() {
                Ok(network) => network,
                Err(err) => {
                    error!("Could not fork the chain, err: {:?}", err);
                    return;
                }
            };

            let provider = ProviderBuilder::new()
                .wallet(pk_signer)
                .connect_http(network.endpoint_url());

            // Fund the EOA wallet.
            let _ = provider
                .anvil_set_balance(config.eoa_address, U256::MAX)
                .await;

            (provider, Some(network))
        }
        false => (
            ProviderBuilder::new()
                .wallet(pk_signer)
                .connect_http(config.rpc_url.clone()),
            None,
        ),
    };

    let profit_receiver = config.profit_receiver;
    tokio::spawn(async move {
        execute_liquidation_queue(liquidation_provider, liquidation_receiver, profit_receiver).await
    });

    let (tx, rx) = tokio::sync::watch::channel(BotHealth::Syncing);

    if config.enable_observability_api {
        // Start the observability api.
        let state = BotState {
            accounts: accounts.clone(),
            oracles: oracles.clone(),
            state: rx,
        };

        tokio::spawn(async move {
            serve(state).await;
        });
    }

    let swap_provider = EulerSwapApi::new(
        config.swap_url.clone(),
        provider.clone().erased(),
        config.chain_id,
        config.profit_receiver,
        config.eoa_address,
        config.swapper_address,
        config.wrapped_native_asset_address,
        // TODO: Move to config.
        "1", // Max slippage.
        EulerPricingApi::new(config.pricing_url.clone(), config.chain_id),
    );

    // Start the liquidation bot.
    run(
        config,
        &provider.erased(),
        accounts,
        vaults,
        oracles,
        account_events_receiver,
        oracles_receiver,
        liquidation_sender,
        &swap_provider,
        Some(tx),
    )
    .await;
}

/// This is the main loop of the liquidation bot.
pub async fn run(
    config: Config,
    provider: &DynProvider,

    accounts: Arc<AccountsTracker>,
    mut vaults: Vaults,
    oracles: OraclesCache,

    // Channels for communicating with the other threads.
    mut account_update_channel: Receiver<Address>,
    mut oracle_update_channel: Receiver<Vec<OracleChange>>,
    liquidation_channel: Sender<PreparedLiquidation>,
    swap_provider: &impl SwapQuoteProvider,
    state: Option<tokio::sync::watch::Sender<BotHealth>>,
) {
    let mut resync_interval = tokio::time::interval(tokio::time::Duration::from_secs(
        config.full_resync_and_check_interval_seconds,
    ));

    // TODO: Keep track of accounts for which the liquidation is currently being processed or was
    // already processed but not yet updated. Otherwise unlucky timing of a price update might cause
    // us to attempt to liquidate it twice.

    loop {
        tokio::select! {
            _ = resync_interval.tick() => {
                info!("Syncing all accounts and checking the health for each of them.");

                let unhealthy_accounts = match refresh_and_check_all(provider, config.clone(), &accounts, &mut vaults, &oracles).await {
                    Ok(unhealthy_accounts) => unhealthy_accounts,
                    Err(e) => {
                        tracing::error!("Error while refreshing, err:{:?}", e);
                        continue;
                    }
                };

                // Signal that the bot is healthy and finished syncing.
                if let Some(ref state_sender) = state {
                    let _ = state_sender.send(BotHealth::Healthy).inspect_err(|e| {
                        tracing::warn!("Could not signal healthy state due to error with sender, err: {:?}", e);
                    });
                }

                let number_of_unhealthy = unhealthy_accounts.len();
                info!("While resyncing we found {} accounts that are unhealthy, now checking which we can/should liquidate..", number_of_unhealthy);

                // Turn the unhealthy accounts into prepared liquidations.
                let liquidations = prepare_liquidations(provider, swap_provider, &config, &oracles, unhealthy_accounts).await.inspect_err(|e| {
                    tracing::error!("Error preparing liquidations, could not prepare any liquidations because of it, err: {:?}", e);
                }).unwrap_or_default();

                if number_of_unhealthy != 0 || !liquidations.is_empty() {
                    info!("Found {} accounts that are unhealthy, for which we are going to perform a liquidation for {} of them", number_of_unhealthy, liquidations.len());
                }

                // Send it to the liquidations thread to handle.
                for liquidation in liquidations.into_iter() {
                    let _ = liquidation_channel.send(liquidation).await;
                }
            }
            Some(oracle_updates) = oracle_update_channel.recv() => {
                // Figure out what accounts are affected by this change.
                let accounts_affected = accounts.get_bulk_impacted_accounts(
                    oracle_updates.iter().map(|oc| oc.oracle.clone()).collect(),
                );

                info!("Oracle price updates have occured that affect {} accounts", accounts_affected.len());

                let unhealthy_accounts: Vec<_> = accounts_affected
                    .iter()
                    .filter(|a| {
                        match a.calculate_health(&oracles) {
                            Ok(health) => {
                                // Update the accounts and mark them as healthy if they are.
                                if health.is_healthy() {
                                    a.set_status(LiquidationReasoning::Healthy);
                                }

                                health.is_unhealthy()
                            },
                            Err(err) => {
                                tracing::error!("Error while checking account health: {}", err);
                                a.set_status(LiquidationReasoning::Error(
                                        types::LiquidationReasoningError::OracleError {
                                            message: err.to_string()
                                        }
                                    )
                                );

                                false
                            },
                        }

                    })
                    .cloned()
                    .collect();

                let number_of_unhealthy = unhealthy_accounts.len();

                // Turn the unhealthy accounts into prepared liquidations.
                let liquidations = prepare_liquidations(provider, swap_provider, &config, &oracles, unhealthy_accounts).await.inspect_err(|e| {
                    tracing::error!("Error preparing liquidations, could not prepare any liquidations because of it, err: {e}");
                }).unwrap_or_default();

                if number_of_unhealthy != 0 || !liquidations.is_empty() {
                    info!("Found {} accounts that are unhealthy, for which we are going to perform a liquidation for {} of them", number_of_unhealthy, liquidations.len());
                }

                // Send it to the liquidations thread to handle.
                for liquidation in liquidations.into_iter() {
                    let _ = liquidation_channel.send(liquidation).await;
                }
            },
            // Track when an event happens on chain involving an account that potentially updates
            // its collaterals and borrows, we (re)fetch the account and add it to our tracker.
            Some(account_event) = account_update_channel.recv() => {
                // Fetch the account.
                match fetch_account(
                    provider.clone().erased(),
                    &config.vault_filter,
                    &mut vaults,
                    config.account_lens_address,
                    config.evc_address,
                    account_event,
                )
                    .await {
                        Ok(account) => {
                            // Track its (new) state.
                            accounts.add(account);
                        },
                        Err(lens::FetchAccountError::FilteredOut(vault)) => {
                            // NOTE: Should we delete the account from the index if it was already
                            // in there? That *shouldn't* be possible but would be a strange edge-case
                            // if it was somehow in there.
                            tracing::debug!("Account {} was not indexed due to it being filtered out by the vault filter for vault {}", account_event, vault);
                        },
                        Err(lens::FetchAccountError::Other(e)) => {
                            tracing::error!("Issue while fetching new account after finding an event onchain, err: {:?}", e);
                        },
                    }


                info!("Received account event, now tracking account {}", account_event);
            },
        }
    }
}

pub async fn prepare_liquidations(
    provider: &DynProvider,
    swap_provider: &impl SwapQuoteProvider,
    config: &Config,
    oracles: &OraclesCache,
    unhealthy_accounts: Vec<Account>,
) -> Result<Vec<PreparedLiquidation>> {
    let mut prepared = Vec::new();
    for account in unhealthy_accounts.iter() {
        // First we check if any of the oracles this account makes use of are Pyth.
        // If so we need to fetch their most recent quotes.
        let mut pyth_ids = Vec::new();
        for oracle in account.dependent_on().iter() {
            let oracle_type = oracles.fetch_type(provider, oracle.clone()).await;

            let oracle_type = match oracle_type {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        "Issues with fetching oracle information, optimistically assuming this to not be a pyth oracle: {}",
                        e
                    );
                    continue;
                }
            };

            oracle_type
                .pyth_ids()
                .iter()
                .for_each(|new_id| pyth_ids.push(*new_id));
        }

        // Fetch pyth data if needed.
        let pyth = match (!pyth_ids.is_empty(), config.pyth_address) {
            (true, Some(pyth)) => {
                match fetch_pyth_data(provider, pyth, pyth_ids).await {
                    Ok(data) => Some(data),
                    Err(e) => {
                        // We log the error and then skip this liquidation as we need to attempt to
                        // process other liquidations.
                        tracing::error!(
                            "Could not fetch pyth data as we were attempting a liquidation, err: {:?}",
                            e
                        );

                        account.set_status(LiquidationReasoning::Error(
                            LiquidationReasoningError::OracleError {
                                message: "Unable to fetch Pyth data".to_string(),
                            },
                        ));
                        continue;
                    }
                }
            }
            (false, _) => None,
            // Somehow this account its position uses pyth oracle but pyth is not configured for
            // this chain, this is a critical error.
            (true, None) => {
                account.set_status(LiquidationReasoning::Error(
                    LiquidationReasoningError::OracleError {
                        message: "Unable to fetch Pyth data".to_string(),
                    },
                ));

                return Err(anyhow!(
                    "This account requires us to update pyth oracles, but there is no pyth deployment configured for this chain"
                ));
            }
        };

        debug!("Checking liquidation for {}", account.address);

        match prepare_liquidation(
            provider,
            swap_provider,
            pyth,
            config.liquidator_address,
            account.clone().clone(),
        )
        .await
        {
            Ok(Some(liquidation)) => {
                debug!("Found route to liquidate {}", account.address);
                prepared.push(liquidation)
            }
            Ok(None) => {
                account.set_status(LiquidationReasoning::NoSwapPath);
                debug!(
                    "Was not able to find a route to liquidate {}",
                    account.address
                );
            }
            Err(e) => {
                account.set_status(LiquidationReasoning::Error(
                    LiquidationReasoningError::Other {
                        message: "Could not prepare the liquidation".to_string(),
                    },
                ));

                tracing::error!(
                    account =? account.address,
                    "Issue when attempting to liquidate account, err: {:?}",
                    e
                )
            }
        }
    }

    Ok(prepared)
}

pub async fn refresh_and_check_all(
    provider: &DynProvider,
    config: Config,
    accounts: &Arc<AccountsTracker>,
    vaults: &mut Vaults,
    oracles: &OraclesCache,
) -> Result<Vec<Account>> {
    let subgraph_url = Url::parse(&config.subgraph_url_prefix)?.join(&config.subgraph_url_path)?;
    let provider_latest_block = provider.get_block_number().await?;

    // Fetch the latest indexed block.
    let starting_block = fetch_latest_indexed_block(subgraph_url.clone())
        .await
        .map_err(|e| {
            anyhow!(
                "Couldn't fetch the latest indexed block from the subgraph: {:?}",
                e
            )
        })?;

    // As a sanity check we report if the indexer is running out of sync with the chain.
    // This does not cause any issues on our side as long as we have been watching the chain
    // ourselves for new accounts.
    if provider_latest_block > starting_block && provider_latest_block - starting_block > 30 {
        warn!(
            provider = provider_latest_block,
            indexer = starting_block,
            "Indexer is likely out of sync, it is {} blocks behind the rpc.",
            provider_latest_block - starting_block
        );
    }

    // fetch all accounts from the subgraph.
    let accounts_to_fetch = fetch_list_of_accounts(subgraph_url, starting_block).await?;

    info!("Re-syncing {} accounts", accounts_to_fetch.len());

    // For each account fetch all their positions in vaults.
    // We do this as a seperate step as this also filters out accounts that are not relevant.
    for account in accounts_to_fetch.iter() {
        debug!("Loading {}", account);

        match fetch_account(
            provider.clone().erased(),
            &config.vault_filter,
            vaults,
            config.account_lens_address,
            config.evc_address,
            *account,
        )
        .await
        {
            Ok(account) => {
                // Track its (new) state.
                accounts.add(account);
            }
            Err(lens::FetchAccountError::FilteredOut(vault)) => {
                // NOTE: Should we delete the account from the index if it was already
                // in there? That *shouldn't* be possible but would be a strange edge-case
                // if it was somehow in there.
                tracing::debug!(
                    "Account {} was not indexed due to it being filtered out by the vault filter for vault {}",
                    *account,
                    vault
                );
            }
            Err(lens::FetchAccountError::Other(e)) => {
                tracing::warn!("Issue while fetching account during re-sync, err: {:?}", e);
            }
        }
    }

    // Attempt to ensure we have all prices we will need for the healthcheck.
    oracles
        .ensure_prices_for(provider, accounts.get_oracle_identifiers())
        .await;

    // Healthcheck all of the accounts, return the ones that are not healthy.
    Ok(accounts
        .all_accounts()
        .iter()
        .filter(|a| match a.calculate_health(oracles) {
            Ok(health) => {
                // Update the accounts and mark them as healthy if they are.
                if health.is_healthy() {
                    a.set_status(LiquidationReasoning::Healthy);
                }

                health.is_unhealthy()
            }
            Err(err) => {
                tracing::error!("Error while checking account health: {}", err);
                a.set_status(LiquidationReasoning::Error(
                    types::LiquidationReasoningError::OracleError {
                        message: err.to_string(),
                    },
                ));

                false
            }
        })
        .cloned()
        .collect())
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
        .map_err(|e| anyhow!("Error while fetching vault balances, err: {:?}", e))?;

        // We have reached the end.
        if new.len() < 1000 {
            rows.extend(new);
            break;
        }

        last_id = match new.last() {
            Some(last) => last.id,
            None => break,
        };

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
    let block = fetch_latest_indexed_block(url.clone()).await.map_err(|e| {
        anyhow!(
            "Couldn't fetch the latest indexed block from the subgraph, err: {:?}",
            e
        )
    })?;

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
        .map_err(|e| anyhow!("Error while fetching vault balances, err: {:?}", e))?;

        // We have reached the end.
        if new.len() < 1000 {
            rows.extend(new);
            break;
        }

        last_id = match new.last() {
            Some(last) => last.id,
            None => break,
        };

        rows.extend(new);
    }

    // Sort the balances by account.
    let map: HashMap<_, Vec<_>> = rows.into_iter().into_group_map_by(|item| item.account);

    let mut accounts = Vec::new();
    for (account_address, balances) in map.into_iter() {
        let mut collaterals = Vec::new();
        let mut borrows = Vec::new();

        for balance in balances.into_iter() {
            if balance.debt > U256::ZERO {
                borrows.push(VaultBorrowPosition {
                    amount: balance.debt,
                    vault: vaults.get_or_fetch(provider, balance.vault).await?,
                });
            }

            if balance.balance > U256::ZERO {
                collaterals.push(VaultCollateralPosition {
                    amount: balance.balance,
                    vault: vaults.get_or_fetch(provider, balance.vault).await?,
                });
            }
        }

        accounts.push(Account::new(account_address, borrows, collaterals));
    }

    // Fetch the current block
    Ok(accounts)
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use crate::{
        config::VaultFilter,
        lens::fetch_account,
        liquidation::{PreparedLiquidation, prepare_liquidation},
        prices::EulerPricingApi,
        swap::{EulerSwapApi, MulticallItem, SwapPayload, SwapQuoteProvider},
        transactions::execute_liquidation_queue,
        vaults::Vaults,
    };
    use alloy::{
        node_bindings::Anvil,
        primitives::{U256, address, bytes},
        providers::{Provider, ProviderBuilder, ext::AnvilApi},
    };
    use tokio::sync::mpsc;

    struct MockSwapProvider;

    impl SwapQuoteProvider for MockSwapProvider {
        async fn find_swap(
            &self,
            liq: PreparedLiquidation,
        ) -> anyhow::Result<Option<PreparedLiquidation>> {
            let liq = liq.with_swap_data(
                Some(SwapPayload {
                    // This data is form the actual on-chain liquidation.
                    multicall_items: [
                        bytes!("0xf71679d0000000000000000000000000000000000000000000000000000000000000002047656e657269630000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002bba09866b6f1025258542478c39720a09b728bf000000000000000000000000076bda095a434a7b00733115a0d679de6478d9f8000000000000000000000000e868084cf08f3c3db11f4b73a95473762d9463f70000000000000000000000000f93f35c0664a6a8231ccae7e22f652c9c075b320000000000000000000000002bba09866b6f1025258542478c39720a09b728bf0000000000000000000000002bba09866b6f1025258542478c39720a09b728bf000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000006352a56caadc4f1e25cd6c75970fa768a3304e640000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000098490411a320000000000000000000000007baa298d36fe21df2f6b54510da76445661a91ed000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000001c0000000000000000000000000076bda095a434a7b00733115a0d679de6478d9f8000000000000000000000000e868084cf08f3c3db11f4b73a95473762d9463f70000000000000000000000007baa298d36fe21df2f6b54510da76445661a91ed0000000000000000000000002bba09866b6f1025258542478c39720a09b728bf0000000000000000000000000000000000000000000000018fbcac6b00d170000000000000000000000000000000000000000000000000018bbd58c617d995470000000000000000000000000000000000000000000000018fbcac6b00d170000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000cad001c30e96765ac90307669d578219d4fb1dce000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000001a00000000000000000000000000000000000000000000000000000000000000420000000000000000000000000000000000000000000000000000000000000054000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000064eb5625d9000000000000000000000000076bda095a434a7b00733115a0d679de6478d9f8000000000000000000000000888888888889758f76e7103c6cbf23abbf58f9460000000000000000000000000000000000000000000000018fbcac6b00d1700000000000000000000000000000000000000000000000000000000000000000000000000000000000888888888889758f76e7103c6cbf23abbf58f94600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000001c447f1de220000000000000000000000007baa298d36fe21df2f6b54510da76445661a91ed0000000000000000000000001f509072388208a698c8d5784bda2702cae06f9c0000000000000000000000000000000000000000000000018fbcac6b00d170000000000000000000000000000000000000000000000000000000000000000080000000000000000000000000e868084cf08f3c3db11f4b73a95473762d9463f70000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e868084cf08f3c3db11f4b73a95473762d9463f7000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000648a6a1e85000000000000000000000000e868084cf08f3c3db11f4b73a95473762d9463f7000000000000000000000000922164bbbd36acf9e854acbbf32facc949fcaeef0000000000000000000000000000000000000000000000018fbcac6b00d1700000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000001a49f865422000000000000000000000000e868084cf08f3c3db11f4b73a95473762d9463f700000000000000000000000000000001000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000004400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000064d1660f99000000000000000000000000e868084cf08f3c3db11f4b73a95473762d9463f70000000000000000000000002bba09866b6f1025258542478c39720a09b728bf0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"),
                            bytes!("0x3bc1f1ed000000000000000000000000076bda095a434a7b00733115a0d679de6478d9f80000000000000000000000000f93f35c0664a6a8231ccae7e22f652c9c075b3200000000000000000000000000000000000000000000000000000000000000050000000000000000000000002bba09866b6f1025258542478c39720a09b728bf")
                        ]
                            .iter()
                            .map(|b| MulticallItem { data: b.clone() })
                            .collect(),
                    }
                    )
                );

            let liq = liq.with_profit(
                U256::from_str("15000000000000").unwrap(),
                U256::from_str("15000000000000").unwrap(),
            );

            return Ok(Some(liq));
        }
    }

    #[tokio::test]
    // The liquidation we are copying:
    // https://etherscan.io/tx/0x42533f3be6999ddeba1c3672d70c91f879ee1568ed61085293f7ff41a874a9d8
    async fn liquidation() {
        let block = 24790465;
        let violator = address!("0x65E30583c1939344d57bBCdf3A1Bbb28d41164f2");
        let recipient = address!("0xA64c03b6be0AF9470573CF8AFC1626dA93C22057");

        // Network (mainnet) specific configuration.
        // let wrapped_native_asset = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));
        let liquidator_address = address!("0xAAF93d5475d092EA68a748137eE19D8130918392");

        let mainnet_rpc = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");

        // Fork the network at the block where this liquidation was present.
        let network = Anvil::new()
            .fork(mainnet_rpc)
            .fork_block_number(block)
            .arg("--disable-min-priority-fee")
            .try_spawn()
            .unwrap();

        let provider = ProviderBuilder::new()
            .connect_http(network.endpoint_url())
            .erased();

        // We set the gas fee for the next block to be very low so the liquidation is always
        // profitable.
        provider
            .anvil_set_next_block_base_fee_per_gas(100)
            .await
            .unwrap();

        // Fetch the account.
        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            violator,
        )
        .await
        .unwrap();

        let (liquidation_sender, liquidation_receiver) = mpsc::channel::<PreparedLiquidation>(100);

        {
            let provider = ProviderBuilder::new()
                .wallet(network.wallet().unwrap())
                .connect_http(network.endpoint_url());

            tokio::spawn(async move {
                execute_liquidation_queue(provider, liquidation_receiver, recipient).await;
            });
        }

        // Sanity check, as we later also check this and then it will be empty.
        assert!(!account.borrows.is_empty());

        let liquidation = prepare_liquidation(
            &provider,
            &MockSwapProvider,
            None, // This liquidation does not use any pyth oracles.
            liquidator_address,
            account,
        )
        .await
        .unwrap()
        .unwrap();

        // Send the liquidation to be executed.
        liquidation_sender.send(liquidation).await.unwrap();

        // Give it some time to perform the liquidation.
        wait_for_next_block(&provider, Some(block)).await;

        // This test seems to be unreliable if its run concurrent with other tests without this
        // delay.
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Re-fetch the account to see its updated status.
        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            violator,
        )
        .await
        .unwrap();

        // Check that they no longer have any debt.
        assert!(account.borrows.is_empty());
    }

    #[tokio::test]
    async fn liquidation_with_swap_data() {
        // This account is healthy at this block.
        let block = 24935457;
        let violator = address!("0x68A405Fbe0bC42a228baFdBdD27F17c15475352D");
        let recipient = address!("0xA64c03b6be0AF9470573CF8AFC1626dA93C22057");

        // Network (mainnet) specific configuration.
        let wrapped_native_asset = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));
        let liquidator_address = address!("0xAAF93d5475d092EA68a748137eE19D8130918392");
        let swapper = address!("0x2Bba09866b6F1025258542478C39720A09B728bF");

        let mainnet_rpc = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");

        // Fork the network at the block where this liquidation was present.
        let network = Anvil::new()
            .fork(mainnet_rpc)
            .fork_block_number(block)
            .arg("--disable-min-priority-fee")
            .try_spawn()
            .unwrap();

        let provider = ProviderBuilder::new()
            .connect_http(network.endpoint_url())
            .erased();

        // Fetch the account.
        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            violator,
        )
        .await
        .unwrap();

        let (liquidation_sender, liquidation_receiver) = mpsc::channel::<PreparedLiquidation>(100);

        {
            let provider = ProviderBuilder::new()
                .wallet(network.wallet().unwrap())
                .connect_http(network.endpoint_url());

            tokio::spawn(async move {
                execute_liquidation_queue(provider, liquidation_receiver, recipient).await;
            });
        }

        // Sanity check, as we later also check this and then it will be empty.
        assert!(!account.borrows.is_empty());

        let liquidation = prepare_liquidation(
            &provider.clone(),
            &EulerSwapApi::new(
                "https://swap.euler.finance".parse().unwrap(),
                provider.clone().erased(),
                1,
                liquidator_address,
                liquidator_address,
                swapper,
                wrapped_native_asset,
                "5", // Max slippage
                EulerPricingApi::new("https://v3.euler.finance".parse().unwrap(), 1),
            ),
            None, // This liquidation does not use any pyth oracles.
            liquidator_address,
            account.clone(),
        )
        .await;

        // Since we have not yet modified the oracle result, this should report as being healthy.
        assert!(matches!(liquidation, Ok(None)));

        // We override the oracle adapter to make this account unhealthy.
        provider.anvil_set_code(address!("0x83b3b76873d36a28440cf53371df404c42497136"), bytes!("0x608060405234801561000f575f5ffd5b506004361061003f575f3560e01c80630579e61f1461004357806306fdde0314610074578063ae68676c14610092575b5f5ffd5b61005d60048036038101906100589190610232565b6100c2565b60405161006b929190610291565b60405180910390f35b61007c6100ff565b6040516100899190610328565b60405180910390f35b6100ac60048036038101906100a79190610232565b61013c565b6040516100b99190610348565b60405180910390f35b5f5f6040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016100f6906103ab565b60405180910390fd5b60606040518060400160405280600a81526020017f4d6f636b4f7261636c6500000000000000000000000000000000000000000000815250905090565b5f732260fac5e5542a773aa44fbcfedf7c193bc2c59973ffffffffffffffffffffffffffffffffffffffff168373ffffffffffffffffffffffffffffffffffffffff1614610191576414f46b0400905061019a565b64174876e80090505b9392505050565b5f5ffd5b5f819050919050565b6101b7816101a5565b81146101c1575f5ffd5b50565b5f813590506101d2816101ae565b92915050565b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f610201826101d8565b9050919050565b610211816101f7565b811461021b575f5ffd5b50565b5f8135905061022c81610208565b92915050565b5f5f5f60608486031215610249576102486101a1565b5b5f610256868287016101c4565b93505060206102678682870161021e565b92505060406102788682870161021e565b9150509250925092565b61028b816101a5565b82525050565b5f6040820190506102a45f830185610282565b6102b16020830184610282565b9392505050565b5f81519050919050565b5f82825260208201905092915050565b8281835e5f83830152505050565b5f601f19601f8301169050919050565b5f6102fa826102b8565b61030481856102c2565b93506103148185602086016102d2565b61031d816102e0565b840191505092915050565b5f6020820190508181035f83015261034081846102f0565b905092915050565b5f60208201905061035b5f830184610282565b92915050565b7f4e6f7420696d706c656d656e74656400000000000000000000000000000000005f82015250565b5f610395600f836102c2565b91506103a082610361565b602082019050919050565b5f6020820190508181035f8301526103c281610389565b905091905056fea2646970667358221220534437302cf2579d8f3d2b01d3b25e913fc61310f84d4e1c8b03fa6f2308520864736f6c63430008210033")).await.unwrap();

        let liquidation = prepare_liquidation(
            &provider.clone(),
            &EulerSwapApi::new(
                "https://swap.euler.finance".parse().unwrap(),
                provider.clone().erased(),
                1,
                liquidator_address,
                liquidator_address,
                swapper,
                wrapped_native_asset,
                "5", // Max slippage.
                EulerPricingApi::new("https://v3.euler.finance".parse().unwrap(), 1),
            ),
            None, // This liquidation does not use any pyth oracles.
            liquidator_address,
            account.clone(),
        )
        .await
        .unwrap()
        .unwrap();

        // Send the liquidation to be executed.
        liquidation_sender.send(liquidation).await.unwrap();

        // Give it some time to perform the liquidation.
        wait_for_next_block(&provider, Some(block)).await;

        // Re-fetch the account to see its updated status.
        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            violator,
        )
        .await
        .unwrap();

        // Check that they no longer have any debt.
        assert!(account.borrows.is_empty());
    }

    /// Waits up to 30 seconds for a new block to be mined, polling once per second.
    /// Panics if no new block is mined within the timeout. Intended for use in tests.
    pub async fn wait_for_next_block<P: Provider>(provider: &P, current_block: Option<u64>) {
        let start_block = match current_block {
            Some(block) => block,
            None => provider
                .get_block_number()
                .await
                .expect("failed to get starting block number"),
        };

        for i in 0..30 {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

            let current_block = provider
                .get_block_number()
                .await
                .expect("failed to get current block number");

            if current_block > start_block {
                tracing::info!(
                    "test: new block mined after {}s: {} -> {}",
                    i + 1,
                    start_block,
                    current_block
                );
                return;
            }
        }

        panic!(
            "no new block was mined within 30 seconds (still at block {})",
            start_block
        );
    }
}
