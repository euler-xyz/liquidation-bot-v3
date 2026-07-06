use alloy::{
    network::TransactionBuilder,
    primitives::{Address, U256},
    providers::{Provider, WalletProvider},
};
use tokio::sync::mpsc::Receiver;
use tracing::{error, info, warn};

use crate::liquidation::PreparedLiquidation;

/// Calculates the total cost of executing a liquidation transaction.
///
/// This is the gas cost (`gas_usage * gas_price`) plus any Pyth update fee that
/// has to be paid as `msg.value`. It is compared against the expected profit to
/// decide whether a liquidation is worth executing.
fn liquidation_cost(gas_usage: u64, gas_price: u128, pyth_cost: U256) -> U256 {
    U256::from(u128::from(gas_usage) * gas_price) + pyth_cost
}

/// Watches the liquidation channel and executes liquidations.
pub async fn execute_liquidation_queue<T: Provider + WalletProvider>(
    provider: T,
    mut queue: Receiver<PreparedLiquidation>,
    profit_receiver: Address,
) {
    loop {
        if let Some(liquidation) = queue.recv().await {
            info!(
                account =? liquidation.account(),
                "received request to liquidate account {}",
                liquidation.account()
            );

            // Build the transaction.
            let tx = liquidation.clone().into_transaction(profit_receiver);

            // Get the gas price for the liquidation.
            let gas_price = match provider.get_gas_price().await {
                Ok(price) => price,
                Err(err) => {
                    error!(
                        "Could not fetch gas price from the RPC, skipping liquidation, err: {:?}",
                        err
                    );
                    continue;
                }
            };

            // Estimate the gas, this also informs us on if its going to revert or not.
            let gas_usage = match provider.estimate_gas(tx.clone()).await {
                Ok(usage) => usage,
                Err(err) => {
                    error!(
                        account =? liquidation.account(),
                        "Error simulating liquidation, err: {:?}", err
                    );
                    continue;
                }
            };

            // Make sure this is profitable, if not then we do not execute.
            let cost = liquidation_cost(gas_usage, gas_price, liquidation.pyth_cost());
            if cost > liquidation.profit() {
                info!(
                    account =? liquidation.account(),
                    gas_price, gas_usage, cost =? cost, profit =? liquidation.profit(), profit_in_asset =? liquidation.profit_in_asset(),
                    "Transaction to liquidate {} is not profitable, skipping it.",
                    liquidation.account()
                );
                liquidation.set_account_status(crate::types::LiquidationReasoning::Unprofitable);
                continue;
            }

            info!(
                gas_price, gas_usage, cost =? cost, profit =? liquidation.profit(), profit_in_asset =? liquidation.profit_in_asset(),
                "Executing transaction to liquidate {}", liquidation.account()
            );

            // NOTE: For some reason alloy will use the estimated gas as the gas_limit. However
            // because EVC calls get quite a bit of a gas refund after using and then clearing
            // storage, the amount of gas that gets used is different than what the limit should be.
            //
            // Example: A transaction may only use 800k gas, but during execution it uses 1M gas and
            // then received a 200k refund. If we were to set a gas limit of 810k the transaction
            // would run out of gas.
            //
            // For this reason we use the gas estimation to see if a transaction would be
            // profitable, but we set the gas limit ourselves to be higher. To account for this
            // refund.

            // We add a 100% margin.
            let tx = tx.with_gas_limit(gas_usage * 2);

            // NOTE: We do not wait for any extra confirmations as there is essentially no risk
            // of a re-org.
            let tx = match provider.send_transaction(tx).await {
                Ok(tx) => tx.get_receipt().await,
                Err(err) => {
                    error!(
                        account =? liquidation.account(),
                        "Issue sending transaction, err: {:?}",
                        err
                    );
                    continue;
                }
            };

            match tx {
                Ok(receipt) => {
                    if receipt.status() {
                        info!(
                            account =? liquidation.account(),
                            "Account {} liquidation succeeded, transaction hash {} included",
                            liquidation.account(),
                            receipt.transaction_hash
                        );
                    } else {
                        warn!(
                            account =? liquidation.account(),
                            "Account {} liquidation reverted, transaction hash {}",
                            liquidation.account(),
                            receipt.transaction_hash
                        );
                    }
                }
                Err(err) => {
                    error!(
                        account =? liquidation.account(),
                        "Error while waiting for liquidation transaction receipt, err: {:?}",
                        err
                    );
                    continue;
                }
            };

            // We do not need to notify the main thread that this execution was a success, as our
            // liquidation transaction will cause a `AccountStatusCheck` event which cause the account watcher to sync to the new state.
        }
    }
}

#[cfg(test)]
mod test {
    use super::liquidation_cost;
    use alloy::primitives::U256;

    #[test]
    fn cost_is_gas_times_price() {
        // 21000 gas at 3 wei/gas = 63000, no pyth cost.
        assert_eq!(
            liquidation_cost(21_000, 3, U256::ZERO),
            U256::from(63_000u64)
        );
    }

    #[test]
    fn cost_includes_pyth_fee() {
        // The Pyth update fee is paid as msg.value and must be part of the cost.
        assert_eq!(
            liquidation_cost(21_000, 3, U256::from(1_000u64)),
            U256::from(64_000u64)
        );
    }

    // The execution gate skips a liquidation when `cost > profit`. These cases
    // pin down that boundary (break-even is executed, not skipped).
    fn should_skip(cost: U256, profit: U256) -> bool {
        cost > profit
    }

    #[test]
    fn unprofitable_liquidation_is_skipped() {
        let cost = liquidation_cost(21_000, 3, U256::ZERO); // 63000
        assert!(should_skip(cost, U256::from(62_999u64)));
    }

    #[test]
    fn break_even_liquidation_is_executed() {
        let cost = liquidation_cost(21_000, 3, U256::ZERO); // 63000
        assert!(!should_skip(cost, U256::from(63_000u64)));
    }

    #[test]
    fn profitable_liquidation_is_executed() {
        let cost = liquidation_cost(21_000, 3, U256::ZERO); // 63000
        assert!(!should_skip(cost, U256::from(100_000u64)));
    }
}
