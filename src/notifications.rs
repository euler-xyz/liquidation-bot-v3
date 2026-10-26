//! Notifies Slack when accounts end up in an error state.
//!
//! The [`NotificationTracker`] decides *whether* an account should be reported, it is plain
//! synchronous state so it can be tested without any network. The [`Notifier`] decides *how*
//! a batch is delivered, in production that is a Slack incoming webhook.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use alloy::primitives::{Address, Bytes};
use anyhow::Result;
use backon::{ExponentialBuilder, Retryable};
use chrono::{DateTime, TimeDelta, Utc};
use itertools::Itertools;
use reqwest::Url;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::{
    config::VaultFilterItem,
    types::{Account, LiquidationReasoning, LiquidationReasoningError},
};

/// The maximum number of accounts listed in a single message, the remainder is summarized.
const MAX_LISTED_ALERTS: usize = 20;

/// The maximum number of characters of an error message included in a notification.
const MAX_DETAIL_CHARS: usize = 300;

/// The maximum number of bytes of revert data included in a notification. This covers the
/// selector plus the first two arguments.
const MAX_REVERT_BYTES: usize = 68;

/// The error kinds we notify about, any other status is never reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ErrorKind {
    Oracle,
    LiquidationRevert,
    Other,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::Oracle => write!(f, "OracleError"),
            ErrorKind::LiquidationRevert => write!(f, "LiquidationRevert"),
            ErrorKind::Other => write!(f, "Other"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Classification {
    // The account is in an error state we report on.
    Report { kind: ErrorKind, detail: String },
    // The account was evaluated and is not in an error state.
    Recovered,
    // The status tells us nothing about whether the account recovered. `Unknown` is the
    // status of every freshly (re)fetched account and of an account for which a
    // liquidation was just prepared, so it must never count as a recovery.
    Ignore,
}

fn classify(status: &LiquidationReasoning) -> Classification {
    match status {
        LiquidationReasoning::Unknown | LiquidationReasoning::Blacklisted(_) => {
            Classification::Ignore
        }
        LiquidationReasoning::Healthy
        | LiquidationReasoning::Unprofitable
        | LiquidationReasoning::NoSwapPath => Classification::Recovered,
        LiquidationReasoning::Error(error) => match error {
            LiquidationReasoningError::OracleError { message } => Classification::Report {
                kind: ErrorKind::Oracle,
                detail: truncate(message),
            },
            LiquidationReasoningError::LiquidationRevert { data } => Classification::Report {
                kind: ErrorKind::LiquidationRevert,
                detail: format_revert_data(data),
            },
            LiquidationReasoningError::Other { message } => Classification::Report {
                kind: ErrorKind::Other,
                detail: truncate(message),
            },
            // A lens error is judged by the state it wraps.
            LiquidationReasoningError::LensError { error, state } => match classify(state) {
                Classification::Report { kind, detail } => Classification::Report {
                    kind,
                    detail: format!(
                        "{detail} (lens query also failed for vault {})",
                        error.vault
                    ),
                },
                other => other,
            },
        },
    }
}

fn truncate(message: &str) -> String {
    match message.char_indices().nth(MAX_DETAIL_CHARS) {
        Some((index, _)) => format!("{}…", &message[..index]),
        None => message.to_string(),
    }
}

fn format_revert_data(data: &Bytes) -> String {
    if data.is_empty() {
        return "empty revert data".to_string();
    }

    let shown = alloy::hex::encode_prefixed(&data[..data.len().min(MAX_REVERT_BYTES)]);
    if data.len() > MAX_REVERT_BYTES {
        format!("revert data {shown}…")
    } else {
        format!("revert data {shown}")
    }
}

/// A single account that should be reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountAlert {
    pub account: Address,
    pub kind: ErrorKind,
    pub detail: String,
    pub borrow_vaults: Vec<Address>,
    pub collateral_vaults: Vec<Address>,
}

/// All alerts of a single sweep, delivered as one message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationBatch {
    pub chain_id: u64,
    pub alerts: Vec<AccountAlert>,
}

impl NotificationBatch {
    /// Renders the batch as Slack `mrkdwn`, grouped by error kind and borrow vault.
    pub fn to_slack_text(&self) -> String {
        let mut text = format!(
            ":rotating_light: *Chain {}*: {} account(s) entered an error state",
            self.chain_id,
            self.alerts.len()
        );

        let groups = self
            .alerts
            .iter()
            .sorted_by_key(|a| (a.kind, a.borrow_vaults.first().copied()))
            .chunk_by(|a| (a.kind, a.borrow_vaults.first().copied()));

        let mut listed = 0;
        for ((kind, borrow_vault), alerts) in &groups {
            if listed >= MAX_LISTED_ALERTS {
                break;
            }

            let borrow_vault = borrow_vault
                .map(|v| format!("`{v}`"))
                .unwrap_or_else(|| "unknown".to_string());
            text.push_str(&format!("\n\n*{kind}*, borrow vault {borrow_vault}"));

            for alert in alerts.take(MAX_LISTED_ALERTS - listed) {
                let collaterals = alert
                    .collateral_vaults
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .join(", ");
                text.push_str(&format!(
                    "\n• `{}`, collaterals: {}, {}",
                    alert.account,
                    if collaterals.is_empty() {
                        "none".to_string()
                    } else {
                        collaterals
                    },
                    escape_mrkdwn(&alert.detail)
                ));
                listed += 1;
            }
        }

        if self.alerts.len() > listed {
            text.push_str(&format!("\n\n…and {} more", self.alerts.len() - listed));
        }

        text
    }
}

/// Slack treats these as control characters, see
/// https://api.slack.com/reference/surfaces/formatting#escaping
fn escape_mrkdwn(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[derive(Debug)]
struct Entry {
    // When we last notified about this account. `None` if the account was never actually
    // reported, i.e. it was already in error at startup or is on the notification blacklist.
    notified_at: Option<DateTime<Utc>>,
    // If the account has been seen in a non-error state since it was last reported.
    recovered: bool,
}

impl Entry {
    /// If a new error for this account should be reported again. It must have recovered in
    /// between, and we must not have reported it within the cooldown, so an account that
    /// flaps between healthy and broken does not spam. An account that broke again within the
    /// cooldown is reported once the cooldown has passed, if it is still broken by then.
    fn rearmed(&self, now: DateTime<Utc>, cooldown: TimeDelta) -> bool {
        self.recovered
            && self
                .notified_at
                .is_none_or(|notified_at| now - notified_at >= cooldown)
    }
}

/// Decides which accounts should be reported, without sending anything itself.
pub struct NotificationTracker {
    // Vaults that produce false positives. Accounts with a borrow or collateral position in
    // one of them are never reported.
    blacklist: HashSet<Address>,
    cooldown: TimeDelta,
    // Keyed by address instead of living on `Account`, as the tracker replaces an account
    // with a freshly fetched one (and status) on every resync.
    entries: HashMap<Address, Entry>,
}

impl NotificationTracker {
    pub fn new(blacklist: &[VaultFilterItem], cooldown: std::time::Duration) -> Self {
        NotificationTracker {
            blacklist: blacklist.iter().map(|item| item.address()).collect(),
            cooldown: TimeDelta::from_std(cooldown).unwrap_or(TimeDelta::MAX),
            entries: HashMap::new(),
        }
    }

    fn is_blacklisted(&self, account: &Account) -> bool {
        account
            .borrows
            .iter()
            .map(|b| b.vault.address)
            .chain(
                account
                    .collaterals
                    .iter()
                    .map(|c| c.vault.erc4626().address),
            )
            .any(|vault| self.blacklist.contains(&vault))
    }

    /// Updates the state with the current status of the accounts and returns the accounts
    /// that should be reported.
    pub fn observe(&mut self, accounts: &[Account], now: DateTime<Utc>) -> Vec<AccountAlert> {
        let mut alerts = Vec::new();

        for account in accounts {
            match classify(&account.status()) {
                Classification::Ignore => {}
                Classification::Recovered => {
                    if let Some(entry) = self.entries.get_mut(&account.address) {
                        entry.recovered = true;
                    }
                }
                Classification::Report { kind, detail } => {
                    let should_report = match self.entries.get(&account.address) {
                        None => true,
                        Some(entry) => entry.rearmed(now, self.cooldown),
                    };
                    if !should_report {
                        continue;
                    }

                    // A blacklisted account is still tracked as if it was reported, so
                    // removing its vault from the blacklist does not suddenly report it.
                    let suppressed = self.is_blacklisted(account);
                    self.entries.insert(
                        account.address,
                        Entry {
                            notified_at: (!suppressed).then_some(now),
                            recovered: false,
                        },
                    );

                    if !suppressed {
                        alerts.push(AccountAlert {
                            account: account.address,
                            kind,
                            detail,
                            borrow_vaults: account
                                .borrows
                                .iter()
                                .map(|b| b.vault.address)
                                .collect(),
                            collateral_vaults: account
                                .collaterals
                                .iter()
                                .map(|c| c.vault.erc4626().address)
                                .collect(),
                        });
                    }
                }
            }
        }

        alerts
    }

    /// Records the accounts that are currently in error without reporting them. They are
    /// only reported once they have recovered and break again.
    pub fn baseline(&mut self, accounts: &[Account]) {
        for account in accounts {
            if matches!(classify(&account.status()), Classification::Report { .. }) {
                self.entries.entry(account.address).or_insert(Entry {
                    notified_at: None,
                    recovered: false,
                });
            }
        }
    }

    /// Forgets the accounts that are no longer tracked (e.g. their debt was repaid).
    pub fn prune(&mut self, tracked: &HashSet<Address>) {
        self.entries.retain(|address, _| tracked.contains(address));
    }
}

/// Connects the [`NotificationTracker`] to the main loop and hands batches off to the
/// notifier task, so a slow or unavailable Slack never stalls the main loop.
pub struct Notifications {
    chain_id: u64,
    tracker: NotificationTracker,
    sender: mpsc::Sender<NotificationBatch>,
    baselined: bool,
}

impl Notifications {
    pub fn new(
        chain_id: u64,
        tracker: NotificationTracker,
        sender: mpsc::Sender<NotificationBatch>,
    ) -> Self {
        Notifications {
            chain_id,
            tracker,
            sender,
            baselined: false,
        }
    }

    /// To be called with *all* tracked accounts after a full resync, which is the only point
    /// where every account has a fresh status. The first call after startup only records the
    /// accounts that are already in error.
    pub fn on_full_resync(&mut self, accounts: &[Account]) {
        if !self.baselined {
            self.tracker.baseline(accounts);
            self.baselined = true;
            return;
        }

        let alerts = self.tracker.observe(accounts, Utc::now());
        self.tracker
            .prune(&accounts.iter().map(|a| a.address).collect());
        self.dispatch(alerts);
    }

    /// To be called with the accounts that were re-evaluated outside of a full resync, e.g.
    /// after a price update.
    pub fn on_partial_update(&mut self, accounts: &[Account]) {
        // Until the first full resync has baselined the state, anything we see here would be
        // reported as new.
        if !self.baselined {
            return;
        }

        let alerts = self.tracker.observe(accounts, Utc::now());
        self.dispatch(alerts);
    }

    fn dispatch(&self, alerts: Vec<AccountAlert>) {
        if alerts.is_empty() {
            return;
        }

        info!(
            "Notifying about {} account(s) that entered an error state",
            alerts.len()
        );

        // NOTE: Never block the main loop on notifications, if the notifier is backed up we
        // drop the batch.
        if let Err(e) = self.sender.try_send(NotificationBatch {
            chain_id: self.chain_id,
            alerts,
        }) {
            warn!(
                "Could not queue an error notification, it is dropped. err: {}",
                e
            );
        }
    }
}

pub trait Notifier {
    #![allow(async_fn_in_trait)]
    async fn send(&self, batch: &NotificationBatch) -> Result<()>;
}

/// Delivers notifications to a Slack incoming webhook.
pub struct SlackWebhookNotifier {
    client: reqwest::Client,
    webhook_url: Url,
    retry: ExponentialBuilder,
}

impl SlackWebhookNotifier {
    pub fn new(webhook_url: Url) -> Self {
        SlackWebhookNotifier {
            client: reqwest::Client::new(),
            webhook_url,
            retry: ExponentialBuilder::default().with_max_times(3),
        }
    }
}

impl Notifier for SlackWebhookNotifier {
    async fn send(&self, batch: &NotificationBatch) -> Result<()> {
        let body = serde_json::json!({ "text": batch.to_slack_text() });

        (|| async {
            self.client
                .post(self.webhook_url.clone())
                .json(&body)
                .send()
                .await?
                .error_for_status()?;

            Ok::<_, reqwest::Error>(())
        })
        .retry(self.retry)
        .await
        // The webhook URL is a secret, keep it out of the error (and thus the logs).
        .map_err(|e| e.without_url())?;

        Ok(())
    }
}

/// Delivers every batch that is received until the channel closes.
pub async fn run_notifier(
    notifier: impl Notifier,
    mut receiver: mpsc::Receiver<NotificationBatch>,
) {
    while let Some(batch) = receiver.recv().await {
        if let Err(e) = notifier.send(&batch).await {
            error!(
                "Could not deliver a notification about {} account(s), err: {:?}",
                batch.alerts.len(),
                e
            );
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy::primitives::{Address, Bytes, bytes};
    use anyhow::Result;
    use axum::{Json, Router, http::StatusCode, routing::post};
    use chrono::{TimeDelta, Utc};
    use tokio::sync::mpsc;

    use super::{
        AccountAlert, ErrorKind, MAX_LISTED_ALERTS, NotificationBatch, NotificationTracker,
        Notifications, Notifier, SlackWebhookNotifier, run_notifier,
    };
    use crate::types::{
        Account, LensError, LiquidationReasoning, LiquidationReasoningError, VaultBorrowPosition,
        VaultCollateralPosition,
    };

    const COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);

    #[derive(Clone, Default)]
    struct RecordingNotifier {
        batches: Arc<Mutex<Vec<NotificationBatch>>>,
    }

    impl Notifier for RecordingNotifier {
        async fn send(&self, batch: &NotificationBatch) -> Result<()> {
            self.batches.lock().unwrap().push(batch.clone());
            Ok(())
        }
    }

    fn account() -> Account {
        Account::new(
            Address::random(),
            vec![VaultBorrowPosition::generate_random()],
            vec![VaultCollateralPosition::generate_random()],
        )
    }

    fn account_with_status(status: LiquidationReasoning) -> Account {
        let account = account();
        account.set_status(status);
        account
    }

    fn oracle_error() -> LiquidationReasoning {
        LiquidationReasoning::Error(LiquidationReasoningError::OracleError {
            message: "oracle error".to_string(),
        })
    }

    fn revert_error() -> LiquidationReasoning {
        LiquidationReasoning::Error(LiquidationReasoningError::LiquidationRevert {
            data: bytes!("0xdeadbeef"),
        })
    }

    fn other_error() -> LiquidationReasoning {
        LiquidationReasoning::Error(LiquidationReasoningError::Other {
            message: "other error".to_string(),
        })
    }

    fn lens_error(state: LiquidationReasoning) -> LiquidationReasoning {
        LiquidationReasoning::Error(LiquidationReasoningError::LensError {
            error: LensError {
                vault: Address::random(),
                query_failure_reason: Bytes::new(),
            },
            state: Box::from(state),
        })
    }

    fn tracker() -> NotificationTracker {
        NotificationTracker::new(&[], COOLDOWN)
    }

    #[test]
    fn only_the_reportable_errors_notify() {
        let mut tracker = tracker();
        let now = Utc::now();

        let reported = [oracle_error(), revert_error(), other_error()].map(account_with_status);
        let not_reported = [
            LiquidationReasoning::Unknown,
            LiquidationReasoning::Healthy,
            LiquidationReasoning::Unprofitable,
            LiquidationReasoning::NoSwapPath,
            LiquidationReasoning::Blacklisted(vec![Address::random()]),
            lens_error(LiquidationReasoning::Unknown),
        ]
        .map(account_with_status);

        let alerts = tracker.observe(&not_reported, now);
        assert!(alerts.is_empty(), "Unexpected alerts: {alerts:?}");

        let alerts = tracker.observe(&reported, now);
        assert_eq!(
            alerts.iter().map(|a| a.kind).collect::<Vec<_>>(),
            vec![
                ErrorKind::Oracle,
                ErrorKind::LiquidationRevert,
                ErrorKind::Other
            ]
        );
        assert_eq!(alerts[1].detail, "revert data 0xdeadbeef");
    }

    #[test]
    fn a_lens_error_is_judged_by_its_inner_state() {
        let mut tracker = tracker();

        let account = account_with_status(lens_error(oracle_error()));
        let alerts = tracker.observe(&[account], Utc::now());

        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, ErrorKind::Oracle);
        assert!(
            alerts[0]
                .detail
                .starts_with("oracle error (lens query also failed")
        );
    }

    #[test]
    fn an_account_that_stays_in_error_is_reported_once() {
        let mut tracker = tracker();
        let account = account_with_status(oracle_error());
        let now = Utc::now();

        assert_eq!(
            tracker.observe(std::slice::from_ref(&account), now).len(),
            1
        );
        for i in 1..10 {
            let later = now + TimeDelta::days(i);
            assert!(
                tracker
                    .observe(std::slice::from_ref(&account), later)
                    .is_empty()
            );
        }
    }

    #[test]
    fn changing_error_kind_does_not_notify_again() {
        let mut tracker = tracker();
        let account = account_with_status(oracle_error());
        let now = Utc::now();

        assert_eq!(
            tracker.observe(std::slice::from_ref(&account), now).len(),
            1
        );

        account.set_status(revert_error());
        assert!(tracker.observe(&[account], now).is_empty());
    }

    #[test]
    fn an_unknown_status_is_not_a_recovery() {
        let mut tracker = tracker();
        let account = account_with_status(oracle_error());
        let now = Utc::now();

        assert_eq!(
            tracker.observe(std::slice::from_ref(&account), now).len(),
            1
        );

        account.set_status(LiquidationReasoning::Unknown);
        tracker.observe(std::slice::from_ref(&account), now + TimeDelta::days(2));

        account.set_status(oracle_error());
        assert!(
            tracker
                .observe(&[account], now + TimeDelta::days(3))
                .is_empty()
        );
    }

    #[test]
    fn a_recovered_account_is_only_reported_again_after_the_cooldown() {
        let mut tracker = tracker();
        let account = account_with_status(oracle_error());
        let now = Utc::now();

        assert_eq!(
            tracker.observe(std::slice::from_ref(&account), now).len(),
            1
        );

        // It recovers and breaks again within the cooldown, which is not reported.
        account.set_status(LiquidationReasoning::Healthy);
        tracker.observe(std::slice::from_ref(&account), now + TimeDelta::hours(1));
        account.set_status(oracle_error());
        assert!(
            tracker
                .observe(std::slice::from_ref(&account), now + TimeDelta::hours(2))
                .is_empty()
        );

        // It is still broken once the cooldown has passed, so it is reported again.
        assert_eq!(
            tracker
                .observe(std::slice::from_ref(&account), now + TimeDelta::hours(25))
                .len(),
            1
        );

        // And that resets it, it has to recover again first.
        assert!(
            tracker
                .observe(&[account], now + TimeDelta::days(5))
                .is_empty()
        );
    }

    #[test]
    fn the_notification_blacklist_matches_borrow_and_collateral_vaults() {
        let by_borrow = account_with_status(oracle_error());
        let by_collateral = account_with_status(oracle_error());
        let not_listed = account_with_status(oracle_error());

        let mut tracker = NotificationTracker::new(
            &[
                by_borrow.borrows[0].vault.address.into(),
                by_collateral.collaterals[0].vault.erc4626().address.into(),
            ],
            COOLDOWN,
        );

        let alerts = tracker.observe(
            &[by_borrow.clone(), by_collateral, not_listed.clone()],
            Utc::now(),
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].account, not_listed.address);

        // Removing the vault from the blacklist does not report an account that is already
        // in error.
        tracker.blacklist.clear();
        assert!(tracker.observe(&[by_borrow], Utc::now()).is_empty());
    }

    #[test]
    fn regular_blacklisted_accounts_are_not_reported() {
        let mut tracker = tracker();
        let account =
            account_with_status(LiquidationReasoning::Blacklisted(vec![Address::random()]));

        // Blacklisted is sticky, so this stays blacklisted.
        account.set_status(oracle_error());
        assert!(tracker.observe(&[account], Utc::now()).is_empty());
    }

    #[test]
    fn baseline_suppresses_existing_errors_only() {
        let mut tracker = tracker();
        let existing = account_with_status(oracle_error());
        let now = Utc::now();

        tracker.baseline(std::slice::from_ref(&existing));
        assert!(
            tracker
                .observe(std::slice::from_ref(&existing), now)
                .is_empty()
        );

        // A new account breaking is reported.
        let new = account_with_status(other_error());
        assert_eq!(tracker.observe(&[existing.clone(), new], now).len(), 1);

        // The baselined account was never reported, so after recovering it is reported
        // straight away when it breaks again.
        existing.set_status(LiquidationReasoning::Healthy);
        tracker.observe(std::slice::from_ref(&existing), now);
        existing.set_status(oracle_error());
        assert_eq!(tracker.observe(&[existing], now).len(), 1);
    }

    #[test]
    fn prune_forgets_untracked_accounts() {
        let mut tracker = tracker();
        let account = account_with_status(oracle_error());
        let now = Utc::now();

        assert_eq!(
            tracker.observe(std::slice::from_ref(&account), now).len(),
            1
        );

        tracker.prune(&HashSet::new());
        assert_eq!(tracker.observe(&[account], now).len(), 1);
    }

    #[test]
    fn a_mass_event_is_one_capped_message() {
        let mut tracker = tracker();
        let accounts: Vec<_> = (0..100)
            .map(|_| account_with_status(oracle_error()))
            .collect();

        let alerts = tracker.observe(&accounts, Utc::now());
        assert_eq!(alerts.len(), 100);

        let text = NotificationBatch {
            chain_id: 1,
            alerts,
        }
        .to_slack_text();
        assert!(text.starts_with(":rotating_light: *Chain 1*: 100 account(s)"));
        assert_eq!(text.matches("\n• ").count(), MAX_LISTED_ALERTS);
        assert!(text.ends_with(&format!("…and {} more", 100 - MAX_LISTED_ALERTS)));
    }

    #[test]
    fn slack_text_escapes_control_characters() {
        let batch = NotificationBatch {
            chain_id: 1,
            alerts: vec![AccountAlert {
                account: Address::ZERO,
                kind: ErrorKind::Other,
                detail: "<!channel> & co".to_string(),
                borrow_vaults: vec![],
                collateral_vaults: vec![],
            }],
        };

        let text = batch.to_slack_text();
        assert!(text.contains("&lt;!channel&gt; &amp; co"));
        assert!(!text.contains("<!channel>"));
    }

    #[tokio::test]
    async fn notifications_are_delivered_after_the_baseline() {
        let notifier = RecordingNotifier::default();
        let (sender, receiver) = mpsc::channel(16);
        let task = tokio::spawn(run_notifier(notifier.clone(), receiver));

        let mut notifications = Notifications::new(1, tracker(), sender);
        let existing = account_with_status(oracle_error());

        // Nothing is reported before, or during, the first full resync.
        notifications.on_partial_update(&[account_with_status(oracle_error())]);
        notifications.on_full_resync(std::slice::from_ref(&existing));

        // After that new errors are, existing ones are not.
        let new = account_with_status(revert_error());
        notifications.on_partial_update(&[existing.clone(), new.clone()]);
        notifications.on_full_resync(&[existing, new.clone()]);

        drop(notifications);
        task.await.unwrap();

        let batches = notifier.batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].chain_id, 1);
        assert_eq!(batches[0].alerts.len(), 1);
        assert_eq!(batches[0].alerts[0].account, new.address);
    }

    async fn serve(status: StatusCode) -> (reqwest::Url, Arc<Mutex<Vec<serde_json::Value>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().route(
            "/services/secret",
            post({
                let received = received.clone();
                move |Json(body): Json<serde_json::Value>| async move {
                    received.lock().unwrap().push(body);
                    status
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/services/secret", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        (url.parse().unwrap(), received)
    }

    fn batch() -> NotificationBatch {
        NotificationBatch {
            chain_id: 1,
            alerts: vec![AccountAlert {
                account: Address::ZERO,
                kind: ErrorKind::Oracle,
                detail: "oracle error".to_string(),
                borrow_vaults: vec![Address::ZERO],
                collateral_vaults: vec![],
            }],
        }
    }

    #[tokio::test]
    async fn slack_webhook_posts_the_text() {
        let (url, received) = serve(StatusCode::OK).await;

        SlackWebhookNotifier::new(url).send(&batch()).await.unwrap();

        let received = received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["text"], batch().to_slack_text());
    }

    #[tokio::test]
    async fn slack_webhook_failures_retry_and_do_not_leak_the_url() {
        let (url, received) = serve(StatusCode::INTERNAL_SERVER_ERROR).await;

        let mut notifier = SlackWebhookNotifier::new(url);
        notifier.retry = notifier.retry.with_min_delay(Duration::from_millis(1));

        let err = notifier.send(&batch()).await.unwrap_err();

        // The first attempt plus three retries.
        assert_eq!(received.lock().unwrap().len(), 4);
        assert!(
            !format!("{err:?}").contains("secret"),
            "Leaked the url: {err:?}"
        );
    }
}
