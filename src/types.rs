use std::{collections::HashMap, sync::Arc};

use alloy::primitives::{Address, U256};
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Vault {
    pub address: Address,
    pub asset: Address,
    pub unit_of_account: Address,
    pub borrow_interest_rate: (),
    pub supply_interest_rate: (),
    pub shares_to_underlying_ratio: U256,
    pub adapter: Address,
    pub ltvs: HashMap<Address, Ltv>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Ltv {
    asset: Address,
    borrow_ltv: U256,
    liquidation_ltv: U256,
    initial_liquidation_ltv: U256,
    target_timestamp: U256,
    ramp_duration: U256,
}

#[derive(PartialEq, Eq, Hash, Clone, Debug, Serialize)]
pub struct OracleIdentifier {
    pub base_asset: Address,
    pub quote_asset: Address,
    pub adapter: Address,
}

#[derive(Clone, Debug, Serialize)]
pub struct Account {
    pub address: Address,
    pub borrows: Vec<VaultBorrowPosition>,
    pub collaterals: Vec<VaultCollateralPosition>,
}

#[derive(Clone, Debug, Serialize)]
pub struct VaultCollateralPosition {
    pub amount: U256,
    pub vault: Arc<Vault>,
}

#[derive(Clone, Debug, Serialize)]
pub struct VaultBorrowPosition {
    pub amount: U256,
    pub vault: Arc<Vault>,
}

impl Ltv {
    pub fn new(
        asset: Address,
        borrow_ltv: U256,
        liquidation_ltv: U256,
        initial_liquidation_ltv: U256,
        target_timestamp: U256,
        ramp_duration: U256,
    ) -> Self {
        Ltv {
            asset,
            borrow_ltv,
            liquidation_ltv,
            initial_liquidation_ltv,
            target_timestamp,
            ramp_duration,
        }
    }

    pub fn calculate_liquidation_ltv(&self, time: DateTime<Utc>) -> U256 {
        let timestamp = U256::from(time.timestamp());

        if U256::from(timestamp) >= self.target_timestamp
            || self.liquidation_ltv >= self.initial_liquidation_ltv
        {
            return self.liquidation_ltv;
        }

        let time_remaining = self.target_timestamp - timestamp;

        // Invariants guaranteed by the branches above:
        //   target < initial         (so `initial - target` does not underflow)
        //   time_remaining <= ramp_duration
        self.liquidation_ltv
            + (self.initial_liquidation_ltv - self.liquidation_ltv) * time_remaining
                / self.ramp_duration
    }

    // Calculates the current liquidation ltv for the asset.
    pub fn current_liquidation_ltv(&self) -> U256 {
        self.calculate_liquidation_ltv(Utc::now())
    }
}

#[cfg(test)]
impl VaultBorrowPosition {
    pub fn generate_random() -> Self {
        VaultBorrowPosition {
            amount: U256::from(100_000_000),
            vault: Arc::from(Vault::generate_random()),
        }
    }
}

#[cfg(test)]
impl VaultCollateralPosition {
    pub fn generate_random() -> Self {
        VaultCollateralPosition {
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
            shares_to_underlying_ratio: U256::from(100_000),
            adapter: Address::random(),
            ltvs: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod test {
    use alloy::primitives::{Address, U256};
    use chrono::DateTime;

    use crate::types::Ltv;

    #[test]
    pub fn calculate_ramping_lltv() {
        // Ramps down to zero.
        let ltv = Ltv {
            asset: Address::random(),
            borrow_ltv: U256::ZERO,
            liquidation_ltv: U256::ZERO,
            initial_liquidation_ltv: U256::from(9500),
            target_timestamp: U256::from(1780233359),
            ramp_duration: U256::from(2592000),
        };

        let time = DateTime::from_timestamp(1778657500, 0).unwrap();
        let lltv = ltv.calculate_liquidation_ltv(time);

        // `5775` is the reported number from the Euler UI.
        assert_eq!(lltv, U256::from(5775));
    }
}
