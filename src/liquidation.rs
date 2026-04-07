use std::str::FromStr;

use alloy::{
    primitives::{Address, Bytes},
    providers::DynProvider,
    sol,
};
use anyhow::Result;
use anyhow::anyhow;
use itertools::Itertools;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    account::ILiquidation,
    oracles::{self, OraclesCache},
    types::Account,
};

sol! {
    #[sol(rpc)]
    contract Liquidator {
        function simulatePythUpdateAndCheckLiquidation(bytes[] calldata pythUpdateData, uint256 pythUpdateFee, address vaultAddress, address liquidatorAddress, address borrowerAddress, address collateralAddress) external payable returns (uint256 maxRepay, uint256 seizedCollateral);
    }
}

pub async fn liquidate_account(
    provider: &DynProvider,
    oracles: OraclesCache,
    liquidator: Address,
    account: Account,
) -> Result<()> {
    // First we check if any of the oracles this account makes use of are Pyth.
    // If so we need to fetch their most recent quotes.
    let mut pyth_ids = Vec::new();
    for oracle in account.dependent_on().iter() {
        oracles
            .fetch(provider, oracle.clone())
            .await?
            .pyth_ids()
            .iter()
            .for_each(|new_id| pyth_ids.push(*new_id));
    }

    // Fetch pyth data if needed.
    let pyth = match !pyth_ids.is_empty() {
        true => {
            // Call the Pyth API to fetch the most recent data for these oracles.
            let request_url = format!(
                "https://hermes.pyth.network/v2/updates/price/latest?ids[]={}",
                pyth_ids.iter().format("&ids[]=")
            );

            dbg!(pyth_ids, &request_url);

            let response: Value = reqwest::get(request_url).await?.json().await?;
            Some(Bytes::from_str(
                response["binary"]["data"]["0"]
                    .clone()
                    .as_str()
                    .ok_or(anyhow!("Could not get calldata from pyth resposne"))?,
            ))
        }
        false => None,
    };

    // TODO: Currently hardcoded value for Mainnet!

    // Simulate the liquidation to calculate the potential profit.
    let vault = ILiquidation::new(account.debt.first().unwrap().vault.address, provider);

    for asset in account.assets.iter() {
        let result = vault
            .checkLiquidation(liquidator, account.address, asset.vault.address)
            .call()
            .await?;

        dbg!(asset.vault.asset, result.maxRepay, result.maxYield);
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use alloy::{
        primitives::address,
        providers::{Provider, ProviderBuilder},
    };

    use crate::{
        lens::fetch_account, liquidation::liquidate_account, oracles::OraclesCache, vaults::Vaults,
    };

    const MAINNET_RPC_ENDPOINT: &str = "https://eth.rpc.blxrbdn.com";

    #[tokio::test]
    async fn test_liquidate_account() {
        let provider = ProviderBuilder::new()
            .connect_http(MAINNET_RPC_ENDPOINT.parse().unwrap())
            .erased();

        let oracle_lens = address!("0x30E6dFB84782A31d561536f64F47231451F7b48A");

        // Our singleton vault store.
        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));
        let oracles = OraclesCache::new(oracle_lens);

        let account = address!("0x819Ce254a22fF820765C85f07503F24268371E9e");
        let liquidator_address = address!("0xAAF93d5475d092EA68a748137eE19D8130918392");

        // Fetch an account.
        let account = fetch_account(
            provider.clone(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            account,
        )
        .await
        .unwrap();

        liquidate_account(&provider, oracles, liquidator_address, account)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_liquidate_account_pyth() {
        let provider = ProviderBuilder::new()
            .connect_http(MAINNET_RPC_ENDPOINT.parse().unwrap())
            .erased();

        let oracle_lens = address!("0x30E6dFB84782A31d561536f64F47231451F7b48A");

        // Our singleton vault store.
        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));
        let oracles = OraclesCache::new(oracle_lens);

        let account = address!("0xa8847b8bf827A9A8d03b2749Da4bC230A16c59d8");
        let liquidator_address = address!("0xAAF93d5475d092EA68a748137eE19D8130918392");

        // Fetch an account.
        let account = fetch_account(
            provider.clone(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            account,
        )
        .await
        .unwrap();

        dbg!(&account);

        liquidate_account(&provider, oracles, liquidator_address, account)
            .await
            .unwrap();
    }
}
