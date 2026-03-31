use std::{error::Error, sync::Arc};

use alloy::{
    primitives::{Address, U256},
    providers::DynProvider,
    sol,
    sol_types::ContractError,
};

use crate::{
    Vaults,
    types::{Account, Vault, VaultAssetPosition, VaultDebtPosition},
};

sol! {

    #[sol(rpc)]
    contract AccountLens {
        function getAccountEnabledVaultsInfo(address evc, address account)
            public
            view
            returns (AccountMultipleVaultsInfo memory);
    }


    #[derive(Debug)]
    struct AccountMultipleVaultsInfo {
        EVCAccountInfo evcAccountInfo;
        VaultAccountInfo[] vaultAccountInfo;
        AccountRewardInfo[] accountRewardInfo;
    }

    #[derive(Debug)]
    struct EVCAccountInfo {
        uint256 timestamp;
        address evc;
        address account;
        bytes19 addressPrefix;
        address owner;
        bool isLockdownMode;
        bool isPermitDisabledMode;
        uint256 lastAccountStatusCheckTimestamp;
        address[] enabledControllers;
        address[] enabledCollaterals;
    }

    #[derive(Debug)]
    struct AccountLiquidityInfo {
        bool queryFailure;
        bytes queryFailureReason;
        address account;
        address vault;
        address unitOfAccount;
        int256 timeToLiquidation;
        uint256 liabilityValueBorrowing;
        uint256 liabilityValueLiquidation;
        uint256 collateralValueBorrowing;
        uint256 collateralValueLiquidation;
        uint256 collateralValueRaw;
        address[] collaterals;
        uint256[] collateralValuesBorrowing;
        uint256[] collateralValuesLiquidation;
        uint256[] collateralValuesRaw;
    }

    #[derive(Debug)]
    struct AccountRewardInfo {
        uint256 timestamp;
        address account;
        address vault;
        address balanceTracker;
        bool balanceForwarderEnabled;
        uint256 balance;
        EnabledRewardInfo[] enabledRewardsInfo;
    }

    #[derive(Debug)]
    struct VaultAccountInfo {
        uint256 timestamp;
        address account;
        address vault;
        address asset;
        uint256 assetsAccount;
        uint256 shares;
        uint256 assets;
        uint256 borrowed;
        uint256 assetAllowanceVault;
        uint256 assetAllowanceVaultPermit2;
        uint256 assetAllowanceExpirationVaultPermit2;
        uint256 assetAllowancePermit2;
        bool balanceForwarderEnabled;
        bool isController;
        bool isCollateral;
        AccountLiquidityInfo liquidityInfo;
    }

    #[derive(Debug)]
    struct EnabledRewardInfo {
        address reward;
        uint256 earnedReward;
        uint256 earnedRewardRecentIgnored;
    }

}

pub async fn fetch_account(
    provider: DynProvider,
    vaults: &mut Vaults,
    lens: Address,
    evc: Address,
    account: Address,
) -> Account {
    let lens = AccountLens::new(lens, provider);
    let result = lens
        .getAccountEnabledVaultsInfo(evc, account)
        .call()
        .await
        .unwrap();

    let debt = result
        .vaultAccountInfo
        .iter()
        .flat_map(|a| {
            match a.borrowed {
                U256::ZERO => None,
                _ => {
                    Some(VaultDebtPosition {
                        amount: a.borrowed,
                        // NOTE: We should be caching vaults and referencing those instead of
                        // creating new ones.
                        vault: Arc::from(Vault {
                            address: a.vault,
                            asset: a.asset,
                            unit_of_account: a.liquidityInfo.unitOfAccount,
                            borrow_interest_rate: (),
                            supply_interest_rate: (),
                            // NOTE: this is incorrect, we should be checking what adapter/oracle it uses.
                            adapter: a.vault,
                        }),
                    })
                }
            }
        })
        .collect();

    let assets = result
        .vaultAccountInfo
        .iter()
        .flat_map(|a| {
            match a.assets {
                U256::ZERO => None,
                _ => {
                    Some(VaultAssetPosition {
                        amount: a.assets,
                        // NOTE: We should be caching vaults and referencing those instead of
                        // creating new ones.
                        vault: Arc::from(Vault {
                            address: a.vault,
                            asset: a.asset,
                            unit_of_account: a.liquidityInfo.unitOfAccount,
                            borrow_interest_rate: (),
                            supply_interest_rate: (),
                            // NOTE: this is incorrect, we should be checking what adapter/oracle it uses.
                            adapter: a.vault,
                        }),
                    })
                }
            }
        })
        .collect();

    Account {
        address: account,
        debt,
        assets,
    }
}
