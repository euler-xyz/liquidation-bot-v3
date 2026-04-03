use std::{collections::HashMap, sync::Arc};

use alloy::primitives::Address;
use itertools::Itertools;

use crate::types::{Account, OracleIdentifier, Vault, VaultAssetPosition, VaultDebtPosition};

pub struct AccountsTracker {
    accounts: HashMap<Address, Account>,
    /// Maps the accounts that are dependent on a oracle.
    oracle_dependents: HashMap<OracleIdentifier, Vec<Address>>,
}

impl AccountsTracker {
    pub fn new() -> Self {
        AccountsTracker {
            accounts: HashMap::new(),
            oracle_dependents: HashMap::new(),
        }
    }

    /// Add a new account to the tracker.
    pub fn add_as_account(
        &mut self,
        address: Address,
        assets: Vec<VaultAssetPosition>,
        debt: Vec<VaultDebtPosition>,
    ) {
        self.add(Account {
            address,
            assets,
            debt,
        });
    }

    pub fn add(&mut self, account: Account) {
        // Skip accounts that have no debt, these are not of interest to us.
        if account.debt.is_empty() {
            return;
        }

        account.dependent_on().iter().for_each(|o| {
            let od = self.oracle_dependents.entry(o.clone()).or_default();
            od.push(account.address);
        });

        // TODO: handle the case where we (accidentally?) replace an existing accounting.
        let _ = self.accounts.insert(account.address, account);
    }

    /// Get all unique oracle identifiers.
    pub fn get_oracle_identifiers(&self) -> Vec<OracleIdentifier> {
        self.oracle_dependents.keys().cloned().collect()
    }

    /// Finds the accounts that are impacted when a specific oracle price changes.
    pub fn get_impacted_accounts(&self, oracle: &OracleIdentifier) -> Vec<Account> {
        self.oracle_dependents
            .get(oracle)
            .unwrap_or(&vec![])
            .iter()
            // This unwrap is still safe as its impossible to be an oracle dependent and not be
            // mapped in accounts. We should still get rid of it though.
            .map(|a| self.accounts.get(a).unwrap().clone())
            .collect()
    }

    /// Finds all accounts that are affected by any of the oracle updates.
    pub fn get_bulk_impacted_accounts(&self, oracles: Vec<OracleIdentifier>) -> Vec<Account> {
        oracles
            .iter()
            .flat_map(|o| self.oracle_dependents.get(o).cloned().unwrap_or(vec![]))
            .unique()
            // This unwrap is still safe as its impossible to be an oracle dependent and not be
            // mapped in accounts. We should still get rid of it though.
            .map(|address| self.accounts.get(&address).unwrap().clone())
            .collect()
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use alloy::primitives::{Address, U256};

    use crate::{
        accounts::AccountsTracker,
        types::{OracleIdentifier, Vault, VaultAssetPosition, VaultDebtPosition},
    };

    #[tokio::test]
    async fn impacted_finds_accounts() {
        let mut accounts = AccountsTracker::new();

        let account_to_find = Address::random();
        let oracle = OracleIdentifier {
            base_asset: Address::random(),
            quote_asset: Address::random(),
            adapter: Address::random(),
        };

        // Create two accounts and insert them into the tracker.
        accounts.add_as_account(
            account_to_find,
            vec![
                VaultAssetPosition {
                    amount: U256::from(100_000_000),
                    vault: Arc::from(Vault {
                        address: Address::random(),
                        asset: oracle.base_asset,
                        unit_of_account: oracle.quote_asset,
                        borrow_interest_rate: (),
                        supply_interest_rate: (),
                        adapter: oracle.adapter,
                    }),
                },
                VaultAssetPosition::generate_random(),
            ],
            vec![VaultDebtPosition {
                amount: U256::from(100_000_000),
                vault: Arc::from(Vault {
                    address: Address::random(),
                    asset: Address::random(),
                    unit_of_account: Address::random(),
                    borrow_interest_rate: (),
                    supply_interest_rate: (),
                    adapter: oracle.adapter,
                }),
            }],
        );

        for _ in 0..5_000 {
            accounts.add_as_account(
                Address::random(),
                vec![
                    VaultAssetPosition::generate_random(),
                    VaultAssetPosition::generate_random(),
                ],
                vec![VaultDebtPosition::generate_random()],
            );
        }

        let found = accounts.get_impacted_accounts(&oracle);
        assert!(found.len() == 1);
        assert!(found.first().unwrap().address == account_to_find);
    }
}
