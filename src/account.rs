use alloy::{
    primitives::{Address, U256},
    providers::{DynProvider, Provider},
    rpc::types::{Filter, Log},
    sol,
    sol_types::SolEvent,
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::collections::HashSet;
use tokio::{sync::broadcast::Sender, time};
use tracing::{debug, error};

use crate::{
    oracles::{ORACLE_PRICING_UNIT, OraclesCache},
    types::{Account, OracleIdentifier, Vault},
    vaults::Vaults,
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

/// The maximum number of blocks we query logs for in a single `eth_getLogs` call. Providers
/// reject queries spanning too many blocks, so after a gap (e.g. an RPC outage) we catch up in
/// chunks of at most this size.
const MAX_LOG_RANGE: u64 = 1_000;

/// How many already processed blocks we re-query on every pass, so events from blocks that got
/// reorged out (and replaced) are not lost. Re-processing a block only re-sends the accounts in
/// it, which is harmless.
const REORG_MARGIN: u64 = 10;

/// Watches the chain for account update events from the most recent block.
pub async fn watch_chain_for_accounts_from_latest(
    provider: DynProvider,
    evc: Address,
    account_update_channel: Sender<Address>,
) {
    // NOTE: We have to keep retrying here, falling back to some default block (e.g. 0) would
    // make us query a range that is way too large.
    let latest = loop {
        match provider.get_block_number().await {
            Ok(latest) => break latest,
            Err(err) => {
                error!("Error while fetching the current block number: {err}");
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }
    };

    watch_chain_for_accounts(provider, evc, account_update_channel, latest).await
}

/// Determines the next (inclusive) block range to query logs for, given the last block we fully
/// processed and the latest block. Returns `None` if there are no new blocks.
///
/// The range always spans at most `chunk` blocks and always includes at least one new block, so
/// every successful query makes progress. Within that, it re-queries up to [`REORG_MARGIN`]
/// already processed blocks. When the range is capped by `chunk` (i.e. while catching up after a
/// gap) the margin is dropped, as those blocks are not near the head anyway.
fn next_range(processed_up_to: Option<u64>, latest: u64, chunk: u64) -> Option<(u64, u64)> {
    let first_unprocessed = match processed_up_to {
        Some(processed) if processed >= latest => return None,
        Some(processed) => processed + 1,
        None => 0,
    };

    let chunk = chunk.max(1);
    let to = latest.min(first_unprocessed.saturating_add(chunk - 1));
    let from = first_unprocessed
        .saturating_sub(REORG_MARGIN)
        .max((to + 1).saturating_sub(chunk));

    Some((from, to))
}

/// Watches the chain for account update events, starting at (and including) `from_block`.
pub async fn watch_chain_for_accounts(
    provider: DynProvider,
    evc: Address,
    account_update_channel: Sender<Address>,
    from_block: u64,
) {
    // The last block for which we have processed all logs.
    let mut processed_up_to = from_block.checked_sub(1);
    let mut chunk = MAX_LOG_RANGE;

    loop {
        let latest = match provider.get_block_number().await {
            Ok(latest) => latest,
            Err(err) => {
                error!("Error while fetching the current block number: {err}");
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        // Process chunks back-to-back until we have caught up with the latest block.
        while let Some((from, to)) = next_range(processed_up_to, latest, chunk) {
            let filter = Filter::new().address(evc).from_block(from).to_block(to);

            let logs: Vec<Log> = match provider.get_logs(&filter).await {
                Ok(logs) => logs,
                Err(err) => {
                    // The provider may have a lower block range limit than we use, so we retry
                    // the same position with a smaller chunk.
                    chunk = (chunk / 2).max(1);
                    error!(
                        "Error while fetching logs from block range {}-{}, retrying with chunks of {} blocks: {err}",
                        from, to, chunk
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

            emit_accounts(&logs, &account_update_channel);

            // Advance past the range we just queried.
            processed_up_to = Some(to);
        }

        // We caught up, so the next pass can use the full chunk size again.
        chunk = MAX_LOG_RANGE;

        // TODO: Make duration configurable, perhaps also an option to watch for new block events.
        time::sleep(tokio::time::Duration::from_secs(15)).await;
    }
}

/// Decodes the accounts from the EVC logs and sends each of them over the channel once.
fn emit_accounts(logs: &[Log], account_update_channel: &Sender<Address>) {
    let mut users = HashSet::new();
    for log in logs {
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

    // Send the updates over the channel. A broadcast send never blocks: if the buffer
    // is full the oldest event is overwritten (and reconciled by the next full resync),
    // so a stalled consumer can never stall this watcher. It only errors when there is
    // no receiver at all.
    for user in users.iter() {
        if let Err(err) = account_update_channel.send(*user) {
            error!(
                "Issue when attempting to send update over accounts channel, the receiver was likely dropped, err: {:?}",
                err
            );
        }
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
    pub fn vaults(&self) -> Vec<Vault> {
        let mut vaults: Vec<Vault> = self.collaterals.iter().map(|a| a.vault.clone()).collect();
        vaults.extend(
            self.borrows
                .iter()
                .map(|d| Vault::EVault(d.vault.clone())),
        );
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
                base_asset: asset.vault.erc4626().asset,
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

    pub fn calculate_health(
        &self,
        prices: &OraclesCache,
        vaults: &Vaults,
    ) -> Result<AccountSolvency> {
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

        // The LTVs are read from the live cache (kept fresh in the background by
        // `poll_vault_ltvs`), not from the `EVault` snapshot embedded on this account, as
        // governance can change them at any time.
        let cached_ltvs = vaults.cached_ltvs(borrow.vault.address);
        let ltvs = cached_ltvs.as_deref().unwrap_or(&borrow.vault.ltvs);

        let total_assets = self
            .collaterals
            .iter()
            .map(|a| {
                // Take into acccount the liquidation LTV.
                match ltvs.get(&a.vault.erc4626().address) {
                    Some(ltv) => {
                        // Convert the amount into shares. The ratio is read from the
                        // live cache (kept fresh in the background by
                        // `poll_vault_shares`), not from the value embedded on this
                        // `Vault` snapshot when it was first fetched. This is a plain
                        // in-memory lookup, never a chain call.
                        let ratio = vaults
                            .cached_shares_to_underlying_ratio(a.vault.erc4626().address)
                            .unwrap_or_else(|| {
                                debug!(
                                    vault =? a.vault.erc4626().address,
                                    "No cached shares_to_underlying ratio for vault, falling back to the ratio recorded when it was first fetched"
                                );
                                a.vault.erc4626().shares_to_underlying_ratio
                            });
                        let amount = a.amount * ratio / U256::from(ORACLE_PRICING_UNIT);

                        // Convert the amount into the unit_of_account.
                        prices.get_quote(
                            &OracleIdentifier {
                                base_asset: a.vault.erc4626().asset,
                                quote_asset: borrow.vault.unit_of_account,
                                adapter: borrow .vault.adapter,
                            },
                            amount,
                        ).map(|amount| amount * ltv.current_liquidation_ltv() / U256::from(10_000))
                    },
                    None => {
                        debug!( controller =? borrow .vault.address, asset =? a.vault.erc4626().asset, "While calculating health for account we found an account with debt but the controller does not support the asset.");
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
mod next_range_test {
    use super::{MAX_LOG_RANGE, REORG_MARGIN, next_range};

    #[test]
    fn nothing_new_returns_none() {
        assert_eq!(next_range(Some(100), 100, MAX_LOG_RANGE), None);
        // The provider may briefly report an older head (e.g. a lagging node).
        assert_eq!(next_range(Some(100), 99, MAX_LOG_RANGE), None);
    }

    #[test]
    fn a_large_gap_is_capped_at_the_chunk_size() {
        let (from, to) = next_range(Some(10_000), 1_000_000, MAX_LOG_RANGE).unwrap();
        assert_eq!(from, 10_001);
        assert_eq!(to - from + 1, MAX_LOG_RANGE);
    }

    #[test]
    fn always_makes_progress_even_with_a_chunk_smaller_than_the_reorg_margin() {
        // After repeated failures the chunk can shrink below the reorg margin. The range must
        // still end past the last processed block, or the watcher would never advance.
        for chunk in 1..=REORG_MARGIN + 1 {
            let (from, to) = next_range(Some(10_000), 1_000_000, chunk).unwrap();
            assert!(to > 10_000, "chunk {chunk} made no progress: {from}-{to}");
            assert!(to - from + 1 <= chunk, "chunk {chunk} exceeded: {from}-{to}");
        }
    }

    #[test]
    fn a_new_block_re_queries_the_reorg_margin() {
        assert_eq!(
            next_range(Some(1_000), 1_001, MAX_LOG_RANGE),
            Some((1_001 - REORG_MARGIN, 1_001))
        );
    }

    #[test]
    fn does_not_underflow_near_genesis() {
        assert_eq!(next_range(None, 5, MAX_LOG_RANGE), Some((0, 5)));
        assert_eq!(next_range(Some(2), 3, MAX_LOG_RANGE), Some((0, 3)));
        // A zero chunk is treated as a single block.
        assert_eq!(next_range(None, 5, 0), Some((0, 0)));
    }
}

#[cfg(test)]
mod watch_test {
    use super::*;
    use alloy::{node_bindings::Anvil, primitives::address, providers::ProviderBuilder};
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::sync::broadcast;

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

        let (tx, mut rx) = broadcast::channel::<Address>(512);

        let handle =
            tokio::spawn(async move { watch_chain_for_accounts(provider, EVC, tx, block).await });

        let mut caught = HashSet::new();

        // The first message may take a while (fork spin-up + the log query). Once
        // it lands, the remaining accounts from the same pass are emitted
        // back-to-back, so a short follow-up timeout is enough to drain them.
        if let Ok(Ok(first)) = tokio::time::timeout(Duration::from_secs(60), rx.recv()).await {
            caught.insert(first);
            while let Ok(Ok(account)) =
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
            Account, EVault, Erc4626Vault, Ltv, OracleIdentifier, Vault, VaultBorrowPosition,
            VaultCollateralPosition,
        },
        vaults::Vaults,
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
    ) -> Arc<EVault> {
        Arc::new(EVault {
            erc4626: Erc4626Vault {
                address,
                asset,
                shares_to_underlying_ratio,
            },
            unit_of_account,
            borrow_interest_rate: (),
            supply_interest_rate: (),
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
    ) -> (Account, OraclesCache, Vaults) {
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
                vault: Vault::EVault(collateral_vault),
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

        // `calculate_health` reads the collateral's shares_to_underlying ratio from the
        // live `Vaults` cache, not from the value embedded on the `Vault` snapshot
        // above (that embedded value only reflects whatever it was when first
        // fetched). Seed the cache here so these tests exercise the same `shares_ratio`
        // they did before this cache was introduced.
        let vaults = Vaults::new(Address::random(), Address::random());
        vaults.insert_ratio_for_test(collateral_vault_addr, shares_ratio);

        (account, cache, vaults)
    }

    #[test]
    fn unhealthy_when_borrow_exceeds_discounted_collateral() {
        // 100 borrow. 100 collateral shares at 1:1 ratio => 100 underlying, then
        // the 80% liquidation LTV discounts it to 80. 100 > 80 => unhealthy.
        let (account, cache, vaults) = fixture(U256::from(100), U256::from(100), unit(), true);

        let solvency = account.calculate_health(&cache, &vaults).unwrap();

        assert_eq!(solvency.borrow_value, U256::from(100));
        assert_eq!(solvency.collateral_value, U256::from(80));
        assert!(solvency.is_unhealthy());
        assert!(!solvency.is_healthy());
    }

    #[test]
    fn healthy_when_discounted_collateral_exceeds_borrow() {
        // 100 borrow. 200 collateral shares => 200 underlying, discounted 80% => 160.
        // 100 <= 160 => healthy.
        let (account, cache, vaults) = fixture(U256::from(100), U256::from(200), unit(), true);

        let solvency = account.calculate_health(&cache, &vaults).unwrap();

        assert_eq!(solvency.borrow_value, U256::from(100));
        assert_eq!(solvency.collateral_value, U256::from(160));
        assert!(solvency.is_healthy());
    }

    #[test]
    fn applies_shares_to_underlying_ratio() {
        // A 2e18 ratio means each share is worth 2 underlying. 100 shares => 200
        // underlying, discounted 80% => 160.
        let (account, cache, vaults) = fixture(
            U256::from(100),
            U256::from(100),
            unit() * U256::from(2),
            true,
        );

        let solvency = account.calculate_health(&cache, &vaults).unwrap();

        assert_eq!(solvency.collateral_value, U256::from(160));
    }

    #[test]
    fn calculate_health_uses_the_live_cached_ratio_not_the_fetch_time_snapshot() {
        // Seed the fixture with a 1:1 ratio embedded on the `Vault` snapshot...
        let (account, cache, vaults) = fixture(U256::from(100), U256::from(100), unit(), true);

        // ...then simulate the background poller (`poll_vault_shares`) refreshing the
        // cache to a new ratio, without touching the `Account`/`Vault` snapshot at all.
        let collateral_vault_addr = account.collaterals.first().unwrap().vault.erc4626().address;
        vaults.insert_ratio_for_test(collateral_vault_addr, unit() * U256::from(2));

        let solvency = account.calculate_health(&cache, &vaults).unwrap();

        // 100 shares at the *updated* 2:1 ratio => 200 underlying, discounted 80% =>
        // 160. If `calculate_health` were still reading the stale embedded snapshot
        // (1:1) this would be 80 instead.
        assert_eq!(solvency.collateral_value, U256::from(160));
    }

    #[test]
    fn calculate_health_uses_the_live_cached_ltvs_not_the_fetch_time_snapshot() {
        // The borrow vault snapshot lists the collateral at an 80% liquidation LTV...
        let (account, cache, vaults) = fixture(U256::from(100), U256::from(100), unit(), true);

        // ...then simulate a refresh picking up a governance change that lowered it to 50%,
        // without touching the `Account`/`Vault` snapshot at all.
        let controller = account.borrows.first().unwrap().vault.address;
        let collateral_vault_addr = account.collaterals.first().unwrap().vault.erc4626().address;
        vaults.insert_ltvs_for_test(
            controller,
            HashMap::from([(collateral_vault_addr, fixed_ltv(5000))]),
        );

        let solvency = account.calculate_health(&cache, &vaults).unwrap();

        // 100 collateral at the *updated* 50% LTV => 50. With the stale snapshot (80%) this
        // would be 80.
        assert_eq!(solvency.collateral_value, U256::from(50));
    }

    #[test]
    fn calculate_health_values_a_collateral_added_after_the_snapshot() {
        // The borrow vault snapshot does not recognize the collateral at all...
        let (account, cache, vaults) = fixture(U256::from(100), U256::from(200), unit(), false);

        // ...but the controller has since added it at an 80% liquidation LTV.
        let controller = account.borrows.first().unwrap().vault.address;
        let collateral_vault_addr = account.collaterals.first().unwrap().vault.erc4626().address;
        vaults.insert_ltvs_for_test(
            controller,
            HashMap::from([(collateral_vault_addr, fixed_ltv(8000))]),
        );

        let solvency = account.calculate_health(&cache, &vaults).unwrap();

        // 200 collateral at 80% => 160, which covers the 100 borrow. With the stale snapshot
        // the collateral would be worth 0 and the account would look unhealthy.
        assert_eq!(solvency.collateral_value, U256::from(160));
        assert!(solvency.is_healthy());
    }

    #[test]
    fn collateral_not_supported_by_controller_is_worthless() {
        // The controller does not list the collateral vault, so per the health
        // logic that collateral contributes zero value.
        let (account, cache, vaults) = fixture(U256::from(100), U256::from(100), unit(), false);

        let solvency = account.calculate_health(&cache, &vaults).unwrap();

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
        let vaults = Vaults::new(Address::random(), Address::random());

        assert!(account.calculate_health(&cache, &vaults).is_err());
    }

    #[test]
    fn errors_when_a_required_price_is_missing() {
        // Same fixture, but drop one of the prices by using a fresh empty cache.
        let (account, _, vaults) = fixture(U256::from(100), U256::from(100), unit(), true);
        let empty = OraclesCache::new(Address::ZERO, None);

        assert!(account.calculate_health(&empty, &vaults).is_err());
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
                    vault: Vault::EVault(vault(
                        Address::random(),
                        collateral_a,
                        Address::random(), // collateral's own uoa should be ignored
                        Address::random(), // collateral's own adapter should be ignored
                        unit(),
                        HashMap::new(),
                    )),
                },
                VaultCollateralPosition {
                    amount: U256::from(1),
                    vault: Vault::EVault(vault(
                        Address::random(),
                        collateral_b,
                        Address::random(),
                        Address::random(),
                        unit(),
                        HashMap::new(),
                    )),
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
