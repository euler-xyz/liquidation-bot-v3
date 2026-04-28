use anyhow::{Error, Result};

use alloy::{primitives::Address, providers::DynProvider, sol};
use tokio::time::Instant;
use tracing::debug;

use crate::{
    Vaults,
    config::VaultFilter,
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

#[derive(Debug)]
pub enum FetchAccountError {
    FilteredOut(Address),
    Other(Error),
}

pub async fn fetch_account(
    provider: DynProvider,
    filter: &VaultFilter,
    vaults: &mut Vaults,
    lens: Address,
    evc: Address,
    account: Address,
) -> Result<Account, FetchAccountError> {
    let lens = AccountLens::new(lens, &provider);

    //
    let start = Instant::now();
    let result = lens
        .getAccountEnabledVaultsInfo(evc, account)
        .call()
        .await
        .map_err(|e| FetchAccountError::Other(e.into()))?;

    debug!("Took {:?}", start.elapsed());

    let mut debt = Vec::new();
    let mut assets = Vec::new();
    for v in result.vaultAccountInfo.iter() {
        if !v.borrowed.is_zero() {
            // Check the filter to see if we should be indexing this.
            if filter.should_filter(v.vault) {
                return Err(FetchAccountError::FilteredOut(v.vault));
            }

            debt.push(VaultDebtPosition {
                amount: v.borrowed,
                vault: vaults
                    .get_or_fetch(&provider, v.vault)
                    .await
                    .map_err(FetchAccountError::Other)?,
            });
        }

        if !v.assets.is_zero() {
            assets.push(VaultAssetPosition {
                amount: v.assets,
                vault: vaults
                    .get_or_fetch(&provider, v.vault)
                    .await
                    .map_err(FetchAccountError::Other)?,
            });
        }
    }

    Ok(Account {
        address: account,
        debt,
        assets,
    })
}
