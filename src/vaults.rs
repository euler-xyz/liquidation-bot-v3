use crate::{
    liquidation::get_shares_to_underlying,
    types::{EVault, Erc4626Vault, Ltv, Vault},
};
use alloy::{primitives::Address, providers::DynProvider, sol};
use anyhow::{Context, Result};
use dashmap::DashMap;
use std::sync::Arc;
use tracing::debug;

pub struct Vaults {
    vault_lens: Address,
    utils_lens: Address,
    vaults: DashMap<Address, Vault>,
}

sol! {
    #[sol(rpc)]
    contract VaultLens {
        function getVaultInfoStatic(address vault) public view returns (VaultInfoStatic memory);
        function getRecognizedCollateralsLTVInfo(address vault) public view returns (LTVInfo[] memory);
    }

    #[sol(rpc)]
    contract UtilsLens {
        function getVaultInfoERC4626(address vault) public view returns (VaultInfoERC4626 memory);
    }

    struct VaultInfoERC4626 {
        uint256 timestamp;
        address vault;
        string vaultName;
        string vaultSymbol;
        uint256 vaultDecimals;
        address asset;
        string assetName;
        string assetSymbol;
        uint256 assetDecimals;
        uint256 totalShares;
        uint256 totalAssets;
        bool isEVault;
    }

    struct VaultInfoStatic {
        uint256 timestamp;
        address vault;
        string vaultName;
        string vaultSymbol;
        uint256 vaultDecimals;
        address asset;
        string assetName;
        string assetSymbol;
        uint256 assetDecimals;
        address unitOfAccount;
        string unitOfAccountName;
        string unitOfAccountSymbol;
        uint256 unitOfAccountDecimals;
        address dToken;
        address oracle;
        address evc;
        address protocolConfig;
        address balanceTracker;
        address permit2;
        address creator;
    }

    struct LTVInfo {
        address collateral;
        uint256 borrowLTV;
        uint256 liquidationLTV;
        uint256 initialLiquidationLTV;
        uint256 targetTimestamp;
        uint256 rampDuration;
    }
}

impl Vaults {
    pub fn new(vault_lens: Address, utils_lens: Address) -> Vaults {
        Vaults {
            vault_lens,
            utils_lens,
            vaults: DashMap::new(),
        }
    }

    pub async fn get_or_fetch(&self, provider: &DynProvider, address: Address) -> Result<Vault> {
        // Check if we already have it stored.
        if let Some(vault) = self.vaults.get(&address) {
            return Ok(vault.clone());
        }

        // Attempt to fetch it as an EVault first. If the lens calls fail we assume the
        // address is not an EVault but a plain ERC4626 vault.
        let vault = match self.fetch_evault(provider, address).await {
            Ok(evault) => Vault::EVault(Arc::from(evault)),
            Err(err) => {
                debug!(
                    vault =? address,
                    err =? err,
                    "Fetching the vault info from the VaultLens failed, assuming this is a plain ERC4626 vault"
                );

                Vault::Erc4626(Arc::from(self.fetch_erc4626(provider, address).await?))
            }
        };

        self.vaults.insert(address, vault.clone());

        Ok(vault)
    }

    /// Fetches the full EVault details through the VaultLens. Fails if the address is
    /// not an EVault.
    async fn fetch_evault(&self, provider: &DynProvider, address: Address) -> Result<EVault> {
        let lens = VaultLens::new(self.vault_lens, provider);
        // TODO: Combine the below 2 calls into a single one, or perform them at the same
        // time.
        let info = lens
            .getVaultInfoStatic(address)
            .call()
            .await
            .with_context(|| {
                format!(
                    "Error while calling the VaultLens for static information on vault {} using lens {}",
                    address, self.vault_lens
                )
            })?;

        let ltv_info = lens
            .getRecognizedCollateralsLTVInfo(address)
            .call()
            .await
            .with_context(|| {
                format!(
                    "Error while calling the VaultLens for vault {} using lens {}",
                    address, self.vault_lens
                )
            })?;

        Ok(EVault {
            erc4626: Erc4626Vault {
                address,
                asset: info.asset,
                shares_to_underlying_ratio: get_shares_to_underlying(provider, address).await?,
            },
            unit_of_account: info.unitOfAccount,
            borrow_interest_rate: (),
            supply_interest_rate: (),
            adapter: info.oracle,
            ltvs: ltv_info
                .iter()
                .map(|ltv| {
                    (
                        ltv.collateral,
                        Ltv::new(
                            ltv.collateral,
                            ltv.borrowLTV,
                            ltv.liquidationLTV,
                            ltv.initialLiquidationLTV,
                            ltv.targetTimestamp,
                            ltv.rampDuration,
                        ),
                    )
                })
                .collect(),
        })
    }

    /// Fetches the details of a plain ERC4626 vault.
    async fn fetch_erc4626(
        &self,
        provider: &DynProvider,
        address: Address,
    ) -> Result<Erc4626Vault> {
        let lens = UtilsLens::new(self.utils_lens, provider);
        let info = lens.getVaultInfoERC4626(address)
            .call()
            .await
            .with_context(|| {
                format!(
                    "Error while calling the UtilsLens for ERC4626 information on vault {} using lens {}",
                    address, self.utils_lens
                )
            })?;

        Ok(Erc4626Vault {
            address,
            asset: info.asset,
            shares_to_underlying_ratio: get_shares_to_underlying(provider, address).await?,
        })
    }
}
