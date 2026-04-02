use alloy::primitives::{Address, U256};
use anyhow::{Result, bail};
use std::sync::Arc;

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
