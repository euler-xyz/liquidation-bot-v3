use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy::{
    primitives::{Address, Bytes, U256},
    providers::{DynProvider, Provider},
    serde::quantity::hashmap,
};
use anyhow::{Context, Result, anyhow};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::trace;

use crate::{
    liquidation::PreparedLiquidation, prices::PriceAsset, types::LiquidationReasoningError,
};

// TODO: Once we know what fields we need (and are nice to have for debugging) we should clean
// these struct and remove unused fields.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapParams {
    pub chain_id: String,
    pub token_in: Address,
    pub token_out: Address,
    pub receiver: Address,
    pub vault_in: Address,
    pub origin: Address,
    pub account_in: Address,
    pub account_out: Address,
    pub amount: U256,
    pub target_debt: U256,
    pub current_debt: U256,
    /// 0 = exact input, 1 = exact output, 2 = target debt
    pub swapper_mode: String,
    /// Maximum slippage in percent (e.g. "0.1" for 0.1%)
    pub slippage: String,
    /// Quote expiry unix timestamp in seconds
    pub deadline: String,
    /// Use bought tokens to repay debt instead of depositing
    pub is_repay: String,

    // ── optional fields ──
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dust_account: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unused_input_receiver: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer_output_to_receiver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_sweep_deposit_out: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_override: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapApiResponse {
    pub success: bool,
    pub message: Option<String>,
    pub data: Option<Value>,
    // pub status_code: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapQuote {
    // pub amount_in: U256,
    // pub amount_in_max: U256,
    pub amount_out: U256,
    // pub amount_out_min: U256,
    // pub account_in: Address,
    // pub account_out: Address,
    // pub vault_in: Address,
    // pub receiver: Address,
    // pub token_in: Token,
    // pub token_out: Token,
    // pub slippage: f64,
    // pub estimated_gas: Option<U256>,
    pub swap: SwapPayload,
    // pub verify: VerifyPayload,
    // pub route: Vec<RouteStep>,
    // pub unused_input_receiver: Option<Address>,
    // pub transfer_output_to_receiver: Option<bool>,
}

// #[derive(Debug, Clone, Deserialize)]
// #[serde(rename_all = "camelCase")]
// pub struct Token {
//     chain_id: U256,
//     address: Address,
//     name: String,
//     decimals: u16,
// }

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapPayload {
    // pub swapper_address: Address,
    // pub swapper_data: primitives::Bytes,
    pub multicall_items: Vec<MulticallItem>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MulticallItem {
    // pub function_name: String,
    // pub args: Option<serde_json::Value>,
    pub data: Bytes,
}

// #[derive(Debug, Clone, Deserialize)]
// #[serde(rename_all = "camelCase")]
// pub struct VerifyPayload {
//     pub verifier_address: Address,
//     pub verifier_data: Bytes,
//     #[serde(rename = "type")]
//     pub verify_type: VerifyType,
//     pub vault: Address,
//     pub account: Address,
//     pub amount: U256,
//     pub deadline: u64,
// }
//
// #[derive(Debug, Clone, Deserialize)]
// #[serde(rename_all = "camelCase")]
// pub enum VerifyType {
//     SkimMin,
//     DebtMax,
//     TransferMin,
// }

// #[derive(Debug, Clone, Deserialize)]
// #[serde(rename_all = "camelCase")]
// pub struct RouteStep {
//     pub provider_name: String,
// }

pub trait SwapQuoteProvider {
    #![allow(async_fn_in_trait)]
    async fn find_swap(
        &self,
        liq: PreparedLiquidation,
    ) -> Result<Option<PreparedLiquidation>, LiquidationReasoningError>;
}

pub struct EulerSwapApi<T: PriceAsset> {
    base_url: Url,

    // Provider used for simulating
    provider: DynProvider,
    chain_id: u64,
    liquidator_eoa: Address,
    profit_receiver: Address,
    swapper_address: Address,
    wrapped_native_asset: Address,

    max_slippage: String,

    // This is used to perform pricing conversions.
    pricing: T,
}

impl<T: PriceAsset> EulerSwapApi<T> {
    pub fn new(
        base_url: Url,
        provider: DynProvider,
        chain_id: u64,
        profit_receiver: Address,
        liquidator_eoa: Address,
        swapper_address: Address,
        wrapped_native_asset: Address,
        max_slippage: &str,
        pricing: T,
    ) -> Self {
        EulerSwapApi {
            base_url,
            provider,
            chain_id,
            profit_receiver,
            liquidator_eoa,
            swapper_address,
            wrapped_native_asset,
            max_slippage: max_slippage.to_string(),
            pricing,
        }
    }

    pub async fn get_swap_quotes(&self, params: &SwapParams) -> Result<Vec<SwapQuote>> {
        let mut query: Vec<(&str, String)> = vec![
            ("chainId", params.chain_id.clone()),
            ("tokenIn", params.token_in.to_string()),
            ("tokenOut", params.token_out.to_string()),
            ("receiver", params.receiver.to_string()),
            ("vaultIn", params.vault_in.to_string()),
            ("origin", params.origin.to_string()),
            ("accountIn", params.account_in.to_string()),
            ("accountOut", params.account_out.to_string()),
            ("amount", params.amount.to_string()),
            ("targetDebt", params.target_debt.to_string()),
            ("currentDebt", params.current_debt.to_string()),
            ("swapperMode", params.swapper_mode.clone()),
            ("slippage", params.slippage.clone()),
            ("deadline", params.deadline.clone()),
            ("isRepay", params.is_repay.clone()),
        ];

        if let Some(ref v) = params.dust_account {
            query.push(("dustAccount", v.to_string()));
        }
        if let Some(ref v) = params.unused_input_receiver {
            query.push(("unusedInputReceiver", v.to_string()));
        }
        if let Some(ref v) = params.transfer_output_to_receiver {
            query.push(("transferOutputToReceiver", v.clone()));
        }
        if let Some(ref v) = params.skip_sweep_deposit_out {
            query.push(("skipSweepDepositOut", v.clone()));
        }
        if let Some(ref v) = params.routing_override {
            query.push(("routingOverride", v.clone()));
        }
        if let Some(ref v) = params.provider {
            query.push(("provider", v.clone()));
        }

        // NOTICE: temp work around as without a regular user agent our requests get blocked by
        // cloudflare.
        let client = Client::builder()
        .user_agent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0",
        )
        .build()?;

        let url =
            reqwest::Url::parse_with_params(format!("{}swaps", self.base_url).as_str(), &query)?;

        tracing::debug!("making Swap API request to {}", url.clone());

        // If set we get the api key that removed the request limit, otherwise we may get limited by
        // cloudflare.
        let api_key = std::env::var("SWAP_API_HEADER_SECRET").unwrap_or_default();

        let response_body = client
            .get(url.clone())
            .header("x-api-key", api_key)
            .send()
            .await?
            .text()
            .await?;

        let response_body: SwapApiResponse = match serde_json::from_str(&response_body) {
            Ok(resp) => resp,
            Err(err) => {
                return Err(anyhow!(
                    "Could not decode response from swap API, body: {}, err: {}, url: {}",
                    response_body,
                    err,
                    url
                ));
            }
        };

        // Make sure that the response was a success before attempting to deserialize the swapquote.
        if !response_body.success {
            let message = response_body.message.unwrap_or_default();

            // The swap api reports no quotes as an error, but for us that should not be an error.
            // So we instead just report back as not having found any quotes.
            if message == "Swap quote not found" {
                return Ok(vec![]);
            }

            return Err(anyhow!("Swap API responded with: {}", message));
        }

        match response_body.data {
            Some(data) => Ok(serde_json::from_value(data)?),
            None => Ok(vec![]),
        }
    }
}

impl<T: PriceAsset> SwapQuoteProvider for EulerSwapApi<T> {
    async fn find_swap(
        &self,
        liq: PreparedLiquidation,
    ) -> Result<Option<PreparedLiquidation>, LiquidationReasoningError> {
        // In this case a swap isn't actually needed. We only need to calculate what the profit of
        // executing the liquidation would be.
        // NOTE: Unsure if this functionality should live in this provider, as its not actually
        // providing a swap quote, but its only here since this also has the PriceAsset trait.
        // We might want to move this somewhere else.
        if liq.borrow().vault.asset == liq.collateral().vault.asset {
            // The amount we need to repay is more than the amount of assets.
            if liq.seized_collateral_amount() < liq.repay_amount() {
                return Ok(None);
            }

            // Calculate the profitis as well as in the native asset.
            let profit = liq.seized_collateral_amount() - liq.repay_amount();
            let profit_in_native = self
                .pricing
                .quote(
                    liq.collateral().vault.asset,
                    profit,
                    self.wrapped_native_asset,
                )
                .await
                .map_err(|e| {
                    tracing::error!("Could not fetch quote, err: {:?}", e);
                    LiquidationReasoningError::Other {
                        message: "Could not calculate profit through the pricing API".to_string(),
                    }
                })?;

            return Ok(Some(liq.with_profit(profit_in_native, profit)));
        }

        let start = SystemTime::now();
        let since_the_epoch = match start.duration_since(UNIX_EPOCH) {
            Ok(since) => since,
            Err(err) => {
                tracing::error!(
                    "CRITICAL ERROR! Time has moved backwards somehow. This should never happen, err: {:?}",
                    err
                );

                return Err(LiquidationReasoningError::Other {
                    message: "Critical error!".to_string(),
                });
            }
        };

        // Build the params to call the swap api with for this liquidation.
        let params = &SwapParams {
            chain_id: self.chain_id.to_string(),
            token_in: liq.collateral().vault.asset,
            token_out: liq.borrow().vault.asset,
            receiver: self.swapper_address,
            vault_in: Address::ZERO,
            origin: self.liquidator_eoa,
            account_in: Address::ZERO,
            account_out: self.swapper_address,
            amount: liq.seized_collateral_amount(),
            target_debt: U256::ZERO,
            current_debt: liq.repay_amount(),
            swapper_mode: "0".to_string(),
            slippage: self.max_slippage.clone(),
            // Deadline of 5 minutes into the future.
            deadline: since_the_epoch
                .saturating_add(Duration::from_mins(5))
                .as_secs()
                .to_string(),
            is_repay: "false".to_string(),
            dust_account: None,
            unused_input_receiver: Some(self.liquidator_eoa),
            transfer_output_to_receiver: None,
            skip_sweep_deposit_out: Some("true".to_string()),
            routing_override: None,
            provider: None,
        };

        // Call the API to get the possible routes.
        let mut quotes: Vec<SwapQuote> = self
            .get_swap_quotes(params)
            .await
            .context("When fetching swap quotes")
            .map_err(|e| {
                tracing::error!("Issue while fetching swap quotes, err: {:?}", e);
                LiquidationReasoningError::Other {
                    message: "Issue fetching swap quotes".to_string(),
                }
            })?;

        // Sort it by most profitable to least profitable.
        quotes.sort_by_key(|q| std::cmp::Reverse(q.amount_out));

        // We track failed attempts and count how often a specific error occured.
        let mut attempts: HashMap<LiquidationReasoningError, usize> = HashMap::new();

        for quote in quotes {
            // Since we are sorted by profibility if this is not suffecient none of the others will
            // be either.
            if liq.repay_amount() > quote.amount_out {
                break;
            }

            // Build the liquidation.
            let liquidation = liq.clone().with_swap_data(Some(quote.swap));

            // Simulate executing it.
            match self
                .provider
                .call(liquidation.clone().into_transaction(self.profit_receiver))
                .await
            {
                Ok(_) => {
                    // This is valid swap data, since we ordered by profitability we can just return
                    // as this will be the most profitable that we will run into.
                    let profit = quote.amount_out - liq.repay_amount();
                    let profit_in_native = self
                        .pricing
                        .quote(
                            liq.collateral().vault.asset,
                            profit,
                            self.wrapped_native_asset,
                        )
                        .await
                        .map_err(|e| {
                            tracing::error!("Could not fetch quote, err: {:?}", e);
                            LiquidationReasoningError::Other {
                                message: "Could not calculate profit through the pricing API"
                                    .to_string(),
                            }
                        })?;

                    // NOTE: We could already determine here if this swap is profitable, as we just
                    // did a simulation so we could check gas usage, and we have the potential
                    // profit. But it might be cleaner to bubble it up anyway and handle it at the
                    // execution stage.
                    return Ok(Some(liquidation.with_profit(profit_in_native, profit)));
                }
                Err(err) => {
                    tracing::debug!(
                        "Error while simulating quote execution and liquidation, err: {:?}",
                        err
                    );

                    let attempt_error = match err {
                        alloy::transports::RpcError::ErrorResp(error_payload) => {
                            // Extract the revert data from the request.
                            LiquidationReasoningError::LiquidationRevert {
                                data: error_payload.as_revert_data().unwrap_or_default(),
                            }
                        }
                        _ => LiquidationReasoningError::Other {
                            message: "RPC Error".to_string(),
                        },
                    };

                    // Store this attempt if its new, otherwise increase the counter on how often we
                    // have seen this error.
                    *attempts.entry(attempt_error).or_insert(0) += 1;

                    // The liquidation call with this swap failed, continueing onto the next.
                    continue;
                }
            }
        }

        // Check if we failed because there were no paths, or if we failed due to an error.
        match attempts.into_iter().max_by_key(|(_, n)| *n) {
            Some((err, _)) => Err(err),
            None => Ok(None),
        }
    }
}
