use std::{collections::HashMap, sync::Arc};

use alloy::{
    dyn_abi::SolType,
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{CallItemBuilder, DynProvider, Provider},
    sol,
};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_with::TimestampSeconds;
use serde_with::serde_as;
use tokio::sync::broadcast::Sender;
use tracing::{debug, error, info, warn};

use crate::{
    config::PythConfig,
    pyth::{
        Pyth::{self},
        fetch_pyth_data,
    },
    types::OracleIdentifier,
};

pub const ORACLE_PRICING_UNIT: i64 = 1000000000000000000;

/// How many oracles we re-resolve concurrently during a [`OraclesCache::refresh_all_types`] pass.
const TYPE_REFRESH_CONCURRENCY: usize = 16;

#[derive(Debug, Clone)]
pub struct OraclesCache {
    lens: Address,

    // Not all chains have a pyth deployment.
    pyth: Option<PythConfig>,

    // The oracles that should be actively tracked.
    active_oracles: Arc<DashSet<OracleIdentifier>>,

    // The resolved oracle types.
    oracles: Arc<DashMap<OracleIdentifier, Oracle>>,

    // The oracle outputs.
    prices: Arc<DashMap<OracleIdentifier, OracleOutput>>,
}

impl OraclesCache {
    pub fn new(oracle_lens: Address, pyth: Option<PythConfig>) -> Self {
        OraclesCache {
            lens: oracle_lens,
            pyth,
            active_oracles: Arc::new(DashSet::new()),
            oracles: Arc::new(DashMap::new()),
            prices: Arc::new(DashMap::new()),
        }
    }

    /// Ensures that we have a price for all of the oracles, as long as they are reporting a price.
    pub async fn ensure_prices_for(&self, provider: &DynProvider, ids: Vec<OracleIdentifier>) {
        // Filter out the ones where we already have a price.
        let new_ids: Vec<OracleIdentifier> = ids
            .iter()
            .filter(|id| self.active_oracles.insert((*id).clone()))
            .cloned()
            .collect();

        // For these new ids we fetch their types and prices.
        for id in new_ids.iter() {
            let result = self.fetch_latest_price(provider, id.clone()).await;
            if let Err(err) = result {
                tracing::warn!("Could not fetch price for {:?}, err: {:?}", id, err);
            }
        }
    }

    pub async fn fetch_type(&self, provider: &DynProvider, id: OracleIdentifier) -> Result<Oracle> {
        // Check if we have this id cached.
        match self.oracles.get(&id) {
            Some(oracle) => Ok(oracle.clone()),
            None => {
                // Resolve the identifier.
                let oracle = id.resolve(provider, self.lens).await.context(format!(
                    "While fetching adapter {} with base {} and quote {} using lens {}",
                    id.adapter, id.base_asset, id.quote_asset, self.lens
                ))?;

                // Store the result.
                self.oracles.insert(id, oracle.clone());
                Ok(oracle)
            }
        }
    }

    /// Re-resolves the type of every cached oracle and updates the cache, so router config
    /// changes (e.g. a pair moving to a Pyth adapter or a different feed) are picked up. A
    /// failure for one oracle is logged and does not affect the others: the previously cached
    /// type is simply left in place until the next successful refresh.
    pub async fn refresh_all_types(&self, provider: &DynProvider) {
        let ids: Vec<OracleIdentifier> = self.oracles.iter().map(|o| o.key().clone()).collect();

        stream::iter(ids)
            .map(|id| async move {
                let oracle = match id.resolve(provider, self.lens).await {
                    Ok(oracle) => oracle,
                    Err(err) => {
                        warn!(
                            oracle =? id,
                            err =? err,
                            "Could not refresh the oracle type, keeping the previously cached value"
                        );
                        return;
                    }
                };

                let previous_pyth_ids = self.oracles.get(&id).map(|o| o.pyth_ids());
                if previous_pyth_ids.is_some_and(|prev| prev != oracle.pyth_ids()) {
                    info!(
                        oracle =? id,
                        new =? oracle,
                        "The Pyth feeds of oracle have changed"
                    );
                }

                self.oracles.insert(id, oracle);
            })
            .buffer_unordered(TYPE_REFRESH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
    }

    /// Calculates the quote based on the most recent price.
    pub fn get_quote(&self, oracle: &OracleIdentifier, amount: U256) -> Result<U256> {
        let price = match self.prices.get(oracle) {
            Some(price) => price.value().price,
            None => {
                match self.active_oracles.insert(oracle.clone()) {
                    false => {
                        debug!(
                            oracle =? oracle,
                            "Due to missing price data we were not able to calculate a quote for this oracle"
                        );
                        bail!("Missing oracle price")
                    }
                    true => {
                        // We do not have a price for this. We add it to the active oracles, so next polling
                        // cycle we will fetch a price.
                        //
                        // This is an edge-case and should already not happen, but this way we will
                        // eventually have all prices we need.
                        warn!(
                            "Missing price for oracle {:?}, adding it to active oracles",
                            oracle
                        );
                        bail!("No price available for this oracle as we were not tracking it")
                    }
                }
            }
        };

        Ok((amount * price).div_ceil(U256::from(ORACLE_PRICING_UNIT)))
    }

    pub async fn fetch_latest_price(
        &self,
        provider: &DynProvider,
        id: OracleIdentifier,
    ) -> Result<OracleOutput> {
        // Fetch the price.
        let price = match self.fetch_price(provider, id.clone()).await {
            Ok(price) => price,
            Err(err) => {
                // Add it to the active_oracles so we will be attempting to fetch the price next
                // time around.
                self.active_oracles.insert(id);
                return Err(err);
            }
        };

        let new_price = match self.prices.get(&id) {
            // We had a prev price and it has changed since last check.
            Some(prev) if prev.price != price => OracleOutput {
                price,
                last_polled_at: Utc::now(),
                last_changed_at: Utc::now(),
            },

            // We did have a previous price but it has not changed since.
            Some(prev) => OracleOutput {
                price,
                last_polled_at: Utc::now(),
                last_changed_at: prev.last_changed_at,
            },

            None => {
                // We did not have a previous price.
                self.active_oracles.insert(id.clone());
                OracleOutput {
                    price,
                    last_polled_at: Utc::now(),
                    last_changed_at: Utc::now(),
                }
            }
        };

        // Cache the price.
        self.prices.insert(id.clone(), new_price.clone());
        Ok(new_price)
    }

    /// Get the oracles that are actively being used.
    pub fn active_oracles(&self) -> Vec<OracleIdentifier> {
        // For simplicity on the consumer side of this function we turn it into a regular vector.
        self.active_oracles
            .iter()
            .map(|item| item.clone())
            .collect()
    }

    // TODO: Determine if this is the correct place for this method to live.
    async fn fetch_price(&self, provider: &DynProvider, id: OracleIdentifier) -> Result<U256> {
        // Build the call we will eventually perform.
        let adapter = IPriceOracle::new(id.adapter, provider);
        let adapter_call = adapter.getQuote(
            U256::from(ORACLE_PRICING_UNIT),
            id.base_asset,
            id.quote_asset,
        );

        // Fetch the oracle either from the chain or from the cache, we need this to determine how
        // to fetch the price.
        let oracle = self.fetch_type(provider, id.clone()).await?;

        // Check to see if this oracle uses pyth.
        let pyth_ids = oracle.pyth_ids();

        // If it has no pyth dependencies then we can fetch the price from the chain.
        if pyth_ids.is_empty() {
            return Ok(adapter_call.call().await?);
        }

        // NOTE: handle the edge-case in which no pyth is configured, but we did find a pyth oracle.
        let pyth = match self.pyth.clone() {
            Some(pyth) => pyth,
            None => {
                error!(
                    "We found a pyth oracle but this chain does not have a pyth deployment configured, this should never happen!"
                );
                return Ok(adapter_call.call().await?);
            }
        };

        let pyth_call = match fetch_pyth_data(provider, pyth.clone(), pyth_ids).await {
            Ok(data) => {
                CallItemBuilder::new(Pyth::new(pyth.address, provider).updatePriceFeeds(data.data))
                    .value(data.cost)
            }
            // If the API call fails, then we will try to fetch the data from the chain anyway.
            // Perhaps it is still up-to-date.
            Err(e) => {
                error!("Pyth api error {}", e);
                return adapter_call.call().await.context("After failing to get the pyth update data from the api we attempted to call the adapter and failed");
            }
        };

        // We are going to simulate updating the oracles and then calling the adapter to fetch the
        // output.
        let (_, price) = provider
            .multicall()
            .add_call(pyth_call)
            .add(adapter_call)
            .aggregate3_value()
            .await?;

        price.map_err(|e| {
            anyhow!(
                "Error fetching the price through the pyth multicall, err: {:?}",
                e
            )
        })
    }

    pub fn all(&self) -> Vec<OracleInformation> {
        self.oracles
            .iter()
            .map(|o| OracleInformation {
                identifier: o.key().clone(),
                oracle: o.value().clone(),
                price: self.prices.get(o.key()).map(|p| p.value().clone()),
            })
            .collect()
    }

    /// Test-only helper to seed the price cache without hitting the chain.
    #[cfg(test)]
    pub(crate) fn insert_price_for_test(&self, id: OracleIdentifier, price: U256) {
        self.prices.insert(
            id,
            OracleOutput {
                price,
                last_polled_at: Utc::now(),
                last_changed_at: Utc::now(),
            },
        );
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OracleInformation {
    pub identifier: OracleIdentifier,
    pub oracle: Oracle,
    pub price: Option<OracleOutput>,
}

#[derive(Debug, Clone)]
pub struct OracleChange {
    pub oracle: OracleIdentifier,
    pub price: U256,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleOutput {
    // The price as outputted by the oracle.
    price: U256,

    // Most recent succesfull check of the price.
    #[serde_as(as = "TimestampSeconds<i64>")]
    last_polled_at: DateTime<Utc>,

    // Last price change that we have seen.
    #[serde_as(as = "TimestampSeconds<i64>")]
    last_changed_at: DateTime<Utc>,
}

/// Periodically re-resolves the type of every oracle the bot has resolved so far, see
/// [`OraclesCache::refresh_all_types`].
pub async fn poll_oracle_types(
    provider: DynProvider,
    oracles: OraclesCache,
    interval: tokio::time::Duration,
) -> Result<()> {
    loop {
        tokio::time::sleep(interval).await;

        info!("Refreshing the oracle types for {} oracles", oracles.oracles.len());

        oracles.refresh_all_types(&provider).await;
    }
}

pub async fn poll_oracles(
    provider: DynProvider,
    oracles: OraclesCache,
    interval: tokio::time::Duration,
    event_channel: Sender<Vec<OracleChange>>,
) -> Result<()> {
    // Track the most recent prices, this is used to notify the main thread on price changes.
    let mut prices: HashMap<OracleIdentifier, OracleOutput> = HashMap::new();

    loop {
        tokio::time::sleep(interval).await;
        let active_oracles = oracles.active_oracles();

        if active_oracles.is_empty() {
            continue;
        }

        info!(
            "Checking {} oracles for price changes",
            active_oracles.len()
        );

        let mut changes = Vec::new();
        for oracle in active_oracles.iter() {
            // Poll the oracle.
            let new_price = match oracles.fetch_latest_price(&provider, oracle.clone()).await {
                Ok(price) => price,
                Err(e) => {
                    warn!(
                        "Error while fetching price for oracle {}: {} -> {}: {e}",
                        oracle.adapter, oracle.base_asset, oracle.quote_asset
                    );
                    continue;
                }
            };

            let prev = prices.get(oracle);

            match prev {
                // If the price did not change.
                Some(prev) if prev.price == new_price.price => {
                    // Update out store.
                    prices.insert(
                        oracle.clone(),
                        OracleOutput {
                            price: prev.price,
                            last_polled_at: Utc::now(),
                            last_changed_at: prev.last_changed_at,
                        },
                    );

                    continue;
                }
                _ => {
                    debug!(
                        new_price =? new_price,
                        "Oracle {}: {} -> {} its price has updated",
                        oracle.adapter,
                        oracle.base_asset,
                        oracle.quote_asset
                    );

                    // Update out store.
                    prices.insert(
                        oracle.clone(),
                        OracleOutput {
                            price: new_price.price,
                            last_polled_at: Utc::now(),
                            last_changed_at: Utc::now(),
                        },
                    );

                    // Track this as having changed.
                    changes.push(OracleChange {
                        oracle: oracle.clone(),
                        price: new_price.price,
                    });
                }
            };
        }

        // Notify the main thread of the price changes, if any. A broadcast send never blocks:
        // if the buffer is full the oldest batch is overwritten (and reconciled by the next
        // full resync), so a stalled consumer can never stall this watcher. It only errors
        // when there is no receiver at all.
        if !changes.is_empty() {
            event_channel.send(changes).map_err(|_| {
                anyhow!("Oracle update channel has no receivers, the main loop is gone.")
            })?;
        }
    }
}

sol! {
    /// @title IPriceOracle
    /// @custom:security-contact security@euler.xyz
    /// @author Euler Labs (https://www.eulerlabs.com/)
    /// @notice Common PriceOracle interface.
    #[sol(rpc)]
    interface IPriceOracle {
        /// @notice Get the name of the oracle.
        /// @return The name of the oracle.
        function name() external view returns (string memory);

        /// @notice One-sided price: How much quote token you would get for inAmount of base token, assuming no price spread.
        /// @param inAmount The amount of `base` to convert.
        /// @param base The token that is being priced.
        /// @param quote The token that is the unit of account.
        /// @return outAmount The amount of `quote` that is equivalent to `inAmount` of `base`.
        function getQuote(uint256 inAmount, address base, address quote) external view returns (uint256 outAmount);

        /// @notice Two-sided price: How much quote token you would get/spend for selling/buying inAmount of base token.
        /// @param inAmount The amount of `base` to convert.
        /// @param base The token that is being priced.
        /// @param quote The token that is the unit of account.
        /// @return bidOutAmount The amount of `quote` you would get for selling `inAmount` of `base`.
        /// @return askOutAmount The amount of `quote` you would spend for buying `inAmount` of `base`.
        function getQuotes(uint256 inAmount, address base, address quote)
            external
            view
            returns (uint256 bidOutAmount, uint256 askOutAmount);
    }

    #[sol(rpc)]
    interface OracleLens {
        function getOracleInfo(address oracleAddress, address[] memory bases, address[] memory quotes)
            public
            view
            returns (OracleDetailedInfo memory);
    }

    struct OracleDetailedInfo {
        address oracle;
        string name;
        bytes oracleInfo;
    }

    struct PythOracleInfo {
        address pyth;
        address base;
        address quote;
        bytes32 feedId;
        uint256 maxStaleness;
        uint256 maxConfWidth;
    }

    struct EulerRouterInfo {
        address governor;
        address fallbackOracle;
        OracleDetailedInfo fallbackOracleInfo;
        address[] bases;
        address[] quotes;
        address[][] resolvedAssets;
        address[] resolvedOracles;
        OracleDetailedInfo[] resolvedOraclesInfo;
    }

    struct CrossAdapterInfo {
        address base;
        address cross;
        address quote;
        address oracleBaseCross;
        address oracleCrossQuote;
        OracleDetailedInfo oracleBaseCrossInfo;
        OracleDetailedInfo oracleCrossQuoteInfo;
    }

}

impl OracleIdentifier {
    // Figure out the type of oracle that this is.
    pub async fn resolve(&self, provider: &DynProvider, lens: Address) -> Result<Oracle> {
        // We use the OracleLens to get the type of oracle.
        let lens = OracleLens::new(lens, provider);

        let info = lens
            .getOracleInfo(self.adapter, vec![self.base_asset], vec![self.quote_asset])
            .call()
            .await?;

        Oracle::new(info.oracle, info.name, info.oracleInfo)
    }
}

impl Oracle {
    pub fn new(address: Address, name: String, oracle_info: Bytes) -> Result<Self> {
        let oracle_type = match name.as_str() {
            "EulerRouter" => {
                // NOTE: since the `oracle_info` only every gets called for a single base_asset and
                // quote_asset, we assume the EulerRouter will also only ever return a single
                // oracle.
                let router_info = EulerRouterInfo::abi_decode(&oracle_info)?;

                let info = router_info.resolvedOraclesInfo.first().ok_or(anyhow!(
                    "Euler router did not return any resolved oracles, this should never happen"
                ))?;

                return Oracle::new(info.oracle, info.name.clone(), info.oracleInfo.clone());
            }
            "CrossAdapter" => {
                let cross_info = CrossAdapterInfo::abi_decode(&oracle_info)?;

                OracleType::CrossAdapter {
                    base: Box::new(Oracle::new(
                        cross_info.oracleBaseCrossInfo.oracle,
                        cross_info.oracleBaseCrossInfo.name,
                        cross_info.oracleBaseCrossInfo.oracleInfo,
                    )?),
                    cross: Box::new(Oracle::new(
                        cross_info.oracleCrossQuoteInfo.oracle,
                        cross_info.oracleCrossQuoteInfo.name,
                        cross_info.oracleCrossQuoteInfo.oracleInfo,
                    )?),
                }
            }
            "PythOracle" => {
                let pyth_data = PythOracleInfo::abi_decode(&oracle_info)?;
                OracleType::Pyth {
                    id: pyth_data.feedId,
                }
            }

            _ => OracleType::Generic,
        };

        Ok(Oracle {
            name,
            address,
            oracle_type,
        })
    }

    /// Returns all the pyth ids for this oracle.
    pub fn pyth_ids(&self) -> Vec<FixedBytes<32>> {
        match self.oracle_type.clone() {
            OracleType::Pyth { id } => {
                vec![id]
            }
            OracleType::CrossAdapter { base, cross } => {
                [base.pyth_ids(), cross.pyth_ids()].concat()
            }
            OracleType::Generic => vec![],
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[allow(dead_code)]
pub struct Oracle {
    name: String,
    address: Address,
    oracle_type: OracleType,
}

#[derive(Clone, Debug, Default, Serialize)]
pub enum OracleType {
    // This is a pyth push oracle, it requires us to update the price onchain.
    Pyth {
        id: FixedBytes<32>,
    },

    // This uses two other oracles.
    CrossAdapter {
        base: Box<Oracle>,
        cross: Box<Oracle>,
    },

    #[default]
    /// This type means there is no special handling required for this oracle.
    Generic,
}

#[cfg(test)]
mod test {
    use alloy::{
        primitives::{Address, address},
        providers::{Provider, ProviderBuilder},
    };

    use crate::{
        config::PythConfig,
        oracles::{OracleType, OraclesCache},
        pyth::DEFAULT_PYTH_ENDPOINT,
        types::OracleIdentifier,
    };

    const MAINNET_RPC_ENDPOINT: &str = "https://eth.rpc.blxrbdn.com";
    const MAINNET_ORACLE_LENS: Address = address!("0x30E6dFB84782A31d561536f64F47231451F7b48A");
    const MAINNET_PYTH: Address = address!("0x4305FB66699C3B2702D4d05CF36551390A4c69C6");

    #[tokio::test]
    async fn identify_pyth_oracle() {
        let oracle = OracleIdentifier {
            base_asset: address!("0x96F6eF951840721AdBF46Ac996b59E0235CB985C"),
            quote_asset: address!("0x0000000000000000000000000000000000000348"),
            adapter: address!("0xfe3ED784f0244B24Df186e576313d682f6Ee9865"),
        };

        let provider = ProviderBuilder::new()
            .connect_http(MAINNET_RPC_ENDPOINT.parse().unwrap())
            .erased();

        let result = oracle
            .resolve(&provider, MAINNET_ORACLE_LENS)
            .await
            .unwrap();

        if let OracleType::Pyth { .. } = result.oracle_type {
        } else {
            panic!("Result is not a Pyth oracle");
        }
    }

    #[tokio::test]
    async fn refresh_all_types_picks_up_a_router_config_change() {
        use alloy::{
            node_bindings::Anvil, providers::ext::AnvilApi, rpc::types::TransactionRequest, sol,
            sol_types::SolCall,
        };

        sol! {
            function govSetConfig(address base, address quote, address oracle);
        }

        let block = 25644480;
        // The EulerRouter of a mainnet EVault, its governor at `block`, and a pair it prices
        // through a generic (non-Pyth) adapter.
        let router = address!("0xC900F9077D4DfB89B68d49fCA60206F90C707f9E");
        let governor = address!("0x9453ee262d7C95955e690AE7aBBD82a08B135685");
        let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let usd = address!("0x0000000000000000000000000000000000000348");
        // An existing PythOracle adapter (the one used in `identify_pyth_oracle`).
        let pyth_adapter = address!("0x922D0c82d70c3c9F8928742E8f75004DEa228FE6");

        let mainnet_rpc = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");
        let network = Anvil::new()
            .fork(mainnet_rpc.clone())
            .fork_block_number(block)
            .try_spawn()
            .unwrap();
        let provider = ProviderBuilder::new()
            .connect_http(network.endpoint_url())
            .erased();

        crate::test_utils::ensure_contracts_on_fork(
            &provider,
            &mainnet_rpc.parse().unwrap(),
            &[MAINNET_ORACLE_LENS],
        )
        .await
        .unwrap();

        let oracles = OraclesCache::new(MAINNET_ORACLE_LENS, None);
        let id = OracleIdentifier {
            base_asset: usdc,
            quote_asset: usd,
            adapter: router,
        };

        // First resolve, this is what gets cached.
        let before = oracles.fetch_type(&provider, id.clone()).await.unwrap();
        assert!(before.pyth_ids().is_empty(), "expected a non-Pyth oracle before the change");

        // The governor moves the pair to a Pyth adapter.
        provider.anvil_impersonate_account(governor).await.unwrap();
        provider
            .anvil_set_balance(governor, U256::from(10).pow(U256::from(18)))
            .await
            .unwrap();
        let receipt = provider
            .send_transaction(
                TransactionRequest::default().from(governor).to(router).input(
                    govSetConfigCall {
                        base: usdc,
                        quote: usd,
                        oracle: pyth_adapter,
                    }
                    .abi_encode()
                    .into(),
                ),
            )
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
        assert!(receipt.status(), "govSetConfig reverted");

        oracles.refresh_all_types(&provider).await;

        // The cached type now reflects the Pyth adapter, so its feed gets updated before use.
        let after = oracles.fetch_type(&provider, id).await.unwrap();
        assert!(
            !after.pyth_ids().is_empty(),
            "expected the refreshed oracle type to be Pyth, got {after:?}"
        );
    }

    #[tokio::test]
    async fn fetch_price_from_pyth_oracle() {
        let oracle = OracleIdentifier {
            base_asset: address!("0x96F6eF951840721AdBF46Ac996b59E0235CB985C"),
            quote_asset: address!("0x0000000000000000000000000000000000000348"),
            adapter: address!("0xfe3ED784f0244B24Df186e576313d682f6Ee9865"),
        };

        let provider = ProviderBuilder::new()
            .connect_http(MAINNET_RPC_ENDPOINT.parse().unwrap())
            .erased();

        let pyth = PythConfig {
            address: MAINNET_PYTH,
            endpoint: DEFAULT_PYTH_ENDPOINT.to_string(),
            // Hermes rejects unauthenticated requests.
            api_key: std::env::var("PYTH_API_KEY").ok(),
        };

        let oracles = OraclesCache::new(MAINNET_ORACLE_LENS, Some(pyth));
        oracles
            .fetch_latest_price(&provider, oracle.clone())
            .await
            .unwrap();
    }

    use crate::oracles::ORACLE_PRICING_UNIT;
    use alloy::primitives::U256;

    fn unit() -> U256 {
        U256::from(ORACLE_PRICING_UNIT)
    }

    fn random_id() -> OracleIdentifier {
        OracleIdentifier {
            base_asset: Address::random(),
            quote_asset: Address::random(),
            adapter: Address::random(),
        }
    }

    #[test]
    fn get_quote_scales_by_price() {
        let cache = OraclesCache::new(Address::ZERO, None);
        let id = random_id();

        // Price of 2 (in 1e18 fixed point) means base is worth 2x quote.
        cache.insert_price_for_test(id.clone(), unit() * U256::from(2));

        // 5 * 2 = 10.
        assert_eq!(cache.get_quote(&id, U256::from(5)).unwrap(), U256::from(10));
    }

    #[test]
    fn get_quote_with_unit_price_is_identity() {
        let cache = OraclesCache::new(Address::ZERO, None);
        let id = random_id();
        cache.insert_price_for_test(id.clone(), unit());

        assert_eq!(
            cache.get_quote(&id, U256::from(12345)).unwrap(),
            U256::from(12345)
        );
    }

    #[test]
    fn get_quote_rounds_up() {
        let cache = OraclesCache::new(Address::ZERO, None);
        let id = random_id();

        // A price of 1 wei with amount 1 gives 1 / 1e18, which is a tiny non-zero
        // fraction. div_ceil must round this up to 1 rather than truncating to 0.
        cache.insert_price_for_test(id.clone(), U256::from(1));

        assert_eq!(cache.get_quote(&id, U256::from(1)).unwrap(), U256::from(1));
    }

    #[test]
    fn get_quote_of_zero_amount_is_zero() {
        let cache = OraclesCache::new(Address::ZERO, None);
        let id = random_id();
        cache.insert_price_for_test(id.clone(), unit() * U256::from(7));

        assert_eq!(cache.get_quote(&id, U256::ZERO).unwrap(), U256::ZERO);
    }

    #[test]
    fn get_quote_missing_price_errors_and_tracks_oracle() {
        let cache = OraclesCache::new(Address::ZERO, None);
        let id = random_id();

        // No price seeded: the quote must fail...
        assert!(cache.get_quote(&id, U256::from(1)).is_err());

        // ...and the oracle should now be tracked so a price gets fetched later.
        assert!(cache.active_oracles().contains(&id));
    }
}
