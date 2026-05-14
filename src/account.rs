use alloy::{
    primitives::{Address, U256},
    providers::{DynProvider, Provider},
    rpc::types::{Filter, Log},
    sol,
    sol_types::SolEvent,
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::sync::Arc;
use tokio::{sync::mpsc::Sender, time};
use tracing::{debug, error, info};

use crate::{
    oracles::OraclesCache,
    types::{Account, OracleIdentifier, Vault},
};

#[derive(Debug, Clone, Serialize)]
pub struct AccountSolvency {
    pub account: Address,
    pub asset_value: U256,
    pub debt_value: U256,
    pub accounted_in: Address,
}

sol! {
    /// @title Events
    /// @custom:security-contact security@euler.xyz
    /// @author Euler Labs (https://www.eulerlabs.com/)
    /// @notice This contract implements the events for the Ethereum Vault Connector.
    #[sol(rpc)]
    contract Events {
        /// @notice Emitted when an account status check is performed.
        /// @param account The account for which the status check is performed.
        /// @param controller The controller performing the status check.
        event AccountStatusCheck(address indexed account, address indexed controller);
    }

    #[sol(rpc)]
    interface ILiquidation {
        /// @notice Checks to see if a liquidation would be profitable, without actually doing anything
        /// @param liquidator Address that will initiate the liquidation
        /// @param violator Address that may be in collateral violation
        /// @param collateral Collateral which is to be seized
        /// @return maxRepay Max amount of debt that can be repaid, in asset units
        /// @return maxYield Yield in collateral corresponding to max allowed amount of debt to be repaid, in collateral
        /// balance (shares for vaults)
        function checkLiquidation(address liquidator, address violator, address collateral)
            external
            view
            returns (uint256 maxRepay, uint256 maxYield);
    }
}

/// Watches the chain for account update events from the most recent block.
pub async fn watch_chain_for_accounts_from_latest(
    provider: DynProvider,
    evc: Address,
    account_update_channel: Sender<Address>,
) {
    let latest = match provider.get_block_number().await {
        Ok(latest) => latest,
        Err(err) => {
            error!("Error while fetching the current block number: {err}");
            0
        }
    };

    watch_chain_for_accounts(provider, evc, account_update_channel, latest).await
}

/// Watches the chain for account update events
pub async fn watch_chain_for_accounts(
    provider: DynProvider,
    evc: Address,
    account_update_channel: Sender<Address>,
    mut from_block: u64,
) {
    loop {
        let latest = match provider.get_block_number().await {
            Ok(latest) => latest,
            Err(err) => {
                error!("Error while fetching the current block number: {err}");
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        if latest >= from_block {
            let filter = Filter::new()
                .address(evc)
                .from_block(from_block)
                .to_block(latest)
                .event_signature(Events::AccountStatusCheck::SIGNATURE_HASH);

            let logs: Vec<Log> = match provider.get_logs(&filter).await {
                Ok(logs) => logs,
                Err(err) => {
                    error!(
                        "Error while fetching logs from block range {}-{}: {err}",
                        from_block, latest
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

            for log in &logs {
                match Events::AccountStatusCheck::decode_log(&log.inner) {
                    Ok(decoded) => {
                        let block = log.block_number.unwrap_or_default();
                        info!(
                            "Found account event for {} at block {}",
                            decoded.account, block
                        );

                        // Send the update over the channel.
                        if let Err(err) = account_update_channel.send(decoded.account).await {
                            error!(
                                "Issue when attempting to send update over accounts channel, it was likely dropped, err: {:?}",
                                err
                            );
                        }
                    }
                    Err(e) => error!("Decode error: {:?}", e),
                }
            }

            // Advance past the range we just queried
            from_block = latest + 1;
        }

        // TODO: Make duration configurable, perhaps also an option to watch for new block events.
        time::sleep(tokio::time::Duration::from_secs(15)).await;
    }
}

impl AccountSolvency {
    pub fn is_unhealthy(&self) -> bool {
        self.debt_value > self.asset_value
    }
}

impl Account {
    /// Get all the vaults this account has relations to.
    pub fn vaults(&self) -> Vec<Arc<Vault>> {
        let mut vaults: Vec<Arc<Vault>> = self.assets.iter().map(|a| a.vault.clone()).collect();
        vaults.extend(self.debt.iter().map(|d| d.vault.clone()));
        vaults
    }

    pub fn dependent_on(&self) -> Vec<OracleIdentifier> {
        let debt = match self.debt.first() {
            Some(debt) => debt,

            // If there is no debt then the account does not have a health score.
            None => return vec![],
        };

        // Add the asset oracles.
        let mut oracles: Vec<OracleIdentifier> = self
            .assets
            .iter()
            .map(|asset| OracleIdentifier {
                base_asset: asset.vault.asset,
                quote_asset: debt.vault.unit_of_account,
                adapter: debt.vault.adapter,
            })
            .collect();

        // Push the debt oracle.
        oracles.push(OracleIdentifier {
            base_asset: debt.vault.asset,
            quote_asset: debt.vault.unit_of_account,
            adapter: debt.vault.adapter,
        });

        oracles
    }

    pub fn calculate_health(&self, prices: &OraclesCache) -> Result<AccountSolvency> {
        let debt = match self.debt.first() {
            Some(debt) => debt,
            None => bail!("An account with no debt does not have a health score."),
        };

        let debt_value = prices.get_quote(
            &OracleIdentifier {
                base_asset: debt.vault.asset,
                quote_asset: debt.vault.unit_of_account,
                adapter: debt.vault.adapter,
            },
            debt.amount,
        )?;

        let total_assets = self
            .assets
            .iter()
            .map(|a| {
                // Take into acccount the liquidation LTV.
                let amount = match debt.vault.ltvs.get(&a.vault.address) {
                    Some(ltv) => {
                        // Convert the amount into shares.
                        let amount = a.amount * a.vault.shares_to_underlying_ratio / U256::from(100_000);

                        // Apply the liquidation LTV onto the underlying.
                        amount * ltv.current_liquidation_ltv() / U256::from(10_000)
                    },
                    None => {
                        debug!( controller =? debt.vault.address, asset =? a.vault.asset, "While calculating health for account we found an account with debt but the controller does not support the asset.");
                        // This asset is not supported by the controller so its value is 0.
                        U256::ZERO
                    }
                };

                prices.get_quote(
                    &OracleIdentifier {
                        base_asset: a.vault.asset,
                        quote_asset: debt.vault.unit_of_account,
                        adapter: debt.vault.adapter,
                    },
                    amount,
                )
            })
            .collect::<Result<Vec<U256>>>()?
            .iter()
            .sum::<U256>();

        // Calculate the asset value.
        Ok(AccountSolvency {
            account: self.address,
            asset_value: total_assets,
            debt_value,
            accounted_in: debt.vault.unit_of_account,
        })
    }
}
