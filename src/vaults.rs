use std::{collections::HashMap, sync::Arc};

use alloy::{primitives::Address, providers::DynProvider, sol};

use crate::types::Vault;

pub struct Vaults {
    vault_lens: Address,
    vaults: HashMap<Address, Arc<Vault>>,
}

sol! {
    #[sol(rpc)]
    contract VaultLens {
        function getVaultInfoStatic(address vault) public view returns (VaultInfoStatic memory);
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
}

impl Vaults {
    pub fn new(vault_lens: Address) -> Vaults {
        Vaults {
            vault_lens,
            vaults: HashMap::new(),
        }
    }

    pub async fn get_or_fetch(&mut self, provider: &DynProvider, address: Address) -> Arc<Vault> {
        // Check if we already have it stored.
        match self.vaults.get(&address) {
            Some(vault) => vault.clone(),
            None => {
                // Fetch the vault and all its details.
                let lens = VaultLens::new(self.vault_lens, provider);
                let info = lens.getVaultInfoStatic(address).call().await.unwrap();
                let vault = Arc::from(Vault {
                    address,
                    asset: info.asset,
                    unit_of_account: info.unitOfAccount,
                    borrow_interest_rate: (),
                    supply_interest_rate: (),
                    adapter: info.oracle,
                });

                self.vaults.insert(address, vault.clone());

                vault
            }
        }
    }
}
