use alloy::{
    primitives::{Address, U256},
    providers::{Provider, WalletProvider},
};
use tokio::sync::mpsc::Receiver;
use tracing::{error, info};

use crate::liquidation::PreparedLiquidation;

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
                        "Could not fetch gas price from the RPC, skipping liquidation, err: {}",
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
                        "Error simulating liquidation, err: {}", err
                    );
                    continue;
                }
            };

            // Make sure this is profitable, if not then we do not execute.
            let cost = U256::from(u128::from(gas_usage) * gas_price) + liquidation.pyth_cost();
            if cost > liquidation.profit() {
                info!(
                    account =? liquidation.account(),
                    gas_price, gas_usage, cost =? cost, profit =? liquidation.profit(),
                    "Transaction to liquidate {} is not profitable, skipping it.",
                    liquidation.account()
                );
                continue;
            }

            info!(
                gas_price, gas_usage, cost =? cost, profit =? liquidation.profit(), profit_in_asset =? liquidation.profit_in_asset(),
                "Executing transaction to liquidate {}", liquidation.account()
            );

            // NOTE: We do not wait for any extra confirmations as there is essentially no risk
            // of a re-org.
            let tx = match provider.send_transaction(tx).await {
                Ok(tx) => tx.get_receipt().await,
                Err(err) => {
                    error!(
                        account =? liquidation.account(),
                        "Issue sending transaction, err: {}",
                        err
                    );
                    continue;
                }
            };

            match tx {
                Ok(receipt) => {
                    info!(
                        account =? liquidation.account(),
                        "Account {} liquidation succeeded, transaction hash {} included",
                        liquidation.account(),
                        receipt.transaction_hash
                    );
                }
                Err(err) => {
                    error!(
                        account =? liquidation.account(),
                        "Error while waiting for liquidation transaction receipt, err: {}",
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
