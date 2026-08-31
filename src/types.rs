use std::{
    collections::HashMap,
    ops::Deref,
    sync::{Arc, RwLock},
};

use alloy::primitives::{Address, Bytes, U256};
use chrono::{DateTime, Utc};
use serde::Serialize;

/// A plain ERC4626 vault. These can only ever be used as collateral, never as the
/// borrow (controller) vault.
#[derive(Clone, Debug, Serialize)]
pub struct Erc4626Vault {
    pub address: Address,
    pub asset: Address,
    pub shares_to_underlying_ratio: U256,
}

/// An Euler vault. An EVault is itself an ERC4626 vault but extends it with
/// borrowing related features. Only an EVault can be the borrow (controller) vault.
#[derive(Clone, Debug, Serialize)]
pub struct EVault {
    #[serde(flatten)]
    pub erc4626: Erc4626Vault,
    pub unit_of_account: Address,
    pub borrow_interest_rate: (),
    pub supply_interest_rate: (),
    pub adapter: Address,
    pub ltvs: HashMap<Address, Ltv>,
}

/// An EVault IS-A ERC4626 vault, so allow direct access to the common fields
/// (`address`, `asset`, `shares_to_underlying_ratio`).
impl Deref for EVault {
    type Target = Erc4626Vault;

    fn deref(&self) -> &Self::Target {
        &self.erc4626
    }
}

/// Any vault known to the bot. Both variants are ERC4626 vaults, use
/// [`Vault::erc4626`] to access the common fields.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum Vault {
    EVault(Arc<EVault>),
    Erc4626(Arc<Erc4626Vault>),
}

impl Vault {
    /// The common ERC4626 fields, available for every vault.
    pub fn erc4626(&self) -> &Erc4626Vault {
        match self {
            Vault::EVault(vault) => &vault.erc4626,
            Vault::Erc4626(vault) => vault,
        }
    }

    /// Returns the EVault if this vault is one. Only EVaults can be borrowed from.
    pub fn as_evault(&self) -> Option<&Arc<EVault>> {
        match self {
            Vault::EVault(vault) => Some(vault),
            Vault::Erc4626(_) => None,
        }
    }
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

#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize)]
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
    // The account holds a position in one or more vaults that are configured as
    // blacklisted. It is still tracked (so it stays visible for observability) but will
    // never be considered for liquidation. Carries the blacklisted vault address(es) that
    // caused this.
    Blacklisted(Vec<Address>),
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
    // This type of error means that while loading the account from the lens we already had an
    // issue. So we likely do not have a correct status of the account. But this type can hold an
    // additional state can show the processing state.
    LensError {
        error: LensError,
        state: Box<LiquidationReasoning>,
    },
    Other {
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Hash, PartialEq, Eq)]
pub struct LensError {
    pub vault: Address,
    pub query_failure_reason: Bytes,
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
    // Any vault can serve as collateral.
    pub vault: Vault,
}

#[derive(Clone, Debug, Serialize)]
pub struct VaultBorrowPosition {
    pub amount: U256,
    // Only an EVault can be borrowed from, which this encodes statically.
    pub vault: Arc<EVault>,
}

/// Whether a `LiquidationReasoning` value represents (or, in the case of a lens error,
/// wraps) a blacklisted status. Recurses into `LensError`'s nested `state` since
/// `Account::set_status` nests a newer status inside an existing lens error rather than
/// discarding it.
fn is_blacklisted_reasoning(reasoning: &LiquidationReasoning) -> bool {
    match reasoning {
        LiquidationReasoning::Blacklisted(_) => true,
        LiquidationReasoning::Error(LiquidationReasoningError::LensError { state, .. }) => {
            is_blacklisted_reasoning(state)
        }
        _ => false,
    }
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
            match (&s.status, &status) {
                // If this new value is a lens error, we replace.
                (
                    _,
                    LiquidationReasoning::Error(LiquidationReasoningError::LensError {
                        error,
                        state,
                    }),
                ) => {
                    *s = AccountStatus::from(status);
                }

                // If the old one is a lens error, we replace the inner state.
                (
                    LiquidationReasoning::Error(LiquidationReasoningError::LensError {
                        error,
                        state,
                    }),
                    _,
                ) => {
                    // In this very specific case we nest the state within the LensError,
                    // this shows its internal state while preserving the fact that this
                    // account had some issue being fetched from the lens.
                    *s = AccountStatus::from(LiquidationReasoning::Error(
                        LiquidationReasoningError::LensError {
                            error: error.clone(),
                            state: Box::from(status),
                        },
                    ));
                }

                // If neither is a lens error we replace the entire status.
                _ => {
                    *s = AccountStatus::from(status);
                }
            };
        }
    }

    /// True if this account is currently marked as blacklisted (directly, or nested inside
    /// a lens-error status that occurred after it was blacklisted). Blacklisted accounts
    /// stay tracked and visible via the observability API, but must never be considered for
    /// liquidation.
    pub fn is_blacklisted(&self) -> bool {
        self.status
            .read()
            .map(|s| is_blacklisted_reasoning(&s.status))
            .unwrap_or(false)
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
            vault: Arc::from(EVault::generate_random()),
        }
    }
}

#[cfg(test)]
impl VaultCollateralPosition {
    pub fn generate_random() -> Self {
        VaultCollateralPosition {
            amount: U256::from(100_000_000),
            vault: Vault::EVault(Arc::from(EVault::generate_random())),
        }
    }
}

#[cfg(test)]
impl Erc4626Vault {
    pub fn generate_random() -> Erc4626Vault {
        Erc4626Vault {
            address: Address::random(),
            asset: Address::random(),
            shares_to_underlying_ratio: U256::from(100_000),
        }
    }
}

#[cfg(test)]
impl EVault {
    pub fn generate_random() -> EVault {
        EVault {
            erc4626: Erc4626Vault::generate_random(),
            unit_of_account: Address::random(),
            borrow_interest_rate: (),
            supply_interest_rate: (),
            adapter: Address::random(),
            ltvs: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod test {
    use alloy::primitives::{Address, Bytes, U256};
    use chrono::DateTime;

    use crate::types::{
        Account, LensError, LiquidationReasoning, LiquidationReasoningError, Ltv,
        VaultBorrowPosition,
    };

    #[test]
    fn is_blacklisted_true_when_status_is_blacklisted() {
        let account = Account::new(
            Address::random(),
            vec![VaultBorrowPosition::generate_random()],
            vec![],
        );
        assert!(!account.is_blacklisted());

        account.set_status(LiquidationReasoning::Blacklisted(vec![Address::random()]));
        assert!(account.is_blacklisted());
    }

    #[test]
    fn is_blacklisted_true_when_nested_inside_a_lens_error() {
        let account = Account::new(
            Address::random(),
            vec![VaultBorrowPosition::generate_random()],
            vec![],
        );

        // Simulate a lens error happening first (e.g. from a different vault's query
        // failure)...
        account.set_status(LiquidationReasoning::Error(
            LiquidationReasoningError::LensError {
                error: LensError {
                    vault: Address::random(),
                    query_failure_reason: Bytes::new(),
                },
                state: Box::from(LiquidationReasoning::Unknown),
            },
        ));
        assert!(!account.is_blacklisted());

        // ...then the blacklist status being set should nest inside it rather than being
        // discarded, and still be detected as blacklisted.
        account.set_status(LiquidationReasoning::Blacklisted(vec![Address::random()]));
        assert!(
            account.is_blacklisted(),
            "Blacklisted status nested inside a lens error should still be detected"
        );
    }

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
