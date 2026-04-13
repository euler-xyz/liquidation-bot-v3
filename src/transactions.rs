use alloy::{
    primitives::Address,
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
            // Build the transaction.
            let tx = liquidation.clone().into_transaction(profit_receiver);

            // Simulate the transaction.
            let simulation = provider.call(tx.clone()).await;

            if let Err(err) = simulation {
                error!("Error simulating liquidation, err: {}", err);
                continue;
            };

            // NOTE: We do not wait for any extra confirmations as there is essentially no risk
            // of a re-org.
            let tx = match provider.send_transaction(tx).await {
                Ok(tx) => tx.get_receipt().await,
                Err(err) => {
                    error!("Issue sending transaction, err: {}", err);
                    continue;
                }
            };

            match tx {
                Ok(receipt) => {
                    info!(
                        "Account {} liquidation succeeded, transaction hash {} included",
                        liquidation.account(),
                        receipt.transaction_hash
                    );
                }
                Err(err) => {
                    error!(
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
