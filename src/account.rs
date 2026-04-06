use alloy::{
    primitives::{Address, U256, address},
    providers::{DynProvider, Provider},
    rpc::types::{Filter, Log},
    sol,
    sol_types::SolEvent,
};
use anyhow::{Result, bail};
use std::sync::Arc;
use tokio::{sync::mpsc::Sender, time};
use tracing::{error, info};

use crate::{
    prices::Prices,
    types::{Account, OracleIdentifier, Vault},
};

#[derive(Debug, Clone)]
pub struct AccountSolvency {
    pub account: Address,
    pub asset_value: U256,
    pub debt_value: U256,
    pub accounted_in: Address,
}

sol! {
    /// @title Events
    /// @custom:security-contact security@euler.xyz
    /// @author Euler Labs (https://www.eulerlabs.com/)
    /// @notice This contract implements the events for the Ethereum Vault Connector.
    #[sol(rpc)]
    contract Events {
        /// @notice Emitted when an account status check is performed.
        /// @param account The account for which the status check is performed.
        /// @param controller The controller performing the status check.
        event AccountStatusCheck(address indexed account, address indexed controller);
    }

    #[sol(rpc)]
    interface ILiquidation {
        /// @notice Checks to see if a liquidation would be profitable, without actually doing anything
        /// @param liquidator Address that will initiate the liquidation
        /// @param violator Address that may be in collateral violation
        /// @param collateral Collateral which is to be seized
        /// @return maxRepay Max amount of debt that can be repaid, in asset units
        /// @return maxYield Yield in collateral corresponding to max allowed amount of debt to be repaid, in collateral
        /// balance (shares for vaults)
        function checkLiquidation(address liquidator, address violator, address collateral)
            external
            view
            returns (uint256 maxRepay, uint256 maxYield);
    }
}

/// Watches the chain for account update events
pub async fn watch_chain_for_accounts(
    provider: DynProvider,
    evc: Address,
    account_update_channel: Sender<Address>,
    mut from_block: u64,
) {
    loop {
        let latest = match provider.get_block_number().await {
            Ok(latest) => latest,
            Err(err) => {
                error!("Error while fetching the current block number: {err}");
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        if latest >= from_block {
            let filter = Filter::new()
                .address(evc)
                .from_block(from_block)
                .to_block(latest)
                .event_signature(Events::AccountStatusCheck::SIGNATURE_HASH);

            let logs: Vec<Log> = match provider.get_logs(&filter).await {
                Ok(logs) => logs,
                Err(err) => {
                    error!(
                        "Error while fetching logs from block range {}-{}: {err}",
                        from_block, latest
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

            for log in &logs {
                match Events::AccountStatusCheck::decode_log(&log.inner) {
                    Ok(decoded) => {
                        let block = log.block_number.unwrap_or_default();
                        info!(
                            "Found account event for {} at block {}",
                            decoded.account, block
                        );
                        account_update_channel.send(decoded.account).await.unwrap();
                    }
                    Err(e) => error!("Decode error: {e}"),
                }
            }

            // Advance past the range we just queried
            from_block = latest + 1;
        }

        // TODO: Make duration configurable, perhaps also an option to watch for new block events.
        time::sleep(tokio::time::Duration::from_secs(15)).await;
    }
}

pub async fn liquidate_account(provider: &DynProvider, account: Account) -> Result<()> {
    // Simulate the liquidation to calculate the potential profit.
    let vault = ILiquidation::new(account.debt.first().unwrap().vault.address, provider);

    // TODO: Currently hardcoded value for Mainnet!
    let liquidator_address = address!("0xAAF93d5475d092EA68a748137eE19D8130918392");

    for asset in account.assets.iter() {
        let result = vault
            .checkLiquidation(liquidator_address, account.address, asset.vault.address)
            .call()
            .await?;

        dbg!(asset.vault.asset, result.maxRepay, result.maxYield);
    }

    Ok(())
}

impl AccountSolvency {
    pub fn is_unhealthy(&self) -> bool {
        self.debt_value > self.asset_value
    }
}

impl Account {
    /// Get all the vaults this account has relations to.
    pub fn vaults(&self) -> Vec<Arc<Vault>> {
        let mut vaults: Vec<Arc<Vault>> = self.assets.iter().map(|a| a.vault.clone()).collect();
        vaults.extend(self.debt.iter().map(|d| d.vault.clone()));
        vaults
    }

    pub fn dependent_on(&self) -> Vec<OracleIdentifier> {
        let debt = match self.debt.first() {
            Some(debt) => debt,

            // If there is no debt then the account does not have a health score.
            None => return vec![],
        };

        // Add the asset oracles.
        let mut oracles: Vec<OracleIdentifier> = self
            .assets
            .iter()
            .map(|asset| OracleIdentifier {
                base_asset: asset.vault.asset,
                quote_asset: debt.vault.unit_of_account,
                adapter: debt.vault.adapter,
            })
            .collect();

        // Push the debt oracle.
        oracles.push(OracleIdentifier {
            base_asset: debt.vault.asset,
            quote_asset: debt.vault.unit_of_account,
            adapter: debt.vault.adapter,
        });

        oracles
    }

    pub fn calculate_health(&self, prices: &Prices) -> Result<AccountSolvency> {
        let debt = match self.debt.first() {
            Some(debt) => debt,
            None => bail!("An account with no debt does not have a health score."),
        };

        let debt_value = prices.get_quote(
            &OracleIdentifier {
                base_asset: debt.vault.asset,
                quote_asset: debt.vault.unit_of_account,
                adapter: debt.vault.adapter,
            },
            debt.amount,
        )?;

        // TODO: Incorporate the LiquidationLTV into the below calculation.
        let total_assets = self
            .assets
            .iter()
            .map(|a| {
                prices.get_quote(
                    &OracleIdentifier {
                        base_asset: a.vault.asset,
                        quote_asset: debt.vault.unit_of_account,
                        adapter: debt.vault.adapter,
                    },
                    a.amount,
                )
            })
            .collect::<Result<Vec<U256>>>()?
            .iter()
            .sum::<U256>();

        // Calculate the asset value.
        Ok(AccountSolvency {
            account: self.address,
            asset_value: total_assets,
            debt_value,
            accounted_in: debt.vault.unit_of_account,
        })
    }
}

#[cfg(test)]
mod test {
    use alloy::{
        primitives::{Address, address},
        providers::{Provider, ProviderBuilder},
    };

    use crate::{account::liquidate_account, lens::fetch_account, vaults::Vaults};

    const MAINNET_RPC_ENDPOINT: &str = "https://eth.rpc.blxrbdn.com";

    #[tokio::test]
    async fn test_liquidate_account() {
        let provider = ProviderBuilder::new()
            .connect_http(MAINNET_RPC_ENDPOINT.parse().unwrap())
            .erased();

        // Our singleton vault store.
        let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));

        let account = address!("0x819Ce254a22fF820765C85f07503F24268371E9e");

        // Fetch an account.
        let account = fetch_account(
            provider.clone(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            account,
        )
        .await
        .unwrap();

        liquidate_account(&provider, account).await.unwrap();
    }
}
