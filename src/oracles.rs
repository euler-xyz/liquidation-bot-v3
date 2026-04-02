use std::collections::HashMap;

use alloy::{primitives::U256, providers::DynProvider, sol};
use anyhow::Result;
use tokio::sync::mpsc::Sender;

use crate::{oracles, types::OracleIdentifier};

#[derive(Debug)]
pub enum OracleEvent {
    UpdatedPrices(Vec<OracleChange>),
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
    initial_oracles: Vec<OracleIdentifier>,
    event_channel: Sender<Vec<OracleChange>>,
) -> Result<()> {
    let mut oracles: HashMap<OracleIdentifier, OracleOutput> = HashMap::new();

    // On start up we make sure to fetch all prices, this way there is never a situation in which we
    // do not already have a price.
    for oracle in initial_oracles.into_iter() {
        oracles.insert(
            oracle.clone(),
            OracleOutput {
                price: oracle.fetch_price(&provider).await?,
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

        let mut changes = Vec::new();
        for (oracle, prev) in oracles.iter_mut() {
            // Poll the oracle.
            let new_price = oracle.fetch_price(&provider).await?;

            // Check if the price has changed.
            if prev.price != new_price {
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
}
