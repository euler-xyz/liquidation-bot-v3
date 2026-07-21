use alloy::primitives::{Address, U256};
use anyhow::anyhow;
use http_cache_reqwest::{CACacheManager, Cache, CacheMode, HttpCache, HttpCacheOptions};
use reqwest::{Client, Url};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

/// Errors that can occur while quoting through the pricing API.
///
/// The main reason this is a dedicated type (rather than `anyhow::Error`) is so
/// callers can distinguish "this chain is not supported by the pricing API at
/// all" from transient failures, and decide to proceed without a profit figure.
#[derive(Debug)]
pub enum PricingError {
    /// The pricing API does not support the chain we are running on.
    ChainNotSupported { chain_id: u64, message: String },
    /// Any other structured error returned by the pricing API.
    Api { code: String, message: String },
    /// Transport, decoding or any other unexpected failure.
    Other(anyhow::Error),
}

impl std::fmt::Display for PricingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PricingError::ChainNotSupported { chain_id, message } => {
                write!(
                    f,
                    "chain {chain_id} is not supported by the pricing API: {message}"
                )
            }
            PricingError::Api { code, message } => {
                write!(f, "pricing API returned an error ({code}): {message}")
            }
            PricingError::Other(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PricingError {}

pub trait PriceAsset {
    async fn quote(
        &self,
        input_asset: Address,
        input_amount: U256,
        output_asset: Address,
    ) -> Result<U256, PricingError>;
}

pub struct EulerPricingApi {
    base_url: Url,
    chain_id: u64,
    client: ClientWithMiddleware,
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
    pub confidence: Option<f64>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    pub timestamp: String,
}

/// The error shape the pricing API returns, e.g.
/// `{"error":{"code":"CHAIN_NOT_SUPPORTED","message":"Chain 146 is not supported","requestId":"req_..."}}`
#[derive(Debug, Clone, Deserialize)]
struct ApiErrorResponse {
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiErrorBody {
    pub code: String,
    pub message: String,
}

async fn get_euler_price(
    client: &ClientWithMiddleware,
    base_url: &Url,
    chain_id: u64,
    asset: Address,
) -> Result<PriceData, PricingError> {
    let url = reqwest::Url::parse(
        format!("{}v3/tokens/{}/{}/price", base_url, chain_id, asset).as_str(),
    )
    .map_err(|err| PricingError::Other(err.into()))?;

    let start = Instant::now();
    let response_body = client
        .get(url)
        .send()
        .await
        .map_err(|err| PricingError::Other(err.into()))?
        .text()
        .await
        .map_err(|err| PricingError::Other(err.into()))?;

    // The API either returns the price data or a structured error object. Try to decode the
    // success shape first, then fall back to the error shape.
    let decode_err = match serde_json::from_str::<PriceResponse>(&response_body) {
        Ok(response) => {
            tracing::debug!("Euler price request took {:?}", start.elapsed());
            return Ok(response.data);
        }
        Err(err) => err,
    };

    if let Ok(response) = serde_json::from_str::<ApiErrorResponse>(&response_body) {
        let ApiErrorBody { code, message } = response.error;

        return Err(if code == "CHAIN_NOT_SUPPORTED" {
            PricingError::ChainNotSupported { chain_id, message }
        } else {
            PricingError::Api { code, message }
        });
    }

    Err(PricingError::Other(anyhow!(
        "Issue while decoding response from price quote api, err: {:?}, response_body: {}",
        decode_err,
        response_body
    )))
}

impl EulerPricingApi {
    pub fn new(base_url: Url, chain_id: u64) -> Self {
        // Attempt to create a tempdir so we can use it for caching requests.
        let client = match tempfile::tempdir() {
            Ok(cache_dir) => {
                let cache_manager = CACacheManager::new(cache_dir.path().to_path_buf(), true);
                ClientBuilder::new(Client::new())
                    .with(Cache(HttpCache {
                        mode: CacheMode::Default,
                        manager: cache_manager,
                        options: HttpCacheOptions::default(),
                    }))
                    .build()
            }
            Err(e) => {
                tracing::warn!(
                    "Could not configure caching layer for pricing api due to: {}",
                    e
                );
                // Still continue, but without the caching layer.
                ClientBuilder::new(Client::new()).build()
            }
        };

        // Build the reqwest client with a caching layer to reduce duplicate requests.
        EulerPricingApi {
            base_url,
            chain_id,
            client,
        }
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
    ) -> Result<U256, PricingError> {
        // No need to do a conversion if both assets are the same.
        // NOTE: This short-circuit never hits the API. So even on chains the pricing API does
        // not support, quotes where input == output (e.g. the profit is already denominated in
        // the native asset) still return a real amount and go through the normal profit gate,
        // instead of the `ChainNotSupported` handling.
        if input_asset == output_asset {
            return Ok(input_amount);
        }

        if input_amount.is_zero() {
            return Ok(U256::ZERO);
        }

        let input_usd =
            get_euler_price(&self.client, &self.base_url, self.chain_id, input_asset).await?;
        let output_usd =
            get_euler_price(&self.client, &self.base_url, self.chain_id, output_asset).await?;

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
    use crate::prices::{EulerPricingApi, PriceAsset};
    use alloy::primitives::{U256, address};

    #[tokio::test]
    async fn price_usdc_usdt() {
        let pricing = EulerPricingApi::new("https://v3.euler.finance/".parse().unwrap(), 1);

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
        let pricing = EulerPricingApi::new("https://v3.euler.finance/".parse().unwrap(), 1);
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
