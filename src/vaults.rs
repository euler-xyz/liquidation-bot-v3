use std::{collections::HashMap, sync::Arc};

use alloy::{primitives::Address, providers::DynProvider, sol};

use crate::types::{LTV, Vault};
use anyhow::{Context, Result};

pub struct Vaults {
    vault_lens: Address,
    vaults: HashMap<Address, Arc<Vault>>,
}

sol! {
    #[sol(rpc)]
    contract VaultLens {
        function getVaultInfoStatic(address vault) public view returns (VaultInfoStatic memory);
        function getRecognizedCollateralsLTVInfo(address vault) public view returns (LTVInfo[] memory);
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
    pub fn new(vault_lens: Address) -> Vaults {
        Vaults {
            vault_lens,
            vaults: HashMap::new(),
        }
    }

    pub async fn get_or_fetch(
        &mut self,
        provider: &DynProvider,
        address: Address,
    ) -> Result<Arc<Vault>> {
        // Check if we already have it stored.
        match self.vaults.get(&address) {
            Some(vault) => Ok(vault.clone()),
            None => {
                // Fetch the vault and all its details.
                let lens = VaultLens::new(self.vault_lens, provider);
                // TODO: Combine the below 2 calls into a single one, or perform them at the same
                // time.
                let info = lens
                    .getVaultInfoStatic(address)
                    .call()
                    .await
                    .with_context(|| {
                        format!(
                            "Error while calling the VaultLens for vault {} using lens {}",
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

                let vault = Arc::from(Vault {
                    address,
                    asset: info.asset,
                    unit_of_account: info.unitOfAccount,
                    borrow_interest_rate: (),
                    supply_interest_rate: (),
                    adapter: info.oracle,
                    ltvs: ltv_info
                        .iter()
                        .map(|ltv| {
                            (
                                ltv.collateral,
                                LTV {
                                    asset: ltv.collateral,
                                    liquidation: ltv.liquidationLTV,
                                },
                            )
                        })
                        .collect(),
                });

                self.vaults.insert(address, vault.clone());

                Ok(vault)
            }
        }
    }
}
