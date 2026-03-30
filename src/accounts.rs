use std::collections::HashMap;

use alloy::primitives::Address;
use itertools::iproduct;

use crate::types::{Account, OracleIdentifier, VaultAssetPosition, VaultDebtPosition};

pub struct AccountsTracker {
    accounts: HashMap<Address, Account>,
    /// Maps the accounts that are dependent on a oracle.
    oracle_dependents: HashMap<OracleIdentifier, Vec<Address>>,
}

impl AccountsTracker {
    fn new() -> Self {
        AccountsTracker {
            accounts: HashMap::new(),
            oracle_dependents: HashMap::new(),
        }
    }

    /// Add a new account to the tracker.
    fn add(
        &mut self,
        address: Address,
        assets: Vec<VaultAssetPosition>,
        debt: Vec<VaultDebtPosition>,
    ) {
        let account = Account {
            address,
            assets,
            debt,
        };

        account.dependent_on().iter().for_each(|o| {
            let od = self.oracle_dependents.entry(o.clone()).or_default();
            od.push(account.address);
        });

        // TODO: handle the case where we (accidentally?) replace an existing accounting.
        let _ = self.accounts.insert(address, account);
    }

    /// Finds the accounts that are impacted when a specific oracle price changes.
    fn get_impacted_accounts(&self, oracle: &OracleIdentifier) -> Vec<Account> {
        self.oracle_dependents
            .get(&oracle)
            .unwrap_or(&vec![])
            .iter()
            // This unwrap is still safe as its impossible to be an oracle dependent and not be
            // mapped in accounts. We should still get rid of it though.
            .map(|a| self.accounts.get(a).unwrap().clone())
            .collect()
    }
}

impl Account {
    fn dependent_on(&self) -> Vec<OracleIdentifier> {
        let mut dependents = Vec::new();
        for (asset, debt) in iproduct!(&self.assets, &self.debt) {
            dependents.push(OracleIdentifier {
                base_asset: debt.vault.asset,
                quote_asset: asset.vault.asset,
                adapter: debt.vault.adapter,
            });
        }
        dependents
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
        accounts.add(
            account_to_find,
            vec![
                VaultAssetPosition {
                    amount: U256::from(100_000_000),
                    vault: Arc::from(Vault {
                        address: Address::random(),
                        asset: oracle.quote_asset,
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
                    asset: oracle.base_asset,
                    borrow_interest_rate: (),
                    supply_interest_rate: (),
                    adapter: oracle.adapter,
                }),
            }],
        );

        for _ in 0..5_000 {
            accounts.add(
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
