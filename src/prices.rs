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
        // No need to do a conversion if both assets are the same.
        if input_asset == output_asset {
            return Ok(input_amount);
        }

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

#[cfg(test)]
mod test {
    use alloy::primitives::{U256, address};

    use crate::prices::{EulerPricingApi, PriceAsset};

    #[tokio::test]
    async fn price_usdc_usdt() {
        let pricing = EulerPricingApi::new("https://v3.eul.dev/".parse().unwrap(), 1);

        // Convert 1 USDT into 1 USDC, this test optimistically assumes there is no depeg for either
        // of these assets.
        let price = pricing
            .quote(
                address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
                U256::from(1_000_000),
                address!("0xdAC17F958D2ee523a2206206994597C13D831ec7"),
            )
            .await
            .unwrap();

        assert!(price > U256::from(950_000));
        assert!(price < U256::from(1_050_000));
    }

    #[tokio::test]
    async fn price_usdc_eth() {
        let pricing = EulerPricingApi::new("https://v3.eul.dev".parse().unwrap(), 1);
        let wrapped_native_asset = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

        let in_amount = U256::from(1_000_000);

        // Price 1 USDC into ETH
        let quote = pricing
            .quote(
                address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
                in_amount,
                wrapped_native_asset,
            )
            .await
            .unwrap();

        // Now we reverse the quote to see if we get back to the original (rougly).
        let quote = pricing
            .quote(
                wrapped_native_asset,
                quote,
                address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            )
            .await
            .unwrap();

        assert!(quote - U256::from(100) < in_amount);
        assert!(quote + U256::from(100) > in_amount);
    }
}
