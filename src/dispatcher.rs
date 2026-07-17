//! Transaction dispatching with explicit nonce management and fee bumping.
//!
//! The dispatcher owns everything between "here is a ready transaction and a cost budget" and
//! "it was mined / we gave up":
//!
//! 1. It manages the nonce locally (bypassing alloy's `CachedNonceManager` by always setting the
//!    nonce and fees explicitly on the request, which makes the fillers skip those fields).
//! 2. If a transaction is not included within `inclusion_timeout` it re-sends it with the same
//!    nonce and bumped fees, up to `max_bumps` times.
//! 3. If it still is not included the transaction is abandoned: the nonce is *kept* so the next
//!    dispatch replaces the stuck transaction instead of queueing behind it, and the last sent
//!    fees are recorded as a floor (nodes only accept same-nonce replacements that bump fees by
//!    ~10-12.5%).
//! 4. A bump is refused when it would push the total cost over the dispatch budget, so we never
//!    knowingly pay more for a transaction than it earns us.
//! 5. On unclassified RPC send errors the nonce is re-synced from the chain as a fallback, so a
//!    wrong local nonce (e.g. we believed a transaction was not included but it was) heals
//!    instead of wedging every subsequent transaction.

use alloy::{
    network::TransactionBuilder,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::{TransactionReceipt, TransactionRequest},
};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Configuration for the dispatch flow.
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    /// How long we wait for a transaction to be included before bumping its fees.
    pub inclusion_timeout: Duration,
    /// How many times we re-send with bumped fees before abandoning the transaction.
    pub max_bumps: usize,
    /// Percentage by which fees are raised over the previously sent fees on each bump. Must be
    /// comfortably above the node replacement minimum (10% on geth, 12.5% priority on some
    /// others), otherwise replacements get rejected as underpriced.
    pub bump_percent: u128,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            inclusion_timeout: Duration::from_secs(30),
            max_bumps: 2,
            bump_percent: 25,
        }
    }
}

/// The gas pricing of a single transaction attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fees {
    Eip1559 {
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    },
    Legacy {
        gas_price: u128,
    },
}

impl Fees {
    /// The most this fee will pay per unit of gas.
    pub fn cap(&self) -> u128 {
        match self {
            Fees::Eip1559 {
                max_fee_per_gas, ..
            } => *max_fee_per_gas,
            Fees::Legacy { gas_price } => *gas_price,
        }
    }

    /// The (fee cap, priority fee) pair. For legacy transactions the whole gas price acts as
    /// both.
    fn components(&self) -> (u128, u128) {
        match self {
            Fees::Eip1559 {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            } => (*max_fee_per_gas, *max_priority_fee_per_gas),
            Fees::Legacy { gas_price } => (*gas_price, *gas_price),
        }
    }

    /// Raises both components by `percent`.
    fn bumped(&self, percent: u128) -> Self {
        let bump = |v: u128| v.saturating_mul(100 + percent) / 100;
        match self {
            Fees::Eip1559 {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            } => Fees::Eip1559 {
                max_fee_per_gas: bump(*max_fee_per_gas),
                max_priority_fee_per_gas: bump(*max_priority_fee_per_gas),
            },
            Fees::Legacy { gas_price } => Fees::Legacy {
                gas_price: bump(*gas_price),
            },
        }
    }

    /// Sets these fees on a transaction request.
    fn apply(&self, tx: TransactionRequest) -> TransactionRequest {
        match self {
            Fees::Eip1559 {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            } => tx
                .with_max_fee_per_gas(*max_fee_per_gas)
                .with_max_priority_fee_per_gas(*max_priority_fee_per_gas),
            Fees::Legacy { gas_price } => tx.with_gas_price(*gas_price),
        }
    }
}

/// Picks the fees for the next attempt at a nonce: the fresh network estimate, but never below
/// the previously sent fees bumped by `bump_percent` (the node would reject the replacement as
/// underpriced otherwise). The result takes the shape (EIP-1559/legacy) of the fresh estimate.
pub fn next_fees(fresh: Fees, previous: Option<Fees>, bump_percent: u128) -> Fees {
    let Some(previous) = previous else {
        return fresh;
    };

    let (fresh_cap, fresh_priority) = fresh.components();
    let (bumped_cap, bumped_priority) = previous.bumped(bump_percent).components();

    match fresh {
        Fees::Eip1559 { .. } => Fees::Eip1559 {
            max_fee_per_gas: fresh_cap.max(bumped_cap),
            max_priority_fee_per_gas: fresh_priority.max(bumped_priority),
        },
        Fees::Legacy { .. } => Fees::Legacy {
            gas_price: fresh_cap.max(bumped_cap),
        },
    }
}

/// The maximum total cost of an attempt: worst-case gas cost plus the value sent along (for us
/// that is the Pyth update fee).
pub fn attempt_cost(expected_gas: u64, fees: &Fees, value: U256) -> U256 {
    U256::from(fees.cap())
        .saturating_mul(U256::from(expected_gas))
        .saturating_add(value)
}

/// Classification of an `eth_sendTransaction`/`eth_sendRawTransaction` error, based on the error
/// message since nodes do not report these as structured errors consistently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendErrorKind {
    /// Something already mined at this nonce; our local nonce is behind the chain.
    NonceTooLow,
    /// A same-nonce transaction sits in the mempool with fees too close to ours.
    ReplacementUnderpriced,
    /// This exact transaction is already in the mempool.
    AlreadyKnown,
    /// Anything else (transport errors, out of funds, ...).
    Other,
}

/// Classifies a send error message. Covers the phrasings used by geth, reth and anvil.
pub fn classify_send_error(message: &str) -> SendErrorKind {
    let message = message.to_lowercase();

    if message.contains("nonce too low") || message.contains("nonce is too low") {
        SendErrorKind::NonceTooLow
    } else if message.contains("replacement transaction underpriced")
        || message.contains("replacement underpriced")
        || message.contains("insufficient gas price to replace")
    {
        SendErrorKind::ReplacementUnderpriced
    } else if message.contains("already known")
        || message.contains("already imported")
        || message.contains("already in the pool")
        || message.contains("duplicate transaction")
    {
        SendErrorKind::AlreadyKnown
    } else {
        SendErrorKind::Other
    }
}

/// The result of dispatching a transaction.
#[derive(Debug)]
pub enum DispatchOutcome {
    /// The transaction was mined. Note that it may still have reverted, check
    /// `receipt.status()`.
    Included(TransactionReceipt),
    /// The transaction was not included after all fee bumps. It is left in the mempool; the
    /// nonce is kept so the next dispatch replaces it (paying at least the recorded fee floor).
    Abandoned,
    /// The dispatch failed without (known) inclusion. Nonce state has been re-synced where
    /// appropriate, so the next dispatch starts from a clean slate.
    Failed(String),
}

/// Outcome of checking whether a nonce was consumed on chain.
enum ConsumedCheck {
    /// One of our own attempts mined.
    Included(TransactionReceipt),
    /// The nonce was consumed, but by a transaction we did not send in this dispatch. Carries
    /// the current chain nonce.
    ConsumedUnknown(u64),
    /// The nonce is still unused on chain (as far as `latest` is concerned).
    NotConsumed,
}

/// Sends transactions strictly serially with explicit nonce management and fee bumping. See the
/// module documentation for the full flow.
///
/// Owned by a single task; one transaction is in flight at a time.
pub struct Dispatcher<P> {
    provider: P,
    sender: Address,
    config: DispatchConfig,

    /// The nonce the next transaction will use. `None` until first synced from the chain.
    next_nonce: Option<u64>,
    /// The fees of the last transaction we sent at `next_nonce` and then abandoned. The next
    /// dispatch must outbid this to replace it in the mempool.
    fee_floor: Option<Fees>,
}

impl<P: Provider> Dispatcher<P> {
    pub fn new(provider: P, sender: Address, config: DispatchConfig) -> Self {
        Self {
            provider,
            sender,
            config,
            next_nonce: None,
            fee_floor: None,
        }
    }

    /// The underlying provider, for callers that need reads (gas estimation etc).
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Re-syncs the local nonce from the chain (`pending` tag). Clears the fee floor if the
    /// nonce moved, as the floor belonged to the old nonce.
    async fn resync_nonce(&mut self) -> Result<u64, String> {
        let nonce = self
            .provider
            .get_transaction_count(self.sender)
            .pending()
            .await
            .map_err(|e| format!("could not fetch the account nonce: {e}"))?;

        if self.next_nonce != Some(nonce) {
            self.fee_floor = None;
        }
        self.next_nonce = Some(nonce);

        Ok(nonce)
    }

    /// Estimates fresh network fees, preferring EIP-1559 and falling back to a legacy gas price
    /// on chains that do not support it.
    async fn estimate_fees(&self) -> Result<Fees, String> {
        match self.provider.estimate_eip1559_fees().await {
            Ok(estimate) => Ok(Fees::Eip1559 {
                max_fee_per_gas: estimate.max_fee_per_gas,
                max_priority_fee_per_gas: estimate.max_priority_fee_per_gas,
            }),
            Err(eip1559_err) => match self.provider.get_gas_price().await {
                Ok(gas_price) => Ok(Fees::Legacy { gas_price }),
                Err(legacy_err) => Err(format!(
                    "could not estimate fees, eip1559 err: {eip1559_err}, legacy err: {legacy_err}"
                )),
            },
        }
    }

    /// Checks whether `nonce` has been consumed on chain, and if so whether it was by one of the
    /// hashes we sent.
    async fn check_consumed(&self, nonce: u64, sent_hashes: &[B256]) -> ConsumedCheck {
        let chain_nonce = match self
            .provider
            .get_transaction_count(self.sender)
            .latest()
            .await
        {
            Ok(count) => count,
            Err(e) => {
                warn!("Could not check the chain nonce, assuming our transaction is still pending, err: {e}");
                return ConsumedCheck::NotConsumed;
            }
        };

        if chain_nonce <= nonce {
            return ConsumedCheck::NotConsumed;
        }

        for hash in sent_hashes {
            if let Ok(Some(receipt)) = self.provider.get_transaction_receipt(*hash).await {
                return ConsumedCheck::Included(receipt);
            }
        }

        ConsumedCheck::ConsumedUnknown(chain_nonce)
    }

    /// Marks the dispatch as successfully mined at `nonce`.
    fn complete(&mut self, nonce: u64, receipt: TransactionReceipt) -> DispatchOutcome {
        self.next_nonce = Some(nonce + 1);
        self.fee_floor = None;
        DispatchOutcome::Included(receipt)
    }

    /// Gives up on the transaction at `nonce`, keeping the nonce and recording the fee floor so
    /// the next dispatch replaces the stuck transaction.
    fn abandon(&mut self, nonce: u64, last_fees: Option<Fees>) -> DispatchOutcome {
        self.next_nonce = Some(nonce);
        self.fee_floor = last_fees;
        DispatchOutcome::Abandoned
    }

    /// Dispatches a transaction: sends it, waits for inclusion, bumps fees on timeout, and
    /// abandons it once bumps are exhausted or the budget is hit.
    ///
    /// `expected_gas` is the gas the transaction is expected to actually use (not the padded gas
    /// limit) and is used together with `max_cost` to refuse attempts that would cost more than
    /// the transaction earns.
    pub async fn dispatch(
        &mut self,
        tx: TransactionRequest,
        expected_gas: u64,
        max_cost: U256,
    ) -> DispatchOutcome {
        let value = tx.value.unwrap_or_default();

        let nonce = match self.next_nonce {
            Some(nonce) => nonce,
            None => match self.resync_nonce().await {
                Ok(nonce) => nonce,
                Err(e) => return DispatchOutcome::Failed(e),
            },
        };
        let mut nonce = nonce;

        // Fees of the last transaction sent at this nonce (by this dispatch, or the floor left
        // behind by a previously abandoned one). The next attempt must outbid them.
        let mut last_fees = self.fee_floor;
        // Every hash we have sent for this nonce; any of them could be the one that mines.
        let mut sent_hashes: Vec<B256> = Vec::new();
        // We allow a single nonce re-sync per dispatch to avoid looping forever when the chain
        // keeps disagreeing with us.
        let mut resynced = false;

        let mut attempt = 0;
        while attempt <= self.config.max_bumps {
            let fresh = match self.estimate_fees().await {
                Ok(fees) => fees,
                Err(e) => return DispatchOutcome::Failed(e),
            };
            let fees = next_fees(fresh, last_fees, self.config.bump_percent);

            // Refuse attempts that would cost more than this transaction is worth to us.
            let cost = attempt_cost(expected_gas, &fees, value);
            if cost > max_cost {
                return if sent_hashes.is_empty() {
                    DispatchOutcome::Failed(format!(
                        "sending at nonce {nonce} would cost {cost} which exceeds the budget of {max_cost}, not sending"
                    ))
                } else {
                    warn!(
                        nonce,
                        "Bumping the fees again would cost {cost}, exceeding the budget of {max_cost}. Abandoning the transaction."
                    );
                    self.abandon(nonce, last_fees)
                };
            }

            let request = fees.apply(tx.clone().with_from(self.sender).with_nonce(nonce));

            info!(
                nonce,
                attempt,
                fee_cap = fees.cap(),
                "Sending transaction (attempt {} of {})",
                attempt + 1,
                self.config.max_bumps + 1
            );

            match self.provider.send_transaction(request).await {
                Ok(pending) => {
                    let hash = *pending.tx_hash();
                    sent_hashes.push(hash);
                    last_fees = Some(fees);

                    // Persist state immediately so it is correct however this dispatch exits.
                    self.next_nonce = Some(nonce);
                    self.fee_floor = Some(fees);

                    match tokio::time::timeout(self.config.inclusion_timeout, pending.get_receipt())
                        .await
                    {
                        Ok(Ok(receipt)) => return self.complete(nonce, receipt),
                        Ok(Err(e)) => {
                            warn!(
                                nonce,
                                "Error while watching transaction {hash} for inclusion, treating it as not included, err: {e}"
                            );
                        }
                        Err(_) => {
                            debug!(
                                nonce,
                                "Transaction {hash} was not included within {:?}",
                                self.config.inclusion_timeout
                            );
                        }
                    }

                    // Not seen via the watcher; check whether the nonce was consumed anyway
                    // (e.g. an earlier attempt of ours mined).
                    match self.check_consumed(nonce, &sent_hashes).await {
                        ConsumedCheck::Included(receipt) => return self.complete(nonce, receipt),
                        ConsumedCheck::ConsumedUnknown(chain_nonce) => {
                            self.next_nonce = Some(chain_nonce);
                            self.fee_floor = None;
                            return DispatchOutcome::Failed(format!(
                                "nonce {nonce} was consumed by a transaction we did not send in this dispatch, local nonce re-synced to {chain_nonce}"
                            ));
                        }
                        ConsumedCheck::NotConsumed => {}
                    }

                    attempt += 1;
                }
                Err(err) => {
                    let message = err.to_string();
                    match classify_send_error(&message) {
                        SendErrorKind::NonceTooLow => {
                            // Something already mined at this nonce (an abandoned transaction
                            // from earlier, or a manual transaction from the same account).
                            if resynced {
                                return DispatchOutcome::Failed(format!(
                                    "nonce still too low after re-syncing from the chain: {message}"
                                ));
                            }
                            resynced = true;

                            match self.resync_nonce().await {
                                Ok(new_nonce) => {
                                    info!(
                                        old_nonce = nonce,
                                        new_nonce, "Nonce was too low, re-synced from the chain."
                                    );
                                    nonce = new_nonce;
                                    last_fees = self.fee_floor;
                                    sent_hashes.clear();
                                    // Does not consume a bump attempt.
                                }
                                Err(e) => return DispatchOutcome::Failed(e),
                            }
                        }
                        SendErrorKind::ReplacementUnderpriced => {
                            // The mempool holds a same-nonce transaction priced higher than our
                            // recorded floor. Treat what we just tried as the new floor and bump
                            // from there.
                            warn!(
                                nonce,
                                "Replacement was underpriced, raising the fee floor and bumping, err: {message}"
                            );
                            last_fees = Some(fees);
                            self.fee_floor = Some(fees);
                            attempt += 1;
                        }
                        SendErrorKind::AlreadyKnown => {
                            // This exact transaction is already in the mempool; wait for it as
                            // if we had just sent it.
                            info!(
                                nonce,
                                "Transaction is already in the mempool, waiting for its inclusion."
                            );
                            last_fees = Some(fees);
                            self.next_nonce = Some(nonce);
                            self.fee_floor = Some(fees);

                            tokio::time::sleep(self.config.inclusion_timeout).await;

                            match self.check_consumed(nonce, &sent_hashes).await {
                                ConsumedCheck::Included(receipt) => {
                                    return self.complete(nonce, receipt);
                                }
                                ConsumedCheck::ConsumedUnknown(chain_nonce) => {
                                    // We did not track the hash (the send errored), so a mined
                                    // transaction at this nonce cannot be attributed. Treat the
                                    // nonce as consumed and move on.
                                    self.next_nonce = Some(chain_nonce);
                                    self.fee_floor = None;
                                    return DispatchOutcome::Failed(format!(
                                        "nonce {nonce} was consumed while waiting on an already-known transaction, local nonce re-synced to {chain_nonce}"
                                    ));
                                }
                                ConsumedCheck::NotConsumed => {}
                            }

                            attempt += 1;
                        }
                        SendErrorKind::Other => {
                            // Unknown failure. Re-sync the nonce from the chain as a fallback so
                            // a wrong local nonce cannot wedge every subsequent dispatch.
                            error!(
                                nonce,
                                "Sending the transaction failed, re-syncing the nonce as a precaution, err: {message}"
                            );
                            if let Err(e) = self.resync_nonce().await {
                                warn!("Nonce re-sync after a failed send also failed, err: {e}");
                            }
                            return DispatchOutcome::Failed(message);
                        }
                    }
                }
            }
        }

        warn!(
            nonce,
            "Transaction was not included after {} attempts, abandoning it. The next dispatch will replace it at the same nonce.",
            self.config.max_bumps + 1
        );
        self.abandon(nonce, last_fees)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::ProviderBuilder;

    const GWEI: u128 = 1_000_000_000;

    // --- next_fees -------------------------------------------------------------------------

    #[test]
    fn first_attempt_uses_fresh_estimate() {
        let fresh = Fees::Eip1559 {
            max_fee_per_gas: 100 * GWEI,
            max_priority_fee_per_gas: 2 * GWEI,
        };
        assert_eq!(next_fees(fresh, None, 25), fresh);
    }

    #[test]
    fn bump_beats_stale_estimate() {
        // The market did not move; the bump over the previous fees must win, otherwise the node
        // rejects the replacement as underpriced.
        let fresh = Fees::Eip1559 {
            max_fee_per_gas: 100 * GWEI,
            max_priority_fee_per_gas: 2 * GWEI,
        };
        let previous = Fees::Eip1559 {
            max_fee_per_gas: 100 * GWEI,
            max_priority_fee_per_gas: 2 * GWEI,
        };

        assert_eq!(
            next_fees(fresh, Some(previous), 25),
            Fees::Eip1559 {
                max_fee_per_gas: 125 * GWEI,
                max_priority_fee_per_gas: 2 * GWEI * 125 / 100,
            }
        );
    }

    #[test]
    fn fresh_spike_beats_bump() {
        // Gas spiked way past our bump; we should pay the market rate or we will never get in.
        let fresh = Fees::Eip1559 {
            max_fee_per_gas: 1000 * GWEI,
            max_priority_fee_per_gas: 50 * GWEI,
        };
        let previous = Fees::Eip1559 {
            max_fee_per_gas: 100 * GWEI,
            max_priority_fee_per_gas: 2 * GWEI,
        };

        assert_eq!(next_fees(fresh, Some(previous), 25), fresh);
    }

    #[test]
    fn components_are_maxed_independently() {
        // Cap comes from the fresh estimate, priority from the bump.
        let fresh = Fees::Eip1559 {
            max_fee_per_gas: 300 * GWEI,
            max_priority_fee_per_gas: 1 * GWEI,
        };
        let previous = Fees::Eip1559 {
            max_fee_per_gas: 200 * GWEI,
            max_priority_fee_per_gas: 10 * GWEI,
        };

        assert_eq!(
            next_fees(fresh, Some(previous), 25),
            Fees::Eip1559 {
                max_fee_per_gas: 300 * GWEI,
                max_priority_fee_per_gas: 10 * GWEI * 125 / 100,
            }
        );
    }

    #[test]
    fn legacy_fees_bump() {
        let fresh = Fees::Legacy {
            gas_price: 100 * GWEI,
        };
        let previous = Fees::Legacy {
            gas_price: 200 * GWEI,
        };

        assert_eq!(
            next_fees(fresh, Some(previous), 25),
            Fees::Legacy {
                gas_price: 250 * GWEI
            }
        );
    }

    #[test]
    fn result_takes_shape_of_fresh_estimate() {
        // A legacy floor carried into an EIP-1559 estimate: the result is EIP-1559 and outbids
        // the floor in both components.
        let fresh = Fees::Eip1559 {
            max_fee_per_gas: 100 * GWEI,
            max_priority_fee_per_gas: 2 * GWEI,
        };
        let previous = Fees::Legacy {
            gas_price: 200 * GWEI,
        };

        assert_eq!(
            next_fees(fresh, Some(previous), 25),
            Fees::Eip1559 {
                max_fee_per_gas: 250 * GWEI,
                max_priority_fee_per_gas: 250 * GWEI,
            }
        );
    }

    #[test]
    fn bump_saturates_instead_of_overflowing() {
        let previous = Fees::Legacy { gas_price: u128::MAX };
        let fresh = Fees::Legacy { gas_price: GWEI };
        // Must not panic.
        assert_eq!(
            next_fees(fresh, Some(previous), 25),
            Fees::Legacy {
                gas_price: u128::MAX / 100
            }
        );
    }

    // --- attempt_cost ----------------------------------------------------------------------

    #[test]
    fn cost_is_gas_times_cap_plus_value() {
        let fees = Fees::Eip1559 {
            max_fee_per_gas: 3,
            max_priority_fee_per_gas: 1,
        };
        assert_eq!(
            attempt_cost(21_000, &fees, U256::from(1_000u64)),
            U256::from(64_000u64)
        );
    }

    // --- classify_send_error ---------------------------------------------------------------

    #[test]
    fn classifies_nonce_too_low() {
        // geth / anvil / reth phrasings.
        assert_eq!(classify_send_error("nonce too low"), SendErrorKind::NonceTooLow);
        assert_eq!(
            classify_send_error("Nonce too low. Expected nonce to be 5 but got 3."),
            SendErrorKind::NonceTooLow
        );
    }

    #[test]
    fn classifies_replacement_underpriced() {
        assert_eq!(
            classify_send_error("replacement transaction underpriced"),
            SendErrorKind::ReplacementUnderpriced
        );
        assert_eq!(
            classify_send_error("insufficient gas price to replace existing transaction"),
            SendErrorKind::ReplacementUnderpriced
        );
    }

    #[test]
    fn classifies_already_known() {
        assert_eq!(classify_send_error("already known"), SendErrorKind::AlreadyKnown);
        assert_eq!(
            classify_send_error("transaction already imported"),
            SendErrorKind::AlreadyKnown
        );
    }

    #[test]
    fn classifies_unknown_errors_as_other() {
        assert_eq!(
            classify_send_error("insufficient funds for gas * price + value"),
            SendErrorKind::Other
        );
        assert_eq!(classify_send_error("connection refused"), SendErrorKind::Other);
    }

    // --- nonce state, with a mocked provider -------------------------------------------------

    #[tokio::test]
    async fn mocked_nonce_is_synced_from_chain_and_clears_stale_floor() {
        let asserter = alloy::transports::mock::Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());

        let mut dispatcher = Dispatcher::new(
            provider,
            Address::ZERO,
            DispatchConfig::default(),
        );

        // Initial sync.
        asserter.push_success(&"0x5");
        assert_eq!(dispatcher.resync_nonce().await.unwrap(), 5);
        assert_eq!(dispatcher.next_nonce, Some(5));

        // A floor recorded for nonce 5 survives a re-sync that lands on the same nonce...
        dispatcher.fee_floor = Some(Fees::Legacy { gas_price: GWEI });
        asserter.push_success(&"0x5");
        assert_eq!(dispatcher.resync_nonce().await.unwrap(), 5);
        assert!(dispatcher.fee_floor.is_some());

        // ...but is cleared when the chain reports a different nonce, as the floor belonged to
        // the old nonce.
        asserter.push_success(&"0x7");
        assert_eq!(dispatcher.resync_nonce().await.unwrap(), 7);
        assert_eq!(dispatcher.next_nonce, Some(7));
        assert!(dispatcher.fee_floor.is_none());
    }
}

/// Tests of the full dispatch flow against a local Anvil node. Auto-mining is toggled off to
/// simulate transactions genuinely stuck in the mempool, and blocks are mined on demand to
/// control exactly when inclusion happens.
#[cfg(test)]
mod anvil_tests {
    use super::*;
    use alloy::{
        consensus::Transaction,
        node_bindings::{Anvil, AnvilInstance},
        providers::{ProviderBuilder, ext::AnvilApi},
    };

    /// A short timeout config so the tests run fast: 500ms inclusion wait, 2 bumps, +25%.
    fn fast_config() -> DispatchConfig {
        DispatchConfig {
            inclusion_timeout: Duration::from_millis(500),
            max_bumps: 2,
            bump_percent: 25,
        }
    }

    /// Spawns a fresh local Anvil node and returns it together with a wallet-enabled provider
    /// and the sender address (the first dev account).
    fn setup() -> (AnvilInstance, impl Provider + Clone, Address) {
        let anvil = Anvil::new().try_spawn().unwrap();
        let sender = anvil.addresses()[0];
        let provider = ProviderBuilder::new()
            .wallet(anvil.wallet().unwrap())
            .connect_http(anvil.endpoint_url());

        (anvil, provider, sender)
    }

    /// A minimal ETH transfer with the gas limit pre-set, mirroring how the executor hands the
    /// dispatcher a fully-built transaction.
    fn transfer(to: Address) -> TransactionRequest {
        TransactionRequest::default()
            .with_to(to)
            .with_value(U256::from(1))
            .with_gas_limit(21_000)
    }

    /// Mines a block every `every` until aborted. Used to include whatever the dispatcher has
    /// pending without racing a single fixed-time mine against its retry windows.
    fn spawn_miner(
        provider: impl Provider + Clone + 'static,
        initial_delay: Duration,
        every: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tokio::time::sleep(initial_delay).await;
            loop {
                let _ = provider.anvil_mine(Some(1), None).await;
                tokio::time::sleep(every).await;
            }
        })
    }

    #[tokio::test]
    async fn happy_path_increments_nonce() {
        let (_anvil, provider, sender) = setup();
        let recipient = _anvil.addresses()[1];
        let mut dispatcher = Dispatcher::new(provider.clone(), sender, fast_config());

        // Two sequential dispatches, both should be included by auto-mining.
        for _ in 0..2 {
            let outcome = dispatcher.dispatch(transfer(recipient), 21_000, U256::MAX).await;
            let DispatchOutcome::Included(receipt) = outcome else {
                panic!("expected inclusion, got {outcome:?}");
            };
            assert!(receipt.status());
        }

        // Both consumed a nonce on chain and the dispatcher agrees on what comes next.
        let chain_nonce = provider.get_transaction_count(sender).latest().await.unwrap();
        assert_eq!(chain_nonce, 2);
        assert_eq!(dispatcher.next_nonce, Some(2));
        assert!(dispatcher.fee_floor.is_none());
    }

    #[tokio::test]
    async fn stuck_transaction_is_bumped_then_included() {
        let (_anvil, provider, sender) = setup();
        let recipient = _anvil.addresses()[1];
        let mut dispatcher = Dispatcher::new(provider.clone(), sender, fast_config());

        provider.anvil_set_auto_mine(false).await.unwrap();

        // The fee estimate the first attempt will use (stable: no blocks are being mined).
        let initial = provider.estimate_eip1559_fees().await.unwrap();

        // Only start mining after the first attempt's inclusion window has passed, so the
        // transaction that eventually mines must be a bumped replacement.
        let miner = spawn_miner(
            provider.clone(),
            Duration::from_millis(1200),
            Duration::from_millis(300),
        );

        let outcome = dispatcher.dispatch(transfer(recipient), 21_000, U256::MAX).await;
        miner.abort();

        let DispatchOutcome::Included(receipt) = outcome else {
            panic!("expected inclusion after bumping, got {outcome:?}");
        };
        assert!(receipt.status());

        // The mined transaction must carry bumped fees (>= +25% over the initial estimate),
        // proving a replacement mined rather than the original.
        let mined = provider
            .get_transaction_by_hash(receipt.transaction_hash)
            .await
            .unwrap()
            .unwrap();
        assert!(
            mined.max_fee_per_gas() >= initial.max_fee_per_gas * 125 / 100,
            "expected bumped fees, initial cap {} vs mined cap {}",
            initial.max_fee_per_gas,
            mined.max_fee_per_gas()
        );
        assert_eq!(mined.nonce(), 0);
    }

    #[tokio::test]
    async fn abandoned_after_max_bumps_keeps_nonce_and_floor() {
        let (_anvil, provider, sender) = setup();
        let recipient = _anvil.addresses()[1];
        let mut dispatcher = Dispatcher::new(provider.clone(), sender, fast_config());

        provider.anvil_set_auto_mine(false).await.unwrap();
        let initial = provider.estimate_eip1559_fees().await.unwrap();

        // Nothing ever mines: initial send + 2 bumps, then the transaction is abandoned.
        let outcome = dispatcher.dispatch(transfer(recipient), 21_000, U256::MAX).await;
        assert!(matches!(outcome, DispatchOutcome::Abandoned), "got {outcome:?}");

        // The nonce is kept for the next dispatch to replace the stuck transaction, and the
        // floor reflects two bumps over the initial estimate (1.25^2 = ~1.56x).
        assert_eq!(dispatcher.next_nonce, Some(0));
        let floor = dispatcher.fee_floor.expect("floor should be recorded");
        assert!(
            floor.cap() > initial.max_fee_per_gas * 150 / 100,
            "expected a twice-bumped floor, initial cap {} vs floor cap {}",
            initial.max_fee_per_gas,
            floor.cap()
        );
    }

    #[tokio::test]
    async fn next_dispatch_replaces_abandoned_transaction() {
        let (_anvil, provider, sender) = setup();
        let recipient_a = _anvil.addresses()[1];
        let recipient_b = _anvil.addresses()[2];
        let mut dispatcher = Dispatcher::new(provider.clone(), sender, fast_config());

        provider.anvil_set_auto_mine(false).await.unwrap();

        // Transaction A gets stuck and is abandoned at nonce 0.
        let outcome_a = dispatcher.dispatch(transfer(recipient_a), 21_000, U256::MAX).await;
        assert!(matches!(outcome_a, DispatchOutcome::Abandoned), "got {outcome_a:?}");

        // Transaction B is dispatched while blocks are being mined. It must NOT queue behind A:
        // it replaces A at the same nonce and gets included.
        let miner = spawn_miner(
            provider.clone(),
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        let outcome_b = dispatcher.dispatch(transfer(recipient_b), 21_000, U256::MAX).await;
        miner.abort();

        let DispatchOutcome::Included(receipt) = outcome_b else {
            panic!("expected B to be included, got {outcome_b:?}");
        };
        assert!(receipt.status());

        // B mined at the nonce A was stuck on, and A itself never mined.
        let mined = provider
            .get_transaction_by_hash(receipt.transaction_hash)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mined.nonce(), 0, "B should replace A at nonce 0");
        assert_eq!(receipt.to, Some(recipient_b), "the mined transaction should be B");
        let chain_nonce = provider.get_transaction_count(sender).latest().await.unwrap();
        assert_eq!(chain_nonce, 1, "exactly one transaction should have mined");
    }

    #[tokio::test]
    async fn nonce_too_low_triggers_resync() {
        let (_anvil, provider, sender) = setup();
        let recipient_a = _anvil.addresses()[1];
        let recipient_b = _anvil.addresses()[2];
        let mut dispatcher = Dispatcher::new(provider.clone(), sender, fast_config());

        provider.anvil_set_auto_mine(false).await.unwrap();

        // Abandon A at nonce 0...
        let outcome_a = dispatcher.dispatch(transfer(recipient_a), 21_000, U256::MAX).await;
        assert!(matches!(outcome_a, DispatchOutcome::Abandoned), "got {outcome_a:?}");

        // ...then A mines after all, consuming nonce 0 while the dispatcher still plans to
        // reuse it.
        provider.anvil_mine(Some(1), None).await.unwrap();
        let chain_nonce = provider.get_transaction_count(sender).latest().await.unwrap();
        assert_eq!(chain_nonce, 1, "the abandoned transaction should have mined");

        // Dispatching B first hits `nonce too low`, re-syncs, and lands at nonce 1.
        provider.anvil_set_auto_mine(true).await.unwrap();
        let outcome_b = dispatcher.dispatch(transfer(recipient_b), 21_000, U256::MAX).await;

        let DispatchOutcome::Included(receipt) = outcome_b else {
            panic!("expected B to be included after the nonce re-sync, got {outcome_b:?}");
        };
        assert!(receipt.status());

        let mined = provider
            .get_transaction_by_hash(receipt.transaction_hash)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mined.nonce(), 1, "B should have re-synced to nonce 1");
        assert_eq!(dispatcher.next_nonce, Some(2));
    }

    #[tokio::test]
    async fn budget_refuses_first_send() {
        let (_anvil, provider, sender) = setup();
        let recipient = _anvil.addresses()[1];
        let mut dispatcher = Dispatcher::new(provider.clone(), sender, fast_config());

        // A budget of 1 wei cannot cover any send; nothing should reach the chain.
        let outcome = dispatcher
            .dispatch(transfer(recipient), 21_000, U256::from(1))
            .await;
        assert!(matches!(outcome, DispatchOutcome::Failed(_)), "got {outcome:?}");

        let chain_nonce = provider.get_transaction_count(sender).pending().await.unwrap();
        assert_eq!(chain_nonce, 0, "no transaction should have been sent");
    }

    #[tokio::test]
    async fn budget_refuses_bump_and_abandons() {
        let (_anvil, provider, sender) = setup();
        let recipient = _anvil.addresses()[1];
        let mut dispatcher = Dispatcher::new(provider.clone(), sender, fast_config());

        provider.anvil_set_auto_mine(false).await.unwrap();

        // Budget exactly covers the initial attempt (value of 1 wei included), but not a +25%
        // bump.
        let initial = provider.estimate_eip1559_fees().await.unwrap();
        let budget = attempt_cost(
            21_000,
            &Fees::Eip1559 {
                max_fee_per_gas: initial.max_fee_per_gas,
                max_priority_fee_per_gas: initial.max_priority_fee_per_gas,
            },
            U256::from(1),
        );

        let outcome = dispatcher.dispatch(transfer(recipient), 21_000, budget).await;
        assert!(matches!(outcome, DispatchOutcome::Abandoned), "got {outcome:?}");

        // Exactly one transaction was sent (it is pending), and the floor is the unbumped
        // initial fee.
        let pending_nonce = provider.get_transaction_count(sender).pending().await.unwrap();
        assert_eq!(pending_nonce, 1, "exactly one send should have happened");
        let floor = dispatcher.fee_floor.expect("floor should be recorded");
        assert!(
            floor.cap() < initial.max_fee_per_gas * 125 / 100,
            "the floor should be the unbumped initial fee"
        );
    }
}
