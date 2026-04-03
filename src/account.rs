use alloy::{
    primitives::{Address, U256},
    providers::{DynProvider, Provider},
    rpc::types::{Filter, Log},
    sol,
    sol_types::SolEvent,
};
use anyhow::{Result, bail};
use std::sync::Arc;
use tokio::{sync::mpsc::Sender, time};

use crate::{
    prices::Prices,
    types::{Account, OracleIdentifier, Vault},
};

#[derive(Debug, Clone)]
pub struct AccountSolvency {
    account: Address,
    asset_value: U256,
    debt_value: U256,
    accounted_in: Address,
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

}

/// Watches the chain for account update events
pub async fn watch_chain_for_accounts(
    provider: DynProvider,
    evc: Address,
    account_update_channel: Sender<Address>,
    mut from_block: u64,
) {
    loop {
        let latest = provider.get_block_number().await.unwrap();

        if latest >= from_block {
            let filter = Filter::new()
                .address(evc)
                .from_block(from_block)
                .to_block(latest)
                .event_signature(Events::AccountStatusCheck::SIGNATURE_HASH);

            let logs: Vec<Log> = provider.get_logs(&filter).await.unwrap();

            for log in &logs {
                match Events::AccountStatusCheck::decode_log(&log.inner) {
                    Ok(decoded) => {
                        let block = log.block_number.unwrap_or_default();
                        println!("Block {block} | Account: {}", decoded.account);
                        account_update_channel.send(decoded.account).await.unwrap();
                    }
                    Err(e) => eprintln!("Decode error: {e}"),
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

    pub fn calculate_health(&self, prices: &Prices) -> Result<AccountSolvency> {
        let debt = match self.debt.first() {
            Some(debt) => debt,
            None => bail!("An account with no debt does not have an health score."),
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
                prices.get_quote(
                    &OracleIdentifier {
                        base_asset: a.vault.asset,
                        quote_asset: debt.vault.unit_of_account,
                        adapter: debt.vault.adapter,
                    },
                    a.amount,
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
