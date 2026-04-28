use std::str::FromStr;

use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::DynProvider,
    sol,
};
use anyhow::{Result, anyhow};
use itertools::Itertools;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct PythResponse {
    pub binary: BinaryData,
    // pub parsed: Vec<ParsedPriceFeed>,
}

#[derive(Debug, Deserialize)]
struct BinaryData {
    // pub encoding: String,
    pub data: Vec<String>,
}

// #[derive(Debug, Deserialize)]
// struct ParsedPriceFeed {
//     pub id: String,
//     pub price: PriceInfo,
//     pub ema_price: PriceInfo,
//     // pub metadata: FeedMetadata,
// }
//
// #[derive(Debug, Deserialize)]
// pub struct PriceInfo {
//     pub price: U256,
//     pub conf: U256,
//     pub expo: i32,
//     pub publish_time: i64,
// }

// #[derive(Debug, Deserialize)]
// pub struct FeedMetadata {
//     pub slot: u64,
//     pub proof_available_time: i64,
//     pub prev_publish_time: i64,
// }
#[derive(Debug, Clone)]
pub struct PythFeedInput {
    pub data: Vec<Bytes>,
    pub cost: U256,
}

sol! {
    #[sol(rpc)]
    contract Pyth {
        /// @notice Update price feeds with given update messages.
        /// This method requires the caller to pay a fee in wei; the required fee can be computed by calling
        /// `getUpdateFee` with the length of the `updateData` array.
        /// Prices will be updated if they are more recent than the current stored prices.
        /// The call will succeed even if the update is not the most recent.
        /// @dev Reverts if the transferred fee is not sufficient or the updateData is invalid.
        /// @param updateData Array of price update data.
        function updatePriceFeeds(bytes[] calldata updateData) external payable;

        /// @notice Returns the required fee to update an array of price updates.
        /// @param updateData Array of price update data.
        /// @return feeAmount The required fee in Wei.
        function getUpdateFee(
            bytes[] calldata updateData
        ) external view returns (uint feeAmount);
    }
}

async fn fetch_pyth(ids: Vec<FixedBytes<32>>) -> Result<PythResponse> {
    let request_url = format!(
        "https://hermes.pyth.network/v2/updates/price/latest?ids[]={}",
        ids.iter().format("&ids[]=")
    );
    Ok(reqwest::get(request_url).await?.json().await?)
}

pub async fn fetch_pyth_data(
    provider: &DynProvider,
    pyth: Address,
    ids: Vec<FixedBytes<32>>,
) -> Result<PythFeedInput> {
    let data: Vec<Bytes> = fetch_pyth(ids)
        .await?
        .binary
        .data
        .iter()
        .map(|d| {
            Bytes::from_str(d)
                .map_err(|_| anyhow!("Could not encode Pyth response to bytes calldata"))
        })
        .collect::<Result<Vec<Bytes>>>()?;

    // Fetch the update cost.
    let pyth = Pyth::new(pyth, provider);
    let cost = pyth.getUpdateFee(data.clone()).call().await?;

    Ok(PythFeedInput { data, cost })
}
