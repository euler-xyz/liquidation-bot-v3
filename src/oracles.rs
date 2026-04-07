use std::{collections::HashMap, default, sync::Arc};

use alloy::{
    dyn_abi::SolType,
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::DynProvider,
    sol,
};
use anyhow::{Result, anyhow};
use dashmap::DashMap;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info};

use crate::types::OracleIdentifier;

#[derive(Debug, Clone)]
pub struct OraclesCache {
    lens: Address,
    oracles: Arc<DashMap<OracleIdentifier, Oracle>>,
}

impl OraclesCache {
    pub fn new(oracle_lens: Address) -> Self {
        OraclesCache {
            lens: oracle_lens,
            oracles: Arc::new(DashMap::new()),
        }
    }

    pub async fn fetch(&self, provider: &DynProvider, id: OracleIdentifier) -> Result<Oracle> {
        // Check if we have this id cached.
        match self.oracles.get(&id) {
            Some(oracle) => Ok(oracle.clone()),
            None => {
                // Resolve the identifier.
                let oracle = id.resolve(provider, self.lens).await?;
                // Store the result.
                self.oracles.insert(id, oracle.clone());
                Ok(oracle)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct OracleChange {
    pub oracle: OracleIdentifier,
    pub price: U256,
}

struct OracleOutput {
    price: U256,
    last_polled_at: (),
    last_changed_at: (),
}

pub async fn poll_oracles(
    provider: DynProvider,
    cache: OraclesCache,
    initial_oracles: Vec<OracleIdentifier>,
    event_channel: Sender<Vec<OracleChange>>,
) -> Result<()> {
    let mut oracles: HashMap<OracleIdentifier, OracleOutput> = HashMap::new();

    // On start up we make sure to fetch all prices, this way there is never a situation in which we
    // do not already have a price.
    for oracle in initial_oracles.into_iter() {
        let price = match oracle.fetch_price(&provider).await {
            Ok(price) => price,
            Err(e) => {
                error!(
                    "Error while fetching price for oracle {}: {} -> {}: {e}",
                    oracle.adapter, oracle.base_asset, oracle.quote_asset
                );

                // NOTICE: For now we insert a fake price, otherwise if we do not do this then this
                // oracle will not be tracked.
                // TODO: Add a way to show a price is not availabe for an oracle.
                U256::ZERO
            }
        };

        oracles.insert(
            oracle.clone(),
            OracleOutput {
                price,
                last_polled_at: (),
                last_changed_at: (),
            },
        );
    }

    // Send the entire set as a price change.
    event_channel
        .send(
            oracles
                .iter()
                .map(|(k, p)| OracleChange {
                    oracle: k.clone(),
                    price: p.price,
                })
                .collect(),
        )
        .await?;

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
        info!("Checking {} oracles for price changes", oracles.len());

        let mut changes = Vec::new();
        for (oracle, prev) in oracles.iter_mut() {
            // Poll the oracle.
            let new_price = match oracle.fetch_price(&provider).await {
                Ok(price) => price,
                Err(e) => {
                    error!(
                        "Error while fetching price for oracle {}: {} -> {}: {e}",
                        oracle.adapter, oracle.base_asset, oracle.quote_asset
                    );
                    continue;
                }
            };

            // Check if the price has changed.
            if prev.price != new_price {
                debug!(
                    old_price =? prev.price,
                    new_price =? new_price,
                    "Oracle {}: {} -> {} its price has updated",
                    oracle.adapter,
                    oracle.base_asset,
                    oracle.quote_asset
                );

                // Update out store.
                prev.price = new_price;

                // Track this as having changed.
                changes.push(OracleChange {
                    oracle: oracle.clone(),
                    price: new_price,
                });
            }
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
    pub async fn fetch_price(&self, provider: &DynProvider) -> Result<U256> {
        let oracle = IPriceOracle::new(self.adapter, provider);

        let result = oracle
            .getQuote(U256::from(100_000), self.base_asset, self.quote_asset)
            .call()
            .await?;

        Ok(result)
    }

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
        dbg!(&self);
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

#[derive(Clone, Debug)]
pub struct Oracle {
    name: String,
    address: Address,
    oracle_type: OracleType,
}

#[derive(Clone, Debug, Default)]
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

    use crate::{oracles::OracleType, types::OracleIdentifier};

    const MAINNET_RPC_ENDPOINT: &str = "https://eth.rpc.blxrbdn.com";
    const MAINNET_ORACLE_LENS: Address = address!("0x30E6dFB84782A31d561536f64F47231451F7b48A");

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
}
