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
use serde::{Deserialize, Serialize};
use serde_with::TimestampSeconds;
use serde_with::serde_as;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info, warn};

use crate::{
    pyth::{
        Pyth::{self},
        fetch_pyth_data,
    },
    types::OracleIdentifier,
};

#[derive(Debug, Clone)]
pub struct OraclesCache {
    lens: Address,
    pyth: Address,

    // The oracles that should be actively tracked.
    active_oracles: Arc<DashSet<OracleIdentifier>>,

    // The resolved oracle types.
    oracles: Arc<DashMap<OracleIdentifier, Oracle>>,

    // The oracle outputs.
    prices: Arc<DashMap<OracleIdentifier, OracleOutput>>,
}

impl OraclesCache {
    pub fn new(oracle_lens: Address, pyth: Address) -> Self {
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
            let _ = self.fetch_latest_price(provider, id.clone()).await;
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

        Ok((amount * price).div_ceil(U256::from(100_000)))
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
            Some(prev) if prev.price == price => OracleOutput {
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
        let adapter_call = adapter.getQuote(U256::from(100_000), id.base_asset, id.quote_asset);

        // Fetch the oracle either from the chain or from the cache, we need this to determine how
        // to fetch the price.
        let oracle = self.fetch_type(provider, id.clone()).await?;

        // Check to see if this oracle uses pyth.
        let pyth_ids = oracle.pyth_ids();

        // If it has no pyth dependencies then we can fetch the price from the chain.
        if pyth_ids.is_empty() {
            return Ok(adapter_call.call().await?);
        }

        let pyth_call = match fetch_pyth_data(provider, self.pyth, pyth_ids).await {
            Ok(data) => {
                CallItemBuilder::new(Pyth::new(self.pyth, provider).updatePriceFeeds(data.data))
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
                    error!(
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

        // Notify the main thread of the price changes, if any.
        if !changes.is_empty() {
            event_channel.send(changes).await?;
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
        oracles::{OracleType, OraclesCache},
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
    async fn fetch_price_from_pyth_oracle() {
        let oracle = OracleIdentifier {
            base_asset: address!("0x96F6eF951840721AdBF46Ac996b59E0235CB985C"),
            quote_asset: address!("0x0000000000000000000000000000000000000348"),
            adapter: address!("0xfe3ED784f0244B24Df186e576313d682f6Ee9865"),
        };

        let provider = ProviderBuilder::new()
            .connect_http(MAINNET_RPC_ENDPOINT.parse().unwrap())
            .erased();

        let oracles = OraclesCache::new(MAINNET_ORACLE_LENS, MAINNET_PYTH);
        oracles
            .fetch_latest_price(&provider, oracle.clone())
            .await
            .unwrap();
    }
}
