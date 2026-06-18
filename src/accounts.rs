use alloy::primitives::Address;
use dashmap::DashMap;
use itertools::Itertools;
use tracing::error;

use crate::types::{
    Account, LiquidationReasoning, OracleIdentifier, VaultBorrowPosition, VaultCollateralPosition,
};

pub struct AccountsTracker {
    accounts: DashMap<Address, Account>,
    /// Maps the accounts that are dependent on a oracle.
    oracle_dependents: DashMap<OracleIdentifier, Vec<Address>>,
}

impl AccountsTracker {
    pub fn new() -> Self {
        AccountsTracker {
            accounts: DashMap::new(),
            oracle_dependents: DashMap::new(),
        }
    }

    /// Add a new account to the tracker.
    pub fn add_as_account(
        &self,
        address: Address,
        collaterals: Vec<VaultCollateralPosition>,
        borrows: Vec<VaultBorrowPosition>,
    ) {
        self.add(Account::new(address, borrows, collaterals));
    }

    pub fn add(&self, account: Account) {
        // Check if we are already tracking this account.
        if let Some(old_account) = self.accounts.get(&account.address) {
            // Remove it as an oracle_dependent for its oracles.
            old_account.dependent_on().iter().for_each(|o| {
                let mut od = self.oracle_dependents.entry(o.clone()).or_default();
                od.retain(|a| *a != account.address);
            });
            drop(old_account);
        }

        // Skip accounts that have no borrows, these are not of interest to us.
        // If it existed before we remove it now.
        if account.borrows.is_empty() {
            self.accounts.remove(&account.address);
            return;
        }

        account.dependent_on().iter().for_each(|o| {
            let mut od = self.oracle_dependents.entry(o.clone()).or_default();
            od.value_mut().push(account.address);
        });

        let _ = self.accounts.insert(account.address, account);
    }

    /// Get all unique oracle identifiers.
    pub fn get_oracle_identifiers(&self) -> Vec<OracleIdentifier> {
        self.oracle_dependents
            .iter()
            .map(|od| od.key().clone())
            .collect()
    }

    /// Finds the accounts that are impacted when a specific oracle price changes.
    pub fn get_impacted_accounts(&self, oracle: &OracleIdentifier) -> Vec<Account> {
        self.oracle_dependents
            .get(oracle)
            .map(|od| od.value().clone())
            .unwrap_or(vec![])
            .iter()
            .filter_map(|a| {
                match self.accounts.get(a) {
                    Some(account) => Some(account.clone()),
                    None => {
                        error!("As we were fetching an impacted account we do not have the account stored, this should be impossible. Some invariant was broken.");
                        None
                    }
                }
            })
            .collect()
    }

    /// Finds all accounts that are affected by any of the oracle updates.
    pub fn get_bulk_impacted_accounts(&self, oracles: Vec<OracleIdentifier>) -> Vec<Account> {
        oracles
            .iter()
            .flat_map(|o| self.oracle_dependents.get(o).map(|od| od.value().clone()).unwrap_or(vec![]))
            .unique()
            .filter_map(|a| {
                match self.accounts.get(&a) {
                    Some(account) => Some(account.clone()),
                    None => {
                        error!("As we were fetching an impacted account we do not have the account stored, this should be impossible. Some invariant was broken.");
                        None
                    }
                }
            })
            .collect()
    }

    pub fn all_accounts(&self) -> Vec<Account> {
        self.accounts.iter().map(|a| a.clone()).collect()
    }
}

#[cfg(test)]
mod test {
    use std::{collections::HashMap, sync::Arc};

    use alloy::{
        node_bindings::Anvil,
        primitives::{Address, U256, address},
        providers::{Provider, ProviderBuilder},
    };

    use crate::{
        accounts::AccountsTracker,
        config::VaultFilter,
        lens::fetch_account,
        types::{Account, OracleIdentifier, Vault, VaultBorrowPosition, VaultCollateralPosition},
        vaults::Vaults,
    };

    #[tokio::test]
    // When updating an account to have no borrow we should be removing the account.
    async fn update_account_to_have_no_borrow() {
        let accounts = AccountsTracker::new();

        let account_address = Address::random();
        let account = Account::new(
            account_address,
            vec![VaultBorrowPosition::generate_random()],
            vec![VaultCollateralPosition::generate_random()],
        );

        accounts.add(account.clone());

        // Should now have 1 account.
        assert_eq!(accounts.all_accounts().len(), 1);

        // Check that we get the account when we check for impacted accounts.
        assert_eq!(
            accounts
                .get_impacted_accounts(account.dependent_on().first().unwrap())
                .len(),
            1
        );

        // Now we update the account to no longer have any outstanding borrows.
        let original_account = account;
        let account = Account::new(
            account_address,
            vec![],
            original_account.collaterals.clone(),
        );

        accounts.add(account);

        // Should now have no accounts.
        assert!(accounts.all_accounts().is_empty());

        // Check that it is no longer being reported as being impacted by price changes.
        original_account
            .dependent_on()
            .iter()
            .for_each(|dp| assert!(accounts.get_impacted_accounts(dp).is_empty()));
    }

    #[tokio::test]
    async fn impacted_finds_accounts() {
        let accounts = AccountsTracker::new();

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
                VaultCollateralPosition {
                    amount: U256::from(100_000_000),
                    vault: Arc::from(Vault {
                        address: Address::random(),
                        asset: oracle.base_asset,
                        unit_of_account: oracle.quote_asset,
                        borrow_interest_rate: (),
                        supply_interest_rate: (),
                        adapter: oracle.adapter,
                        ltvs: HashMap::new(),
                        shares_to_underlying_ratio: U256::from(100_000),
                    }),
                },
                VaultCollateralPosition::generate_random(),
            ],
            vec![VaultBorrowPosition {
                amount: U256::from(100_000_000),
                vault: Arc::from(Vault {
                    address: Address::random(),
                    asset: Address::random(),
                    unit_of_account: oracle.quote_asset,
                    borrow_interest_rate: (),
                    supply_interest_rate: (),
                    adapter: oracle.adapter,
                    ltvs: HashMap::new(),
                    shares_to_underlying_ratio: U256::from(100_000),
                }),
            }],
        );

        for _ in 0..5_000 {
            accounts.add_as_account(
                Address::random(),
                vec![
                    VaultCollateralPosition::generate_random(),
                    VaultCollateralPosition::generate_random(),
                ],
                vec![VaultBorrowPosition::generate_random()],
            );
        }

        let found = accounts.get_impacted_accounts(&oracle);
        assert!(found.len() == 1);
        assert!(found.first().unwrap().address == account_to_find);
    }

    #[tokio::test]
    async fn filter_whitelist() {
        let block = 24899561;
        let account = address!("0x5Dac9ccC215b9Af65B486066786F79d9aa0043Db");
        let vault = address!("0x9bd52f2805c6af014132874124686e7b248c2cbb");

        let mainnet_rpc = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");
        let network = Anvil::new()
            .fork(mainnet_rpc)
            .fork_block_number(block)
            .try_spawn()
            .unwrap();

        let provider = ProviderBuilder::new()
            .connect_http(network.endpoint_url())
            .erased();

        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));

        // The filter that will allow the account.
        let happy_filter = VaultFilter {
            mode: crate::config::VaultFilterMode::Whitelist,
            items: vec![Address::random(), Address::random(), vault],
        };

        // Fetch the account with no filter, this should work as expected.
        fetch_account(
            provider.clone(),
            &happy_filter,
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            account,
        )
        .await
        .expect("Could not fetch account");

        let sad_filter = VaultFilter {
            mode: crate::config::VaultFilterMode::Whitelist,
            items: vec![Address::random(), Address::random()],
        };

        // Fetch the account again but now with whitelist filter that should not allow it.
        fetch_account(
            provider.clone(),
            &sad_filter,
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            account,
        )
        .await
        .expect_err("Expected this to get filtered out by the whitelist");
    }

    #[tokio::test]
    async fn filter_blacklist() {
        let block = 24899561;
        let account = address!("0x5Dac9ccC215b9Af65B486066786F79d9aa0043Db");
        let vault = address!("0x9bd52f2805c6af014132874124686e7b248c2cbb");

        let mainnet_rpc = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");
        let network = Anvil::new()
            .fork(mainnet_rpc)
            .fork_block_number(block)
            .try_spawn()
            .unwrap();

        let provider = ProviderBuilder::new()
            .connect_http(network.endpoint_url())
            .erased();

        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));

        // The filter that will allow the account.
        let happy_filter = VaultFilter {
            mode: crate::config::VaultFilterMode::Blacklist,
            items: vec![Address::random(), Address::random()],
        };

        // Fetch the account with no filter, this should work as expected.
        fetch_account(
            provider.clone(),
            &happy_filter,
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            account,
        )
        .await
        .expect("Could not fetch account");

        let sad_filter = VaultFilter {
            mode: crate::config::VaultFilterMode::Blacklist,
            items: vec![Address::random(), Address::random(), vault],
        };

        // Fetch the account again but now with whitelist filter that should not allow it.
        fetch_account(
            provider.clone(),
            &sad_filter,
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            account,
        )
        .await
        .expect_err("Expected this to get filtered out by the whitelist");
    }
}
