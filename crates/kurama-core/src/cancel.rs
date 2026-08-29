use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use kurama_protocol::traits::{BoxFuture, CancelSignal};
use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<CancelState>,
}

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) -> bool {
        if self.inner.cancelled.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.inner.notify.notify_waiters();
        true
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
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

impl CancelSignal for CancelToken {
    fn is_cancelled(&self) -> bool {
        CancelToken::is_cancelled(self)
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(CancelToken::cancelled(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_is_sticky_and_wakes_all_waiters() {
        let token = CancelToken::new();
        let first = token.clone();
        let second = token.clone();
        let waiters = tokio::spawn(async move {
            tokio::join!(first.cancelled(), second.cancelled());
        });
        assert!(token.cancel());
        assert!(!token.cancel());
        waiters.await.expect("waiters");
        token.cancelled().await;
    }
}
