use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use alloy::primitives::{Address, Bytes, U256};
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
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
/// This enum reports the reason for why an account is not being liquidated.
pub enum LiquidationReasoning {
    // The health status of the account is unknown.
    Unknown,
    // The account is healthy there is no reason to consider a liquidation.
    Healthy,
    // The account could be liquidated but doing so is unprofitable.
    Unprofitable,
    // We can not liquidate this account as we can not find a swap path.
    NoSwapPath,
    // There is an error that is preventing this account from being liquidatable.
    Error(LiquidationReasoningError),
}

#[derive(Clone, Debug, Serialize, Hash, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LiquidationReasoningError {
    OracleError {
        // TODO: Add this back in, for now its not easy to get so skipping it.
        // oracle: Address,
        message: String,
    },
    // If the liquidation reverts during a simulation we store the revert data.
    LiquidationRevert {
        data: Bytes,
    },
    Other {
        message: String,
    },
}

impl From<alloy::transports::RpcError<alloy::transports::TransportErrorKind>>
    for LiquidationReasoningError
{
    fn from(err: alloy::transports::RpcError<alloy::transports::TransportErrorKind>) -> Self {
        match err {
            alloy::transports::RpcError::ErrorResp(error_payload) => {
                LiquidationReasoningError::LiquidationRevert {
                    data: error_payload.as_revert_data().unwrap_or_default(),
                }
            }
            _ => LiquidationReasoningError::Other {
                message: "RPC Error".to_string(),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Account {
    pub address: Address,
    pub borrows: Vec<VaultBorrowPosition>,
    pub collaterals: Vec<VaultCollateralPosition>,

    status: Arc<RwLock<AccountStatus>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountStatus {
    // The status of the account.
    status: LiquidationReasoning,
    // The date at which it was last updated.
    time: DateTime<Utc>,
}

impl AccountStatus {
    pub fn new() -> Self {
        AccountStatus {
            status: LiquidationReasoning::Unknown,
            time: Utc::now(),
        }
    }

    pub fn from(status: LiquidationReasoning) -> Self {
        AccountStatus {
            status,
            time: Utc::now(),
        }
    }
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

impl Account {
    pub fn new(
        address: Address,
        borrows: Vec<VaultBorrowPosition>,
        collaterals: Vec<VaultCollateralPosition>,
    ) -> Self {
        Account {
            address,
            borrows,
            collaterals,
            status: Arc::from(RwLock::from(AccountStatus::new())),
        }
    }

    // Attempt to update the status. If we can't get the lock then its not an issue as this is
    // non-critical and only for observability.
    pub fn set_status(&self, status: LiquidationReasoning) {
        if let Ok(mut s) = self.status.try_write() {
            *s = AccountStatus::from(status);
        }
    }
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
