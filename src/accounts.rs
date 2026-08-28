use crate::types::{Account, OracleIdentifier, VaultBorrowPosition, VaultCollateralPosition};
use alloy::primitives::Address;
use dashmap::DashMap;
use itertools::Itertools;
use tracing::error;

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
        account::AccountSolvency,
        accounts::AccountsTracker,
        config::{VaultFilter, load_configuration_file_for_test},
        lens::fetch_account,
        oracles::OraclesCache,
        test_utils::ensure_contracts_on_fork,
        types::{
            Account, EVault, Erc4626Vault, OracleIdentifier, Vault, VaultBorrowPosition,
            VaultCollateralPosition,
        },
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
                    vault: Vault::Erc4626(Arc::from(Erc4626Vault {
                        address: Address::random(),
                        asset: oracle.base_asset,
                        shares_to_underlying_ratio: U256::from(100_000),
                    })),
                },
                VaultCollateralPosition::generate_random(),
            ],
            vec![VaultBorrowPosition {
                amount: U256::from(100_000_000),
                vault: Arc::from(EVault {
                    erc4626: Erc4626Vault {
                        address: Address::random(),
                        asset: Address::random(),
                        shares_to_underlying_ratio: U256::from(100_000),
                    },
                    unit_of_account: oracle.quote_asset,
                    borrow_interest_rate: (),
                    supply_interest_rate: (),
                    adapter: oracle.adapter,
                    ltvs: HashMap::new(),
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
        let block = 25644480;
        let account = address!("0x81633c1357ddb25d8625efb2cad26a60988475bc");
        let vault = address!("0xba98fc35c9dfd69178ad5dce9fa29c64554783b5");

        let mainnet_rpc = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");
        let config = load_configuration_file_for_test(&mainnet_rpc, 1).unwrap();

        let network = Anvil::new()
            .fork(mainnet_rpc)
            .fork_block_number(block)
            .try_spawn()
            .unwrap();

        let provider = ProviderBuilder::new()
            .connect_http(network.endpoint_url())
            .erased();

        // Some of the configured contracts may not have been deployed yet at the forked
        // block, inject their current code so we can just use the config addresses.
        ensure_contracts_on_fork(
            &provider,
            &config.rpc_url,
            &[
                config.vault_lens_address,
                config.utils_lens_address,
                config.account_lens_address,
            ],
        )
        .await
        .unwrap();

        let vaults = &mut Vaults::new(config.vault_lens_address, config.utils_lens_address);

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
            config.account_lens_address,
            config.evc_address,
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
            config.account_lens_address,
            config.evc_address,
            account,
        )
        .await
        .expect_err("Expected this to get filtered out by the whitelist");
    }

    #[tokio::test]
    async fn filter_blacklist() {
        let block = 25644480;
        let account = address!("0x81633c1357ddb25d8625efb2cad26a60988475bc");
        let vault = address!("0xba98fc35c9dfd69178ad5dce9fa29c64554783b5");

        let mainnet_rpc = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");
        let config = load_configuration_file_for_test(&mainnet_rpc, 1).unwrap();

        let network = Anvil::new()
            .fork(mainnet_rpc)
            .fork_block_number(block)
            .try_spawn()
            .unwrap();

        let provider = ProviderBuilder::new()
            .connect_http(network.endpoint_url())
            .erased();

        // Some of the configured contracts may not have been deployed yet at the forked
        // block, inject their current code so we can just use the config addresses.
        ensure_contracts_on_fork(
            &provider,
            &config.rpc_url,
            &[
                config.vault_lens_address,
                config.utils_lens_address,
                config.account_lens_address,
            ],
        )
        .await
        .unwrap();

        let vaults = &mut Vaults::new(config.vault_lens_address, config.utils_lens_address);

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
            config.account_lens_address,
            config.evc_address,
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
            config.account_lens_address,
            config.evc_address,
            account,
        )
        .await
        .expect_err("Expected this to get filtered out by the whitelist");
    }

    // Debug helper: fetches a single account, logs its values and its health. All
    // contract addresses are taken from the chain's configuration file. If a block number
    // is given the chain is forked at that block, otherwise the live chain is used at the
    // latest block.
    // Run with: <RPC_ENV>=<url> cargo test fetch_and_log -- --nocapture
    async fn fetch_and_log(
        rpc_env: &str,
        chain_id: u64,
        block: Option<u64>,
        account: Address,
    ) -> (Account, AccountSolvency) {
        let rpc = std::env::var(rpc_env).unwrap_or_else(|_| panic!("{rpc_env} must be set"));
        let config = load_configuration_file_for_test(&rpc, chain_id).unwrap();

        // Keeps the Anvil instance alive for the duration of the function when forking.
        let (provider, _network) = match block {
            Some(block) => {
                let network = Anvil::new()
                    .fork(rpc)
                    .fork_block_number(block)
                    .try_spawn()
                    .unwrap();

                let provider = ProviderBuilder::new()
                    .connect_http(network.endpoint_url())
                    .erased();

                // Some of the configured contracts may not have been deployed yet at the
                // forked block, inject their current code so we can just use the config
                // addresses.
                ensure_contracts_on_fork(
                    &provider,
                    &config.rpc_url,
                    &[
                        config.vault_lens_address,
                        config.utils_lens_address,
                        config.account_lens_address,
                        config.oracle_lens_address,
                    ],
                )
                .await
                .unwrap();

                (provider, Some(network))
            }
            None => (
                ProviderBuilder::new()
                    .connect_http(config.rpc_url.clone())
                    .erased(),
                None,
            ),
        };

        let vaults = &mut Vaults::new(config.vault_lens_address, config.utils_lens_address);

        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            config.account_lens_address,
            config.evc_address,
            account,
        )
        .await
        .expect("Could not fetch account");

        println!("account: {}", account.address);
        for b in &account.borrows {
            println!(
                "borrow: vault={} asset={} amount={}",
                b.vault.address, b.vault.asset, b.amount
            );
        }
        for c in &account.collaterals {
            println!(
                "collateral: vault={} asset={} amount={}",
                c.vault.erc4626().address,
                c.vault.erc4626().asset,
                c.amount
            );
        }
        println!("full: {account:#?}");

        // Health check: fetch prices for all oracles this account depends on, then
        // calculate its solvency.
        let oracles = OraclesCache::new(config.oracle_lens_address, None);
        oracles
            .ensure_prices_for(&provider, account.dependent_on())
            .await;

        let solvency = account
            .calculate_health(&oracles, vaults)
            .expect("Could not calculate account health");

        println!(
            "health: collateral_value={} borrow_value={} unit_of_account={} healthy={}",
            solvency.collateral_value,
            solvency.borrow_value,
            solvency.unit_of_account,
            solvency.is_healthy()
        );

        (account, solvency)
    }

    #[tokio::test]
    async fn fetch_and_check_single_wei_precision() {
        // Base.
        let (_, solvency) = fetch_and_log(
            "BASE_RPC",
            8453,
            None,
            address!("0xbc2053df37acd48a36a6d6b1b70a47aafc020c1c"),
        )
        .await;

        assert!(solvency.is_healthy(), "Expected the account to be healthy");
    }

    #[tokio::test]
    async fn fetch_and_check_single_borrow_and_collateral() {
        // Mainnet.
        let (account, _) = fetch_and_log(
            "MAINNET_RPC",
            1,
            Some(25679728),
            address!("0xb6cBe8b123392eF6aA72897Bb85bD6515d2e8dB6"),
        )
        .await;

        assert_eq!(account.borrows.len(), 1, "Expected exactly one borrow");
        assert_eq!(
            account.collaterals.len(),
            1,
            "Expected exactly one collateral"
        );
    }
}
