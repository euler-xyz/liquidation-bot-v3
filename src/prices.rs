use alloy::primitives::{Address, U256};
use anyhow::Result;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};

pub trait PriceAsset {
    async fn quote(
        &self,
        input_asset: Address,
        input_amount: U256,
        output_asset: Address,
    ) -> Result<U256>;
}

pub struct EulerPricingApi {
    base_url: Url,
    chain_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PriceResponse {
    pub data: PriceData,
    pub meta: Meta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PriceData {
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    pub address: Address,
    #[serde(rename = "priceUsd")]
    pub price_usd: f64,
    pub decimals: u32,
    pub source: String,
    pub confidence: f64,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    pub timestamp: String,
}

async fn get_euler_price(base_url: &Url, chain_id: u64, asset: Address) -> Result<PriceData> {
    let url = reqwest::Url::parse(
        format!("{}v3/tokens/{}/{}/price", base_url, chain_id, asset).as_str(),
    )?;

    dbg!(&url);

    let client = Client::builder().build()?;
    let response: PriceResponse = client.get(url).send().await?.json().await?;

    Ok(response.data)
}

impl EulerPricingApi {
    pub fn new(base_url: Url, chain_id: u64) -> Self {
        EulerPricingApi { base_url, chain_id }
    }
}

impl PriceAsset for EulerPricingApi {
    /// Euler pricing api uses USD as its unit of accounting, so we will need to get two prices to
    /// go from the input into the output.
    async fn quote(
        &self,
        input_asset: Address,
        input_amount: U256,
        output_asset: Address,
    ) -> Result<U256> {
        let input_usd = get_euler_price(&self.base_url, self.chain_id, input_asset).await?;
        let output_usd = get_euler_price(&self.base_url, self.chain_id, output_asset).await?;

        let input_price = U256::from((input_usd.price_usd * 1e18) as u128);
        let output_price = U256::from((output_usd.price_usd * 1e18) as u128);

        let num =
            input_amount * input_price * U256::from(10u128).pow(U256::from(output_usd.decimals));
        let den = output_price * U256::from(10u128).pow(U256::from(input_usd.decimals));
        let output_amount = num / den;

        Ok(output_amount)
    }
}
