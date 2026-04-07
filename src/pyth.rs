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
    pub parsed: Vec<ParsedPriceFeed>,
}

#[derive(Debug, Deserialize)]
struct BinaryData {
    pub encoding: String,
    pub data: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ParsedPriceFeed {
    pub id: String,
    pub price: PriceInfo,
    pub ema_price: PriceInfo,
    pub metadata: FeedMetadata,
}

#[derive(Debug, Deserialize)]
pub struct PriceInfo {
    pub price: U256,
    pub conf: U256,
    pub expo: i32,
    pub publish_time: i64,
}

#[derive(Debug, Deserialize)]
pub struct FeedMetadata {
    pub slot: u64,
    pub proof_available_time: i64,
    pub prev_publish_time: i64,
}
#[derive(Debug, Clone)]
pub struct PythFeedInput {
    pub data: Vec<Bytes>,
    pub cost: U256,
}

sol! {
    #[sol(rpc)]
    contract Pyth {
        function getUpdateFee(
            bytes[] calldata updateData
        ) public view returns (uint256 feeAmount);
    }
}

pub async fn fetch_pyth(ids: Vec<FixedBytes<32>>) -> Result<PythResponse> {
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
