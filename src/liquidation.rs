use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::{
    network::TransactionBuilder,
    primitives::{Address, Bytes, U256},
    providers::DynProvider,
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
};
use anyhow::{Result, anyhow};

use crate::{
    account::ILiquidation,
    pyth::PythFeedInput,
    swap::{SwapPayload, SwapQuoteProvider},
    types::{Account, VaultAssetPosition, VaultDebtPosition},
};

sol! {
    #[sol(rpc)]
    contract Liquidator {
        function simulatePythUpdateAndCheckLiquidation(bytes[] calldata pythUpdateData, uint256 pythUpdateFee, address vaultAddress, address liquidatorAddress, address borrowerAddress, address collateralAddress) external payable returns (uint256 maxRepay, uint256 seizedCollateral);
        function liquidateSingleCollateral(LiquidationParams calldata params, bytes[] calldata swapperData) external returns (bool success);
        function liquidateSingleCollateralWithPythOracle(LiquidationParams calldata params, bytes[] calldata swapperData, bytes[] calldata pythUpdateData) external payable returns (bool success);
    }

    struct LiquidationParams {
        address violatorAddress;
        address vault;
        address borrowedAsset;
        address collateralVault;
        address collateralAsset;
        uint256 repayAmount;
        uint256 seizedCollateralAmount;
        address receiver;
    }

    #[sol(rpc)]
    contract Vault {
        /// @notice Calculate amount of assets corresponding to the requested shares amount
        /// @param shares Amount of shares to convert
        /// @return The amount of assets
        function convertToAssets(uint256 shares) external view returns (uint256);
    }

}

#[derive(Debug, Clone)]
/// Contains all the data required to execute a liquidation.
pub struct PreparedLiquidation {
    // The account this liquidation is regarding.
    account: Account,
    // The debt thats being repaid.
    debt: VaultDebtPosition,
    // The asset that is being liquidated.
    asset: VaultAssetPosition,
    // The amount of debt that will be repaid.
    repay_amount: U256,
    // The amount of collateral being seized.
    seized_collateral_amount: U256,
    // The pyth data that will be required to perform the liquidation.
    pyth: Option<PythFeedInput>,
    // The swap data required to convert the assets into debt.
    swap: Option<SwapPayload>,
    // The liquidator contract being used.
    liquidator: Address,
    // The resulting profit from the liquidation, converted into ETH.
    profit: U256,
    // The profit from the liquidation in the original asset.
    profit_in_asset: U256,
}

/// Prepares a liquidation by calculating what the most profitable method is.
pub async fn prepare_liquidation(
    provider: &DynProvider,
    swap_provider: &impl SwapQuoteProvider,
    chain_id: u64,
    pyth: Option<PythFeedInput>,
    wrapped_native_asset_address: Address,
    liquidator_address: Address,
    swapper_address: Address,
    liquidator_eoa: Address,
    account: Account,
) -> Result<Option<PreparedLiquidation>> {
    let debt = match account.debt.first() {
        Some(debt) => debt,
        // We can't liquidate an account that does not have any debt.
        None => return Ok(None),
    };

    let vault_address = debt.vault.address;
    let vault = ILiquidation::new(vault_address, provider);

    let start = SystemTime::now();
    let since_the_epoch = match start.duration_since(UNIX_EPOCH) {
        Ok(since) => since,
        Err(err) => {
            return Err(anyhow!(
                "Issue while getting the current time, it appears to be moving backwards. err: {err}"
            ));
        }
    };

    let mut prepared_liquidation: Option<PreparedLiquidation> = None;
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

        let new_potential_liquidation = match debt.vault.asset == asset.vault.asset {
            true => {
                // Since these are in the same asset, the assets need to be more than the repay.
                if max_assets < max_repay {
                    continue;
                }

                PreparedLiquidation {
                    account: account.clone(),
                    debt: debt.clone(),
                    asset: asset.clone(),
                    repay_amount: max_repay,
                    seized_collateral_amount: max_assets,
                    pyth: pyth.clone(),
                    // No swap neeeded.
                    swap: None,
                    liquidator: liquidator_address,
                    profit: U256::ZERO,
                    profit_in_asset: max_assets - max_repay,
                }
            }
            false => {
                let liquidation = PreparedLiquidation {
                    account: account.clone(),
                    debt: debt.clone(),
                    asset: asset.clone(),
                    repay_amount: max_repay,
                    seized_collateral_amount: max_assets,
                    pyth: pyth.clone(),
                    liquidator: liquidator_address,

                    // These fields will be caldulated and set by the swap provider.
                    swap: None,
                    profit: U256::ZERO,
                    profit_in_asset: U256::ZERO,
                };

                // Find the swap data for it.
                match swap_provider.find_swap(liquidation).await {
                    Ok(Some(liq)) => liq,
                    Ok(None) => {
                        continue;
                    }
                    Err(err) => {
                        tracing::error!(
                            "Issue while attempting to find a swap route, err: {}",
                            err
                        );
                        continue;
                    }
                }
            }
        };

        // Check if the profit from this would be higher than what we have previously found.
        if let Some(prepared) = &prepared_liquidation
            && prepared.profit > new_potential_liquidation.profit
        {
            continue;
        }

        // The profit will be higher so we store this as the best option.
        prepared_liquidation = Some(new_potential_liquidation);
    }

    Ok(prepared_liquidation)
}

impl PreparedLiquidation {
    /// Builds a transaction from a preparedLiquidation.
    pub fn into_transaction(self, receiver: Address) -> TransactionRequest {
        let params = LiquidationParams {
            violatorAddress: self.account.address,
            vault: self.debt.vault.address,
            borrowedAsset: self.debt.vault.asset,
            collateralVault: self.asset.vault.address,
            collateralAsset: self.asset.vault.asset,
            repayAmount: self.repay_amount,
            seizedCollateralAmount: self.seized_collateral_amount,
            receiver,
        };

        let swap_data: Vec<Bytes> = self
            .swap
            .map(|s| s.multicall_items)
            .unwrap_or_default()
            .iter()
            .map(|mi| mi.data.clone())
            .collect();

        let (calldata, value) = match self.pyth {
            Some(pyth) => (
                Liquidator::liquidateSingleCollateralWithPythOracleCall {
                    params,
                    swapperData: swap_data,
                    pythUpdateData: pyth.data,
                }
                .abi_encode(),
                pyth.cost,
            ),
            None => (
                Liquidator::liquidateSingleCollateralCall {
                    params,
                    swapperData: swap_data,
                }
                .abi_encode(),
                U256::ZERO,
            ),
        };

        TransactionRequest::default()
            .with_to(self.liquidator)
            .with_input(calldata)
            .with_value(value)
    }

    pub fn account(&self) -> Address {
        self.account.address
    }

    pub fn profit(&self) -> U256 {
        self.profit
    }

    pub fn profit_in_asset(&self) -> U256 {
        self.profit_in_asset
    }

    pub fn asset(&self) -> VaultAssetPosition {
        self.asset.clone()
    }

    pub fn debt(&self) -> VaultDebtPosition {
        self.debt.clone()
    }

    pub fn repay_amount(&self) -> U256 {
        self.repay_amount
    }

    pub fn seized_collateral_amount(&self) -> U256 {
        self.seized_collateral_amount
    }

    pub fn pyth_cost(&self) -> U256 {
        match &self.pyth {
            Some(pyth) => pyth.cost,
            None => U256::ZERO,
        }
    }

    pub fn with_swap_data(mut self, swap_data: Option<SwapPayload>) -> Self {
        self.swap = swap_data;
        self
    }

    pub fn with_profit(mut self, native_profit: U256, profit_in_asset: U256) -> Self {
        self.profit = native_profit;
        self.profit_in_asset = profit_in_asset;
        self
    }
}

pub async fn get_shares_to_underlying(provider: &DynProvider, vault: Address) -> Result<U256> {
    Vault::new(vault, provider)
        .convertToAssets(U256::from(100_000))
        .call()
        .await
        .map_err(|e| {
            anyhow!(
                "Couldn't fetch shares to underlying ratio for vault {}, err: {}",
                vault,
                e
            )
        })
}

#[cfg(test)]
mod test {
    use alloy::{
        primitives::address,
        providers::{Provider, ProviderBuilder},
    };

    use crate::{
        config::VaultFilter, lens::fetch_account, liquidation::prepare_liquidation,
        oracles::OraclesCache, prices::EulerPricingApi, pyth::fetch_pyth_data, swap::EulerSwapApi,
        vaults::Vaults,
    };

    const MAINNET_RPC_ENDPOINT: &str = "https://eth.rpc.blxrbdn.com";

    #[tokio::test]
    async fn test_prepare_liquidation() {
        let provider = ProviderBuilder::new()
            .connect_http(MAINNET_RPC_ENDPOINT.parse().unwrap())
            .erased();

        let wrapped_native_asset = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let oracle_lens = address!("0x30E6dFB84782A31d561536f64F47231451F7b48A");
        let pyth_address = address!("0x4305FB66699C3B2702D4d05CF36551390A4c69C6");
        let swapper = address!("0x2Bba09866b6F1025258542478C39720A09B728bF");

        // Our singleton vault store.
        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));
        let oracles = OraclesCache::new(oracle_lens, pyth_address);

        let account = address!("0x68e9669391AD60B5D72B996a9bd523c3962D2883");
        let liquidator_address = address!("0xAAF93d5475d092EA68a748137eE19D8130918392");

        // Fetch an account.
        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            account,
        )
        .await
        .unwrap();

        // First we check if any of the oracles this account makes use of are Pyth.
        // If so we need to fetch their most recent quotes.
        let mut pyth_ids = Vec::new();
        for oracle in account.dependent_on().iter() {
            oracles
                .fetch_type(&provider, oracle.clone())
                .await
                .unwrap()
                .pyth_ids()
                .iter()
                .for_each(|new_id| pyth_ids.push(*new_id));
        }

        // Fetch pyth data if needed.
        let pyth = match !pyth_ids.is_empty() {
            true => {
                // Call the Pyth API to fetch the most recent data for these oracles.
                Some(
                    fetch_pyth_data(&provider, pyth_address, pyth_ids)
                        .await
                        .unwrap(),
                )
            }
            false => None,
        };

        dbg!(
            prepare_liquidation(
                &provider.clone(),
                &EulerSwapApi::new(
                    "https://swap.euler.finance".parse().unwrap(),
                    provider.erased(),
                    1,
                    liquidator_address,
                    liquidator_address,
                    swapper,
                    wrapped_native_asset,
                    EulerPricingApi::new("https://v3.eul.dev/".parse().unwrap(), 1),
                ),
                1,
                pyth,
                wrapped_native_asset,
                liquidator_address,
                swapper,
                liquidator_address,
                account,
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn test_prepare_liquidation_with_pyth() {
        let provider = ProviderBuilder::new()
            .connect_http(MAINNET_RPC_ENDPOINT.parse().unwrap())
            .erased();

        let wrapped_native_asset = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let oracle_lens = address!("0x30E6dFB84782A31d561536f64F47231451F7b48A");
        let pyth_address = address!("0x4305FB66699C3B2702D4d05CF36551390A4c69C6");
        let swapper = address!("0x2Bba09866b6F1025258542478C39720A09B728bF");

        // Our singleton vault store.
        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));
        let oracles = OraclesCache::new(oracle_lens, pyth_address);

        let account = address!("0xa8847b8bf827A9A8d03b2749Da4bC230A16c59d8");
        let liquidator_address = address!("0xAAF93d5475d092EA68a748137eE19D8130918392");

        // Fetch an account.
        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            account,
        )
        .await
        .unwrap();

        dbg!(&account);

        // First we check if any of the oracles this account makes use of are Pyth.
        // If so we need to fetch their most recent quotes.
        let mut pyth_ids = Vec::new();
        for oracle in account.dependent_on().iter() {
            oracles
                .fetch_type(&provider, oracle.clone())
                .await
                .unwrap()
                .pyth_ids()
                .iter()
                .for_each(|new_id| pyth_ids.push(*new_id));
        }

        // Fetch pyth data if needed.
        let pyth = match !pyth_ids.is_empty() {
            true => {
                // Call the Pyth API to fetch the most recent data for these oracles.
                Some(
                    fetch_pyth_data(&provider, pyth_address, pyth_ids)
                        .await
                        .unwrap(),
                )
            }
            false => None,
        };

        prepare_liquidation(
            &provider.clone(),
            &EulerSwapApi::new(
                "https://swap.euler.finance".parse().unwrap(),
                provider.erased(),
                1,
                liquidator_address,
                liquidator_address,
                swapper,
                wrapped_native_asset,
                EulerPricingApi::new("https://v3.eul.dev/".parse().unwrap(), 1),
            ),
            1,
            pyth,
            wrapped_native_asset,
            liquidator_address,
            swapper,
            liquidator_address,
            account,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_check_if_liquidateble() {
        let account = address!("0x421c4869095B637d59f25b427904D792dcBe0260");

        let provider = ProviderBuilder::new()
            .connect_http(MAINNET_RPC_ENDPOINT.parse().unwrap())
            .erased();

        let oracle_lens = address!("0x30E6dFB84782A31d561536f64F47231451F7b48A");
        let pyth_address = address!("0x4305FB66699C3B2702D4d05CF36551390A4c69C6");

        // Our singleton vault store.
        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));
        let oracles = OraclesCache::new(oracle_lens, pyth_address);

        // Fetch an account.
        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            account,
        )
        .await
        .unwrap();

        oracles
            .ensure_prices_for(&provider, account.dependent_on())
            .await;

        dbg!(account.calculate_health(&oracles).unwrap().is_unhealthy());
    }
}
