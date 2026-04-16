use anyhow::Result;

use alloy::{primitives::Address, providers::DynProvider, sol};

use crate::{
    Vaults,
    types::{Account, VaultAssetPosition, VaultDebtPosition},
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
) -> Result<Account> {
    let lens = AccountLens::new(lens, &provider);
    let result = lens
        .getAccountEnabledVaultsInfo(evc, account)
        .call()
        .await?;

    let mut debt = Vec::new();
    let mut assets = Vec::new();
    for v in result.vaultAccountInfo.iter() {
        if !v.borrowed.is_zero() {
            debt.push(VaultDebtPosition {
                amount: v.borrowed,
                vault: vaults.get_or_fetch(&provider, v.vault).await?,
            });
        }

        if !v.assets.is_zero() {
            assets.push(VaultAssetPosition {
                amount: v.assets,
                vault: vaults.get_or_fetch(&provider, v.vault).await?,
            });
        }
    }

    Ok(Account {
        address: account,
        debt,
        assets,
    })
}
