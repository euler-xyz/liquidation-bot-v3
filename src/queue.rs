use std::collections::{HashSet, VecDeque};

use alloy::primitives::Address;

/// A queue of per-account work, of which at most `concurrency` jobs run at a time and at most one
/// per account.
///
/// Pushing an account that is already queued is a no-op. Pushing one that is running schedules a
/// single re-run once it completes, so the latest request is always served (e.g. a refetch after
/// a second event) without ever running the same account twice at once.
pub struct AccountQueue {
    concurrency: usize,
    queue: VecDeque<Address>,
    queued: HashSet<Address>,
    running: HashSet<Address>,
    rerun: HashSet<Address>,
}

impl AccountQueue {
    pub fn new(concurrency: usize) -> Self {
        AccountQueue {
            concurrency: concurrency.max(1),
            queue: VecDeque::new(),
            queued: HashSet::new(),
            running: HashSet::new(),
            rerun: HashSet::new(),
        }
    }

    /// Requests work for an account.
    pub fn push(&mut self, account: Address) {
        if self.running.contains(&account) {
            self.rerun.insert(account);
        } else if self.queued.insert(account) {
            self.queue.push_back(account);
        }
    }

    /// The next account to start work for, if there is one and there is capacity for it. The
    /// account counts as running until [`AccountQueue::complete`] is called for it.
    pub fn start_next(&mut self) -> Option<Address> {
        if self.running.len() >= self.concurrency {
            return None;
        }

        let account = self.queue.pop_front()?;
        self.queued.remove(&account);
        self.running.insert(account);
        Some(account)
    }

    /// Marks the work for an account as done, re-queueing it if it was requested again meanwhile.
    pub fn complete(&mut self, account: Address) {
        self.running.remove(&account);
        if self.rerun.remove(&account) {
            self.push(account);
        }
    }
}

#[cfg(test)]
mod test {
    use alloy::primitives::Address;

    use super::AccountQueue;

    #[test]
    fn starts_in_fifo_order() {
        let mut queue = AccountQueue::new(16);
        let (a, b) = (Address::random(), Address::random());

        queue.push(a);
        queue.push(b);

        assert_eq!(queue.start_next(), Some(a));
        assert_eq!(queue.start_next(), Some(b));
        assert_eq!(queue.start_next(), None);
    }

    #[test]
    fn pushing_a_queued_account_is_a_no_op() {
        let mut queue = AccountQueue::new(16);
        let a = Address::random();

        queue.push(a);
        queue.push(a);

        assert_eq!(queue.start_next(), Some(a));
        assert_eq!(queue.start_next(), None);
    }

    #[test]
    fn respects_the_concurrency_limit() {
        let mut queue = AccountQueue::new(2);
        let accounts: Vec<Address> = (0..3).map(|_| Address::random()).collect();
        accounts.iter().for_each(|a| queue.push(*a));

        assert_eq!(queue.start_next(), Some(accounts[0]));
        assert_eq!(queue.start_next(), Some(accounts[1]));
        assert_eq!(queue.start_next(), None, "two are already running");

        queue.complete(accounts[0]);
        assert_eq!(queue.start_next(), Some(accounts[2]));
    }

    #[test]
    fn a_running_account_is_not_started_twice_but_re_runs_once_completed() {
        let mut queue = AccountQueue::new(16);
        let a = Address::random();

        queue.push(a);
        assert_eq!(queue.start_next(), Some(a));

        // Requested (twice) while running: it must not run concurrently...
        queue.push(a);
        queue.push(a);
        assert_eq!(queue.start_next(), None);

        // ...but runs once more after it completes.
        queue.complete(a);
        assert_eq!(queue.start_next(), Some(a));
        queue.complete(a);
        assert_eq!(queue.start_next(), None);
    }
}
