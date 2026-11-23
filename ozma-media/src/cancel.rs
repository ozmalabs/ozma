// SPDX-License-Identifier: AGPL-3.0-only
//! A small, allocation-light cancellation token for pipeline stages.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

#[derive(Debug, Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

/// A cloneable cancellation signal.
///
/// Cancellation is one-way and idempotent. Every clone observes the same
/// state, which is what lets a failed encoder stop the capture task and lets
/// an owning session stop both.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

impl CancelToken {
    /// Create a token that is not yet cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation and wake every waiter.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.notify.notify_waiters();
    }

    /// Check the current state without waiting.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Wait until [`CancelToken::cancel`] is called.
    ///
    /// Returns immediately if the token was already cancelled. The double
    /// check around `notified()` closes the race between observing the flag
    /// and registering as a waiter.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancellation_wakes_waiters() {
        let token = CancelToken::new();
        let waiter = token.clone();
        let handle = tokio::spawn(async move { waiter.cancelled().await });
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("waiter should wake")
            .expect("waiter task should not panic");
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn already_cancelled_returns_immediately() {
        let token = CancelToken::new();
        token.cancel();
        token.cancel();
        tokio::time::timeout(Duration::from_millis(50), token.cancelled())
            .await
            .expect("already-cancelled token must not block");
    }
}
