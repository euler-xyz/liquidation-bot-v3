use alloy::{
    primitives::{Address, U256},
    providers::{DynProvider, Provider},
    rpc::types::{Filter, Log},
    sol,
    sol_types::SolEvent,
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::{collections::HashSet, sync::Arc};
use tokio::{sync::mpsc::Sender, time};
use tracing::{debug, error, info};

use crate::{
    oracles::{ORACLE_PRICING_UNIT, OraclesCache},
    types::{Account, OracleIdentifier, Vault},
};

#[derive(Debug, Clone, Serialize)]
pub struct AccountSolvency {
    pub account: Address,
    pub collateral_value: U256,
    pub borrow_value: U256,
    pub unit_of_account: Address,
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

        /// @notice Emitted when the controller status is changed for an account.
        /// @param account The account for which the controller status is changed.
        /// @param controller The address of the controller.
        /// @param enabled True if the controller is enabled, false otherwise.
        event ControllerStatus(address indexed account, address indexed controller, bool enabled);


        /// @notice Emitted when the collateral status is changed for an account.
        /// @param account The account for which the collateral status is changed.
        /// @param collateral The address of the collateral.
        /// @param enabled True if the collateral is enabled, false otherwise.
        event CollateralStatus(address indexed account, address indexed collateral, bool enabled);
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
                .to_block(latest);

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

            let mut users = HashSet::new();
            for log in &logs {
                // Decode any of these events, then extract the account from it and add it to the
                // set.
                match log.topic0() {
                    Some(&Events::AccountStatusCheck::SIGNATURE_HASH) => {
                        match Events::AccountStatusCheck::decode_log(&log.inner) {
                            Ok(decoded) => users.insert(decoded.account),
                            Err(_) => continue,
                        };
                    }
                    Some(&Events::ControllerStatus::SIGNATURE_HASH) => {
                        match Events::ControllerStatus::decode_log(&log.inner) {
                            Ok(decoded) => users.insert(decoded.account),
                            Err(_) => continue,
                        };
                    }
                    Some(&Events::CollateralStatus::SIGNATURE_HASH) => {
                        match Events::CollateralStatus::decode_log(&log.inner) {
                            Ok(decoded) => users.insert(decoded.account),
                            Err(_) => continue,
                        };
                    }
                    _ => {}
                };
            }

            // Send the updates over the channel.
            for user in users.iter() {
                if let Err(err) = account_update_channel.send(*user).await {
                    error!(
                        "Issue when attempting to send update over accounts channel, it was likely dropped, err: {:?}",
                        err
                    );
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
        self.borrow_value > self.collateral_value
    }

    /// Just here to make code more readable.
    pub fn is_healthy(&self) -> bool {
        !self.is_unhealthy()
    }
}

impl Account {
    /// Get all the vaults this account has relations to.
    pub fn vaults(&self) -> Vec<Arc<Vault>> {
        let mut vaults: Vec<Arc<Vault>> =
            self.collaterals.iter().map(|a| a.vault.clone()).collect();
        vaults.extend(self.borrows.iter().map(|d| d.vault.clone()));
        vaults
    }

    pub fn dependent_on(&self) -> Vec<OracleIdentifier> {
        let debt = match self.borrows.first() {
            Some(borrow) => borrow,

            // If there is no debt then the account does not have a health score.
            None => return vec![],
        };

        // Add the asset oracles.
        let mut oracles: Vec<OracleIdentifier> = self
            .collaterals
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
        let borrow = match self.borrows.first() {
            Some(borrow) => borrow,
            None => bail!("An account with no borrow does not have a health score."),
        };

        let borrow_value = prices.get_quote(
            &OracleIdentifier {
                base_asset: borrow.vault.asset,
                quote_asset: borrow.vault.unit_of_account,
                adapter: borrow.vault.adapter,
            },
            borrow.amount,
        )?;

        let total_assets = self
            .collaterals
            .iter()
            .map(|a| {
                // Take into acccount the liquidation LTV.
                match borrow.vault.ltvs.get(&a.vault.address) {
                    Some(ltv) => {
                        // Convert the amount into shares.
                        let amount = a.amount * a.vault.shares_to_underlying_ratio / U256::from(ORACLE_PRICING_UNIT);

                        // Apply the liquidation LTV onto the underlying.
                        let amount = amount * ltv.current_liquidation_ltv() / U256::from(10_000);

                        prices.get_quote(
                            &OracleIdentifier {
                                base_asset: a.vault.asset,
                                quote_asset: borrow.vault.unit_of_account,
                                adapter: borrow .vault.adapter,
                            },
                            amount,
                        )
                    },
                    None => {
                        debug!( controller =? borrow .vault.address, asset =? a.vault.asset, "While calculating health for account we found an account with debt but the controller does not support the asset.");
                        // This asset is not supported by the controller so its value is 0.
                        Ok(U256::ZERO)
                    }
                }

            })
            .collect::<Result<Vec<U256>>>()?
            .iter()
            .sum::<U256>();

        // Calculate the asset value.
        Ok(AccountSolvency {
            account: self.address,
            collateral_value: total_assets,
            borrow_value,
            unit_of_account: borrow.vault.unit_of_account,
        })
    }
}

/// Tests that `watch_chain_for_accounts` actually catches each of the events it
/// decodes. Each test forks mainnet at a block where a specific event was
/// emitted by the EVC, runs the watcher over exactly that block, and asserts
/// that the account carried by the event arrives on the update channel.
#[cfg(test)]
mod watch_test {
    use super::*;
    use alloy::{node_bindings::Anvil, primitives::address, providers::ProviderBuilder};
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// The mainnet EVC.
    const EVC: Address = address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383");

    /// Forks mainnet at `block`, runs `watch_chain_for_accounts` starting from
    /// that block, and returns every account emitted on the channel.
    ///
    /// The watcher loops forever with a 15s sleep between passes, but the first
    /// pass runs immediately and processes the `[block, latest]` range — where
    /// `latest` is the fork block itself, so exactly `block` is queried. We run
    /// it on a background task, drain everything it emits from that first pass,
    /// then abort it.
    async fn accounts_caught_at_block(block: u64) -> HashSet<Address> {
        let mainnet_rpc = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");

        let network = Anvil::new()
            .fork(mainnet_rpc)
            .fork_block_number(block)
            .try_spawn()
            .unwrap();

        let provider = ProviderBuilder::new()
            .connect_http(network.endpoint_url())
            .erased();

        let (tx, mut rx) = mpsc::channel::<Address>(100);

        let handle =
            tokio::spawn(async move { watch_chain_for_accounts(provider, EVC, tx, block).await });

        let mut caught = HashSet::new();

        // The first message may take a while (fork spin-up + the log query). Once
        // it lands, the remaining accounts from the same pass are emitted
        // back-to-back, so a short follow-up timeout is enough to drain them.
        if let Ok(Some(first)) = tokio::time::timeout(Duration::from_secs(60), rx.recv()).await {
            caught.insert(first);
            while let Ok(Some(account)) =
                tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
            {
                caught.insert(account);
            }
        }

        handle.abort();
        caught
    }

    /// `AccountStatusCheck(address indexed account, address indexed controller)`
    /// https://etherscan.io/block/25472337
    #[tokio::test]
    async fn catches_account_status_check() {
        let expected = address!("0x0e0c281ff05D34729Cd764DcfC4Fa999b720407c");

        let caught = accounts_caught_at_block(25472337).await;

        assert!(
            caught.contains(&expected),
            "AccountStatusCheck account {expected} was not caught; got {caught:?}"
        );
    }

    /// `ControllerStatus(address indexed account, address indexed controller, bool enabled)`
    #[tokio::test]
    async fn catches_controller_status() {
        let expected = address!("0x8714D57fBBDBd202B10CaFDF562996a2ED961e10");
        let block = 25471918;

        let caught = accounts_caught_at_block(block).await;

        assert!(
            caught.contains(&expected),
            "ControllerStatus account {expected} was not caught; got {caught:?}"
        );
    }

    /// `CollateralStatus(address indexed account, address indexed collateral, bool enabled)`
    #[tokio::test]
    async fn catches_collateral_status() {
        let expected = address!("0x006D9F269695Ad9EB8f727f042EE380684332914");
        let block = 25466477;

        let caught = accounts_caught_at_block(block).await;

        assert!(
            caught.contains(&expected),
            "CollateralStatus account {expected} was not caught; got {caught:?}"
        );
    }
}

#[cfg(test)]
mod test {
    use std::{collections::HashMap, sync::Arc};

    use alloy::primitives::{Address, U256};

    use crate::{
        oracles::{ORACLE_PRICING_UNIT, OraclesCache},
        types::{
            Account, Ltv, OracleIdentifier, Vault, VaultBorrowPosition, VaultCollateralPosition,
        },
    };

    /// ORACLE_PRICING_UNIT as a U256 (1e18). A price equal to this means 1 unit of
    /// base is worth exactly 1 unit of quote.
    fn unit() -> U256 {
        U256::from(ORACLE_PRICING_UNIT)
    }

    /// Builds an LTV whose `current_liquidation_ltv()` is deterministically
    /// `liquidation_ltv` (in basis points), independent of the current time.
    ///
    /// This works because `calculate_liquidation_ltv` short-circuits and returns
    /// `liquidation_ltv` whenever `liquidation_ltv >= initial_liquidation_ltv`.
    fn fixed_ltv(bps: u64) -> Ltv {
        Ltv::new(
            Address::random(),
            U256::from(bps),
            U256::from(bps), // liquidation_ltv
            U256::from(bps), // initial_liquidation_ltv == liquidation_ltv => no ramp
            U256::ZERO,
            U256::from(1),
        )
    }

    fn vault(
        address: Address,
        asset: Address,
        unit_of_account: Address,
        adapter: Address,
        shares_to_underlying_ratio: U256,
        ltvs: HashMap<Address, Ltv>,
    ) -> Arc<Vault> {
        Arc::new(Vault {
            address,
            asset,
            unit_of_account,
            borrow_interest_rate: (),
            supply_interest_rate: (),
            shares_to_underlying_ratio,
            adapter,
            ltvs,
        })
    }

    /// Fixture returning (account, oracle-cache) for a single-collateral,
    /// single-borrow account.
    ///
    /// - `collateral_supported` controls whether the borrow controller lists the
    ///   collateral vault in its LTVs (bps 8000 = 80%). If false the collateral
    ///   is worth zero per the health logic.
    /// - prices for both the borrow and collateral oracles are seeded to `unit()`
    ///   (1:1), so the quote of an `amount` is just `amount`.
    /// - `shares_ratio` is the collateral vault's shares->underlying ratio.
    fn fixture(
        borrow_amount: U256,
        collateral_amount: U256,
        shares_ratio: U256,
        collateral_supported: bool,
    ) -> (Account, OraclesCache) {
        let uoa = Address::random();
        let adapter = Address::random();

        let borrow_asset = Address::random();
        let borrow_vault_addr = Address::random();
        let collateral_asset = Address::random();
        let collateral_vault_addr = Address::random();

        let mut ltvs = HashMap::new();
        if collateral_supported {
            ltvs.insert(collateral_vault_addr, fixed_ltv(8000));
        }

        let borrow_vault = vault(borrow_vault_addr, borrow_asset, uoa, adapter, unit(), ltvs);
        let collateral_vault = vault(
            collateral_vault_addr,
            collateral_asset,
            uoa,
            adapter,
            shares_ratio,
            HashMap::new(),
        );

        let account = Account::new(
            Address::random(),
            vec![VaultBorrowPosition {
                amount: borrow_amount,
                vault: borrow_vault,
            }],
            vec![VaultCollateralPosition {
                amount: collateral_amount,
                vault: collateral_vault,
            }],
        );

        let cache = OraclesCache::new(Address::ZERO, None);
        cache.insert_price_for_test(
            OracleIdentifier {
                base_asset: borrow_asset,
                quote_asset: uoa,
                adapter,
            },
            unit(),
        );
        cache.insert_price_for_test(
            OracleIdentifier {
                base_asset: collateral_asset,
                quote_asset: uoa,
                adapter,
            },
            unit(),
        );

        (account, cache)
    }

    #[test]
    fn unhealthy_when_borrow_exceeds_discounted_collateral() {
        // 100 borrow. 100 collateral shares at 1:1 ratio => 100 underlying, then
        // the 80% liquidation LTV discounts it to 80. 100 > 80 => unhealthy.
        let (account, cache) = fixture(U256::from(100), U256::from(100), unit(), true);

        let solvency = account.calculate_health(&cache).unwrap();

        assert_eq!(solvency.borrow_value, U256::from(100));
        assert_eq!(solvency.collateral_value, U256::from(80));
        assert!(solvency.is_unhealthy());
        assert!(!solvency.is_healthy());
    }

    #[test]
    fn healthy_when_discounted_collateral_exceeds_borrow() {
        // 100 borrow. 200 collateral shares => 200 underlying, discounted 80% => 160.
        // 100 <= 160 => healthy.
        let (account, cache) = fixture(U256::from(100), U256::from(200), unit(), true);

        let solvency = account.calculate_health(&cache).unwrap();

        assert_eq!(solvency.borrow_value, U256::from(100));
        assert_eq!(solvency.collateral_value, U256::from(160));
        assert!(solvency.is_healthy());
    }

    #[test]
    fn applies_shares_to_underlying_ratio() {
        // A 2e18 ratio means each share is worth 2 underlying. 100 shares => 200
        // underlying, discounted 80% => 160.
        let (account, cache) = fixture(
            U256::from(100),
            U256::from(100),
            unit() * U256::from(2),
            true,
        );

        let solvency = account.calculate_health(&cache).unwrap();

        assert_eq!(solvency.collateral_value, U256::from(160));
    }

    #[test]
    fn collateral_not_supported_by_controller_is_worthless() {
        // The controller does not list the collateral vault, so per the health
        // logic that collateral contributes zero value.
        let (account, cache) = fixture(U256::from(100), U256::from(100), unit(), false);

        let solvency = account.calculate_health(&cache).unwrap();

        assert_eq!(solvency.collateral_value, U256::ZERO);
        assert!(solvency.is_unhealthy());
    }

    #[test]
    fn errors_when_account_has_no_borrow() {
        let account = Account::new(
            Address::random(),
            vec![],
            vec![VaultCollateralPosition::generate_random()],
        );
        let cache = OraclesCache::new(Address::ZERO, None);

        assert!(account.calculate_health(&cache).is_err());
    }

    #[test]
    fn errors_when_a_required_price_is_missing() {
        // Same fixture, but drop one of the prices by using a fresh empty cache.
        let (account, _) = fixture(U256::from(100), U256::from(100), unit(), true);
        let empty = OraclesCache::new(Address::ZERO, None);

        assert!(account.calculate_health(&empty).is_err());
    }

    #[test]
    fn dependent_on_lists_each_collateral_plus_the_debt_oracle() {
        let uoa = Address::random();
        let adapter = Address::random();
        let debt_asset = Address::random();
        let collateral_a = Address::random();
        let collateral_b = Address::random();

        let account = Account::new(
            Address::random(),
            vec![VaultBorrowPosition {
                amount: U256::from(1),
                vault: vault(
                    Address::random(),
                    debt_asset,
                    uoa,
                    adapter,
                    unit(),
                    HashMap::new(),
                ),
            }],
            vec![
                VaultCollateralPosition {
                    amount: U256::from(1),
                    vault: vault(
                        Address::random(),
                        collateral_a,
                        Address::random(), // collateral's own uoa should be ignored
                        Address::random(), // collateral's own adapter should be ignored
                        unit(),
                        HashMap::new(),
                    ),
                },
                VaultCollateralPosition {
                    amount: U256::from(1),
                    vault: vault(
                        Address::random(),
                        collateral_b,
                        Address::random(),
                        Address::random(),
                        unit(),
                        HashMap::new(),
                    ),
                },
            ],
        );

        let deps = account.dependent_on();

        // One oracle per collateral, plus the debt oracle.
        assert_eq!(deps.len(), 3);

        // Collateral oracles must be quoted against the DEBT's unit of account and
        // adapter, not the collateral vault's own.
        assert!(deps.contains(&OracleIdentifier {
            base_asset: collateral_a,
            quote_asset: uoa,
            adapter,
        }));
        assert!(deps.contains(&OracleIdentifier {
            base_asset: collateral_b,
            quote_asset: uoa,
            adapter,
        }));

        // The debt oracle is the last entry.
        assert_eq!(
            deps.last().unwrap(),
            &OracleIdentifier {
                base_asset: debt_asset,
                quote_asset: uoa,
                adapter,
            }
        );
    }

    #[test]
    fn dependent_on_is_empty_without_a_borrow() {
        let account = Account::new(
            Address::random(),
            vec![],
            vec![VaultCollateralPosition::generate_random()],
        );

        assert!(account.dependent_on().is_empty());
    }
}
