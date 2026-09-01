use alloy::{primitives::Address, providers::DynProvider, sol};
use anyhow::{Error, Result, anyhow};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::{
    Vaults,
    config::VaultFilter,
    types::{
        Account, LensError, LiquidationReasoning, VaultBorrowPosition, VaultCollateralPosition,
    },
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
        bool queryFailure;
        bytes queryFailureReason;
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
    vaults: &Vaults,
    account_lens: Address,
    evc: Address,
    account: Address,
) -> Result<Account, FetchAccountError> {
    let lens = AccountLens::new(account_lens, &provider);

    //
    let start = Instant::now();
    let result = lens
        .getAccountEnabledVaultsInfo(evc, account)
        .call()
        .await
        .map_err(|e| FetchAccountError::Other(e.into()))?;

    debug!("Took {:?}", start.elapsed());

    let mut borrows = Vec::new();
    let mut collaterals = Vec::new();
    let mut lens_error = None;
    let mut blacklisted_vaults = Vec::new();
    for v in result.vaultAccountInfo.iter() {
        // Check if a query failure happened in the lens when attempting to fetch the information
        // for this vault.
        if v.queryFailure {
            lens_error = Some(LensError {
                vault: v.vault,
                query_failure_reason: v.queryFailureReason.clone(),
            });
            continue;
        }

        if !v.borrowed.is_zero() {
            // A whitelist excludes this vault entirely: the account is out of scope for
            // this bot, drop it.
            if filter.should_filter(v.vault) {
                return Err(FetchAccountError::FilteredOut(v.vault));
            }

            // A blacklisted vault does *not* drop the account. We still want it tracked (so
            // it stays visible for observability); it is marked below and, further up the
            // pipeline, never considered for liquidation.
            let is_blacklisted_vault = filter.is_blacklisted(v.vault);
            if is_blacklisted_vault {
                blacklisted_vaults.push(v.vault);
            }

            match vaults.get_or_fetch(&provider, v.vault).await {
                Ok(vault) => {
                    // Only an EVault can be borrowed from.
                    let evault = vault
                        .as_evault()
                        .ok_or_else(|| {
                            FetchAccountError::Other(anyhow!(
                                "Account {} has a borrow on vault {} which is not an EVault, this should be impossible",
                                account,
                                v.vault
                            ))
                        })?
                        .clone();

                    borrows.push(VaultBorrowPosition {
                        amount: v.borrowed,
                        vault: evault,
                    });
                }
                // Vaults often get blacklisted precisely because something is wrong with
                // them on-chain, so a failure to fetch its metadata here is expected. Don't
                // let it hide the whole account - track it without this borrow leg instead.
                Err(e) if is_blacklisted_vault => {
                    warn!(
                        "Could not fetch metadata for blacklisted vault {} on account {}, tracking the account without this borrow position, err: {:?}",
                        v.vault, account, e
                    );
                }
                Err(e) => return Err(FetchAccountError::Other(e)),
            }
        }

        if !v.assets.is_zero() {
            collaterals.push(VaultCollateralPosition {
                amount: v.shares,
                vault: vaults
                    .get_or_fetch(&provider, v.vault)
                    .await
                    .map_err(FetchAccountError::Other)?,
            });
        }
    }

    // Create the account.
    let account = Account::new(account, borrows, collaterals);

    // If an error occured, update the account to contain the lens error.
    if let Some(error) = lens_error {
        account.set_status(crate::types::LiquidationReasoning::Error(
            crate::types::LiquidationReasoningError::LensError {
                error,
                state: Box::from(LiquidationReasoning::Unknown),
            },
        ));
    }

    // If any of the account's borrows are in a blacklisted vault, mark it. This is set
    // after the lens-error status above so `Account::set_status`'s merge logic nests it
    // correctly instead of one silently overwriting the other.
    if !blacklisted_vaults.is_empty() {
        account.set_status(LiquidationReasoning::Blacklisted(blacklisted_vaults));
    }

    Ok(account)
}
