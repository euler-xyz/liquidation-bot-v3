use alloy::primitives::{self, Address, Bytes, U256};
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub status_code: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapQuote {
    pub amount_in: U256,
    pub amount_in_max: U256,
    pub amount_out: U256,
    pub amount_out_min: U256,
    pub account_in: Address,
    pub account_out: Address,
    pub vault_in: Address,
    pub receiver: Address,
    pub token_in: Token,
    pub token_out: Token,
    pub slippage: f64,
    pub estimated_gas: Option<U256>,
    pub swap: SwapPayload,
    pub verify: VerifyPayload,
    pub route: Vec<RouteStep>,
    pub unused_input_receiver: Option<Address>,
    pub transfer_output_to_receiver: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    chain_id: U256,
    address: Address,
    name: String,
    decimals: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapPayload {
    pub swapper_address: Address,
    pub swapper_data: primitives::Bytes,
    // pub multicall_items: Vec<MulticallItem>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MulticallItem {
    pub function_name: String,
    pub args: Option<serde_json::Value>,
    pub data: Bytes,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyPayload {
    pub verifier_address: Address,
    pub verifier_data: Bytes,
    #[serde(rename = "type")]
    pub verify_type: VerifyType,
    pub vault: Address,
    pub account: Address,
    pub amount: U256,
    pub deadline: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VerifyType {
    SkimMin,
    DebtMax,
    TransferMin,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteStep {
    pub provider_name: String,
}

pub async fn get_swap_quote(base_url: &str, params: &SwapParams) -> Result<Option<SwapQuote>> {
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
        reqwest::Url::parse_with_params(format!("{}/swap", base_url).as_str(), &query).unwrap();

    let response_body = client
        .get(url)
        .send()
        .await?
        .json::<SwapApiResponse>()
        .await?;

    // Make sure that the response was a success before attempting to deserialize the swapquote.
    if !response_body.success {
        return Ok(None);
    }

    match response_body.data {
        Some(data) => Ok(Some(serde_json::from_value(data)?)),
        None => Ok(None),
    }
}
