use anyhow::Result;
use anyhow::anyhow;
use std::collections::HashMap;

use alloy::primitives::U256;

use crate::oracles::OracleChange;
use crate::types::OracleIdentifier;

#[derive(Clone, Debug)]
pub struct Prices {
    store: HashMap<OracleIdentifier, U256>,
}

impl Prices {
    pub fn new() -> Self {
        Prices {
            store: HashMap::new(),
        }
    }

    pub fn update(&mut self, oracle: OracleIdentifier, price: U256) {
        self.store.insert(oracle, price);
    }

    pub fn update_bulk(&mut self, updates: Vec<OracleChange>) {
        updates
            .iter()
            .for_each(|u| self.update(u.oracle.clone(), u.price));
    }

    /// Gets the price we have stored and uses it to calculate the value of `amount`
    pub fn get_quote(&self, oracle: &OracleIdentifier, amount: U256) -> Result<U256> {
        // Get the price for our fixed amount.
        let price = self
            .store
            .get(oracle)
            .ok_or(anyhow!("Oracle not found in price set."))?;

        Ok((amount * price).div_ceil(U256::from(100_000)))
    }
}
