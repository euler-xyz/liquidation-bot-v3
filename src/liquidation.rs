use std::collections::HashMap;

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
    oracles::ORACLE_PRICING_UNIT,
    pyth::PythFeedInput,
    swap::{SwapPayload, SwapQuoteProvider},
    types::{
        Account, LiquidationReasoning, LiquidationReasoningError, VaultBorrowPosition,
        VaultCollateralPosition,
    },
};

sol! {
    #[sol(rpc)]
    contract Liquidator {
        address public immutable owner;
        address public immutable swapperAddress;
        address public immutable swapVerifierAddress;
        address public immutable evcAddress;
        address public immutable PYTH;

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

/// The expected profit of a liquidation, denominated in the native asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedProfit {
    /// The profit converted into the native asset through the pricing API.
    Native(U256),
    /// The pricing API does not support this chain, so the profit in native terms can not be
    /// determined. Liquidations with an unknown profit are executed as long as they are
    /// possible, without any profitability check.
    Unknown,
}

#[derive(Debug, Clone)]
/// Contains all the data required to execute a liquidation.
pub struct PreparedLiquidation {
    // The account this liquidation is regarding.
    account: Account,
    // The borrow thats being repaid.
    borrow: VaultBorrowPosition,
    // The asset that is being liquidated.
    collateral: VaultCollateralPosition,
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
    // The resulting profit from the liquidation, converted into the native asset, or unknown
    // when the pricing API does not support this chain.
    profit: ExpectedProfit,
    // The profit from the liquidation in the original asset.
    profit_in_asset: U256,
}

/// Prepares a liquidation by calculating what the most profitable method is.
pub async fn prepare_liquidation(
    provider: &DynProvider,
    swap_provider: &impl SwapQuoteProvider,
    pyth: Option<PythFeedInput>,
    liquidator_address: Address,
    account: Account,
) -> Result<Option<PreparedLiquidation>, LiquidationReasoningError> {
    let borrow = match account.borrows.first() {
        Some(borrow) => borrow,
        // We can't liquidate an account that does not have any debt.
        None => return Ok(None),
    };

    let vault_address = borrow.vault.address;
    let vault = ILiquidation::new(vault_address, provider);

    // We track failed attempts and count how often a specific error occured.
    let mut attempts: HashMap<LiquidationReasoningError, usize> = HashMap::new();

    let mut prepared_liquidation: Option<PreparedLiquidation> = None;
    for asset in account.collaterals.iter() {
        // Calculate the result of the liquidation.
        let (max_repay, max_yield) = match pyth.clone() {
            Some(pyth) => {
                let liquidator = Liquidator::new(liquidator_address, provider);
                let liq_result = match liquidator
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
                    .await
                {
                    Ok(liq_result) => liq_result,
                    Err(err) => {
                        tracing::error!("Error while checking (pyth) liquidation, err: {:?}", err);

                        let attempt_error = LiquidationReasoningError::LiquidationRevert {
                            data: err.as_revert_data().unwrap_or_default(),
                        };

                        // Store this attempt if its new, otherwise increase the counter on how often we
                        // have seen this error.
                        *attempts.entry(attempt_error).or_insert(0) += 1;

                        continue;
                    }
                };

                (liq_result.maxRepay, liq_result.seizedCollateral)
            }
            None => {
                let liq_result = match vault
                    .checkLiquidation(liquidator_address, account.address, asset.vault.address)
                    .call()
                    .await
                {
                    Ok(liq_result) => liq_result,
                    Err(err) => {
                        tracing::error!("Error while checkingliquidation, err: {:?}", err);

                        let attempt_error = LiquidationReasoningError::LiquidationRevert {
                            data: err.as_revert_data().unwrap_or_default(),
                        };

                        // Store this attempt if its new, otherwise increase the counter on how often we
                        // have seen this error.
                        *attempts.entry(attempt_error).or_insert(0) += 1;

                        continue;
                    }
                };

                (liq_result.maxRepay, liq_result.maxYield)
            }
        };

        if max_repay.is_zero() || max_yield.is_zero() {
            continue;
        }

        let vault = Vault::new(asset.vault.address, provider);

        let max_assets = match vault.convertToAssets(max_yield).call().await {
            Ok(max_assets) => max_assets,
            Err(err) => {
                tracing::error!("Error while converting to assets, err: {:?}", err);

                let attempt_error = LiquidationReasoningError::LiquidationRevert {
                    data: err.as_revert_data().unwrap_or_default(),
                };

                // Store this attempt if its new, otherwise increase the counter on how often we
                // have seen this error.
                *attempts.entry(attempt_error).or_insert(0) += 1;

                continue;
            }
        };

        let new_potential_liquidation = PreparedLiquidation {
            account: account.clone(),
            borrow: borrow.clone(),
            collateral: asset.clone(),
            repay_amount: max_repay,
            seized_collateral_amount: max_assets,
            pyth: pyth.clone(),
            liquidator: liquidator_address,

            // These fields will be caldulated and set by the swap provider.
            swap: None,
            profit: ExpectedProfit::Native(U256::ZERO),
            profit_in_asset: U256::ZERO,
        };

        // Find the swap data for it.
        let new_potential_liquidation =
            match swap_provider.find_swap(new_potential_liquidation).await {
                Ok(Some(liq)) => liq,
                Ok(None) => {
                    continue;
                }
                Err(err) => {
                    tracing::error!(
                        "Issue while attempting to find a swap route, err: {:?}",
                        err
                    );

                    // Store this attempt if its new, otherwise increase the counter on how often we
                    // have seen this error.
                    *attempts.entry(err).or_insert(0) += 1;

                    continue;
                }
            };

        // Check if the profit from this would be higher than what we have previously found.
        if let Some(prepared) = &prepared_liquidation {
            let new_is_better = match (prepared.profit, new_potential_liquidation.profit) {
                (ExpectedProfit::Native(current), ExpectedProfit::Native(new)) => new >= current,
                // If the pricing API can not price this chain we can not compare in native
                // terms. Fall back to the profit in the borrow asset, which is comparable
                // across collaterals since an account only has a single borrow.
                _ => new_potential_liquidation.profit_in_asset >= prepared.profit_in_asset,
            };

            if !new_is_better {
                continue;
            }
        }

        // The profit will be higher so we store this as the best option.
        prepared_liquidation = Some(new_potential_liquidation);
    }

    // If we found a liquidation option then we return it, otherwise we check if we ran into any
    // error and which was the most common error and return that.
    match prepared_liquidation {
        Some(prepared_liquidation) => Ok(Some(prepared_liquidation)),
        None => match attempts.into_iter().max_by_key(|(_, n)| *n) {
            Some((err, _)) => Err(err),
            None => Ok(None),
        },
    }
}

impl PreparedLiquidation {
    /// Test-only constructor. Profit fields default to zero and can be set with
    /// [`PreparedLiquidation::with_profit`]; swap data with
    /// [`PreparedLiquidation::with_swap_data`].
    #[cfg(test)]
    pub(crate) fn new_for_test(
        account: Account,
        borrow: VaultBorrowPosition,
        collateral: VaultCollateralPosition,
        repay_amount: U256,
        seized_collateral_amount: U256,
        liquidator: Address,
        pyth: Option<PythFeedInput>,
    ) -> Self {
        PreparedLiquidation {
            account,
            borrow,
            collateral,
            repay_amount,
            seized_collateral_amount,
            pyth,
            swap: None,
            liquidator,
            profit: ExpectedProfit::Native(U256::ZERO),
            profit_in_asset: U256::ZERO,
        }
    }

    /// Builds a transaction from a preparedLiquidation.
    pub fn into_transaction(self, receiver: Address) -> TransactionRequest {
        let params = LiquidationParams {
            violatorAddress: self.account.address,
            vault: self.borrow.vault.address,
            borrowedAsset: self.borrow.vault.asset,
            collateralVault: self.collateral.vault.address,
            collateralAsset: self.collateral.vault.asset,
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

    pub fn set_account_status(&self, status: LiquidationReasoning) {
        self.account.set_status(status)
    }

    pub fn profit(&self) -> ExpectedProfit {
        self.profit
    }

    pub fn profit_in_asset(&self) -> U256 {
        self.profit_in_asset
    }

    pub fn collateral(&self) -> VaultCollateralPosition {
        self.collateral.clone()
    }

    pub fn borrow(&self) -> VaultBorrowPosition {
        self.borrow.clone()
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

    pub fn with_profit(mut self, profit: ExpectedProfit, profit_in_asset: U256) -> Self {
        self.profit = profit;
        self.profit_in_asset = profit_in_asset;
        self
    }
}

pub async fn get_shares_to_underlying(provider: &DynProvider, vault: Address) -> Result<U256> {
    Vault::new(vault, provider)
        .convertToAssets(U256::from(ORACLE_PRICING_UNIT))
        .call()
        .await
        .map_err(|e| {
            anyhow!(
                "Couldn't fetch shares to underlying ratio for vault {}, err: {:?}",
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
        config::{VaultFilter, load_configuration_file_for_test},
        lens::fetch_account,
        liquidation::prepare_liquidation,
        oracles::OraclesCache,
        prices::EulerPricingApi,
        pyth::fetch_pyth_data,
        swap::EulerSwapApi,
        vaults::Vaults,
    };

    #[tokio::test]
    async fn test_prepare_liquidation() {
        let rpc_url = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");
        let chain_id = 1;

        let config = load_configuration_file_for_test(&rpc_url, chain_id).unwrap();
        let provider = ProviderBuilder::new().connect_http(config.rpc_url).erased();

        // Our singleton vault store.
        let vaults = &mut Vaults::new(config.vault_lens_address);
        let oracles = OraclesCache::new(config.oracle_lens_address, config.pyth.clone());

        let account = address!("0x68e9669391AD60B5D72B996a9bd523c3962D2883");
        let liquidator_address = address!("0xAAF93d5475d092EA68a748137eE19D8130918392");

        // Fetch an account.
        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            config.account_lens_address,
            config.evc_address,
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
                    fetch_pyth_data(&provider, config.pyth.unwrap(), pyth_ids)
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
                    config.chain_id,
                    liquidator_address,
                    liquidator_address,
                    config.swapper_address,
                    config.wrapped_native_asset_address,
                    "1", // Max slippage.
                    EulerPricingApi::new(
                        "https://v3.euler.finance".parse().unwrap(),
                        config.chain_id
                    ),
                ),
                pyth,
                liquidator_address,
                account,
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn test_prepare_liquidation_with_pyth() {
        let rpc_url = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");
        let chain_id = 1;

        let config = load_configuration_file_for_test(&rpc_url, chain_id).unwrap();
        let provider = ProviderBuilder::new().connect_http(config.rpc_url).erased();

        // Our singleton vault store.
        let vaults = &mut Vaults::new(config.vault_lens_address);
        let oracles = OraclesCache::new(config.oracle_lens_address, config.pyth.clone());

        let account = address!("0xa8847b8bf827A9A8d03b2749Da4bC230A16c59d8");
        let liquidator_address = address!("0xAAF93d5475d092EA68a748137eE19D8130918392");

        // Fetch an account.
        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            config.account_lens_address,
            config.evc_address,
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
                    fetch_pyth_data(&provider, config.pyth.unwrap(), pyth_ids)
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
                    config.chain_id,
                    liquidator_address,
                    liquidator_address,
                    config.swapper_address,
                    config.wrapped_native_asset_address,
                    "1", // Max slippage.
                    EulerPricingApi::new(
                        "https://v3.euler.finance".parse().unwrap(),
                        config.chain_id
                    ),
                ),
                pyth,
                liquidator_address,
                account,
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn test_check_if_liquidateble() {
        let rpc_url = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");
        let chain_id = 1;

        let config = load_configuration_file_for_test(&rpc_url, chain_id).unwrap();
        let provider = ProviderBuilder::new().connect_http(config.rpc_url).erased();

        // Our singleton vault store.
        let vaults = &mut Vaults::new(config.vault_lens_address);
        let oracles = OraclesCache::new(config.oracle_lens_address, config.pyth.clone());

        let account = address!("0x421c4869095B637d59f25b427904D792dcBe0260");

        // Fetch an account.
        let account = fetch_account(
            provider.clone(),
            &VaultFilter::default(),
            vaults,
            config.account_lens_address,
            config.evc_address,
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

#[cfg(test)]
mod into_transaction_test {
    use std::{collections::HashMap, sync::Arc};

    use alloy::{
        primitives::{Address, Bytes, TxKind, U256},
        sol_types::SolCall,
    };

    use super::{Liquidator, PreparedLiquidation};
    use crate::{
        pyth::PythFeedInput,
        types::{Account, Vault, VaultBorrowPosition, VaultCollateralPosition},
    };

    fn liquidation(pyth: Option<PythFeedInput>) -> (PreparedLiquidation, Address) {
        let liquidator = Address::random();
        let make_vault = || {
            Arc::new(Vault {
                address: Address::random(),
                asset: Address::random(),
                unit_of_account: Address::random(),
                borrow_interest_rate: (),
                supply_interest_rate: (),
                shares_to_underlying_ratio: U256::from(1),
                adapter: Address::random(),
                ltvs: HashMap::new(),
            })
        };

        let borrow = VaultBorrowPosition {
            amount: U256::from(100),
            vault: make_vault(),
        };
        let collateral = VaultCollateralPosition {
            amount: U256::from(100),
            vault: make_vault(),
        };

        let liq = PreparedLiquidation::new_for_test(
            Account::new(Address::random(), vec![borrow.clone()], vec![collateral.clone()]),
            borrow,
            collateral,
            U256::from(100),
            U256::from(150),
            liquidator,
            pyth,
        );

        (liq, liquidator)
    }

    #[test]
    fn non_pyth_encodes_plain_liquidation_with_zero_value() {
        let (liq, liquidator) = liquidation(None);
        let receiver = Address::random();

        let tx = liq.into_transaction(receiver);

        // Calls the liquidator contract.
        assert_eq!(tx.to, Some(TxKind::Call(liquidator)));
        // No msg.value when there's no Pyth fee.
        assert_eq!(tx.value, Some(U256::ZERO));

        // Selector must be the plain single-collateral liquidation.
        let input = tx.input.input.unwrap();
        assert_eq!(
            &input[..4],
            Liquidator::liquidateSingleCollateralCall::SELECTOR.as_slice()
        );
    }

    #[test]
    fn pyth_encodes_pyth_liquidation_and_forwards_cost_as_value() {
        let cost = U256::from(4_242u64);
        let (liq, liquidator) = liquidation(Some(PythFeedInput {
            data: vec![Bytes::from_static(&[0xAB, 0xCD])],
            cost,
        }));
        let receiver = Address::random();

        let tx = liq.into_transaction(receiver);

        assert_eq!(tx.to, Some(TxKind::Call(liquidator)));
        // The Pyth update fee must be forwarded as msg.value.
        assert_eq!(tx.value, Some(cost));

        // Selector must be the Pyth variant.
        let input = tx.input.input.unwrap();
        assert_eq!(
            &input[..4],
            Liquidator::liquidateSingleCollateralWithPythOracleCall::SELECTOR.as_slice()
        );
    }
}
