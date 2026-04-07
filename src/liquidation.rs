use std::{
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{
    primitives::{Address, Bytes, U256, address},
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
    pyth::fetch_pyth_data,
    swap::{SwapParams, get_swap_quote},
    types::Account,
};

sol! {
    #[sol(rpc)]
    contract Liquidator {
        function simulatePythUpdateAndCheckLiquidation(bytes[] calldata pythUpdateData, uint256 pythUpdateFee, address vaultAddress, address liquidatorAddress, address borrowerAddress, address collateralAddress) external payable returns (uint256 maxRepay, uint256 seizedCollateral);
    }

    #[sol(rpc)]
    contract Vault {
        /// @notice Calculate amount of assets corresponding to the requested shares amount
        /// @param shares Amount of shares to convert
        /// @return The amount of assets
        function convertToAssets(uint256 shares) external view returns (uint256);
    }

}

pub async fn liquidate_account(
    provider: &DynProvider,
    oracles: OraclesCache,
    pyth_address: Address,
    liquidator_address: Address,
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
            Some(fetch_pyth_data(provider, pyth_address, pyth_ids).await?)
        }
        false => None,
    };

    // Simulate the liquidation to calculate the potential profit.
    let debt = account.debt.first().unwrap();
    let vault_address = debt.vault.address;
    let vault = ILiquidation::new(vault_address, provider);

    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .expect("time should go forward");

    for asset in account.assets.iter() {
        // Calculate the result of the liquidation.
        let (max_repay, max_yield) = match pyth.clone() {
            Some(pyth) => {
                let liquidator = Liquidator::new(liquidator_address, provider);
                let liq_result = liquidator
                    .simulatePythUpdateAndCheckLiquidation(
                        pyth.data,
                        pyth.cost,
                        vault_address,
                        // TODO: this should be the signer address.
                        liquidator_address,
                        account.address,
                        asset.vault.address,
                    )
                    .value(pyth.cost)
                    .call()
                    .await?;

                (liq_result.maxRepay, liq_result.seizedCollateral)
            }
            None => {
                let liq_result = vault
                    .checkLiquidation(liquidator_address, account.address, asset.vault.address)
                    .call()
                    .await?;

                (liq_result.maxRepay, liq_result.maxYield)
            }
        };

        if max_repay.is_zero() || max_yield.is_zero() {
            continue;
        }

        let vault = Vault::new(asset.vault.address, provider);
        let max_assets = vault.convertToAssets(max_yield).call().await?;

        // Have the swap api calculate attempt to convert the colleteral to the debt.
        let swap_result = get_swap_quote(
            "https://swap.euler.finance",
            &SwapParams {
                chain_id: "1".to_string(),
                token_in: asset.vault.asset,
                token_out: debt.vault.asset,
                receiver: liquidator_address,
                vault_in: asset.vault.address,
                // TODO: this should be the signer address
                origin: liquidator_address,
                account_in: liquidator_address,
                account_out: liquidator_address,
                amount: max_assets,
                target_debt: U256::ZERO,
                current_debt: max_repay,
                swapper_mode: "0".to_string(),
                slippage: "0.5".to_string(),
                deadline: since_the_epoch.as_secs().to_string(),
                is_repay: "false".to_string(),
                dust_account: None,
                unused_input_receiver: None,
                transfer_output_to_receiver: None,
                skip_sweep_deposit_out: None,
                routing_override: None,
                provider: None,
            },
        )
        .await?;

        dbg!(swap_result);

        dbg!(asset.vault.asset, max_repay, max_yield, max_assets);
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
        let pyth_address = address!("0x4305FB66699C3B2702D4d05CF36551390A4c69C6");

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

        liquidate_account(
            &provider,
            oracles,
            pyth_address,
            liquidator_address,
            account,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_liquidate_account_pyth() {
        let provider = ProviderBuilder::new()
            .connect_http(MAINNET_RPC_ENDPOINT.parse().unwrap())
            .erased();

        let oracle_lens = address!("0x30E6dFB84782A31d561536f64F47231451F7b48A");
        let pyth_address = address!("0x4305FB66699C3B2702D4d05CF36551390A4c69C6");

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

        liquidate_account(
            &provider,
            oracles,
            pyth_address,
            liquidator_address,
            account,
        )
        .await
        .unwrap();
    }
}
