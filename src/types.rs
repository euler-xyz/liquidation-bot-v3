use std::sync::Arc;

use alloy::primitives::{Address, U256};

#[derive(Clone, Debug)]
pub struct Vault {
    pub address: Address,
    pub asset: Address,
    pub unit_of_account: Address,
    pub borrow_interest_rate: (),
    pub supply_interest_rate: (),
    pub adapter: Address,
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct OracleIdentifier {
    pub base_asset: Address,
    pub quote_asset: Address,
    pub adapter: Address,
}

#[derive(Clone, Debug)]
pub struct Account {
    pub address: Address,
    pub debt: Vec<VaultDebtPosition>,
    pub assets: Vec<VaultAssetPosition>,
}

#[derive(Clone, Debug)]
pub struct VaultAssetPosition {
    pub amount: U256,
    pub vault: Arc<Vault>,
}

#[derive(Clone, Debug)]
pub struct VaultDebtPosition {
    pub amount: U256,
    pub vault: Arc<Vault>,
}

#[cfg(test)]
impl VaultDebtPosition {
    pub fn generate_random() -> VaultDebtPosition {
        VaultDebtPosition {
            amount: U256::from(100_000_000),
            vault: Arc::from(Vault::generate_random()),
        }
    }
}

#[cfg(test)]
impl VaultAssetPosition {
    pub fn generate_random() -> VaultAssetPosition {
        VaultAssetPosition {
            amount: U256::from(100_000_000),
            vault: Arc::from(Vault::generate_random()),
        }
    }
}

#[cfg(test)]
impl Vault {
    pub fn generate_random() -> Vault {
        Vault {
            address: Address::random(),
            asset: Address::random(),
            unit_of_account: Address::random(),
            borrow_interest_rate: (),
            supply_interest_rate: (),
            adapter: Address::random(),
        }
    }
}
