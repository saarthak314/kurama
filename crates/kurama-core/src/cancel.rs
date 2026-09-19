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

    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        let inner = self.inner.clone();
        async move {
            if inner.cancelled.load(Ordering::Acquire) {
                return;
            }
            let notified = inner.notify.notified();
            if inner.cancelled.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

impl CancelSignal for CancelToken {
    fn is_cancelled(&self) -> bool {
        CancelToken::is_cancelled(self)
    }

    fn cancelled(&self) -> BoxFuture<'static, ()> {
        Box::pin(CancelToken::cancelled(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_is_sticky_and_wakes_all_waiters() {
        let token = CancelToken::new();
        let first: Arc<dyn CancelSignal> = Arc::new(token.clone());
        let second: Arc<dyn CancelSignal> = Arc::new(token.clone());
        let first_wait = first.cancelled();
        let second_wait = second.cancelled();
        drop((first, second));
        let waiters = tokio::spawn(async move {
            tokio::join!(first_wait, second_wait);
        });
        assert!(token.cancel());
        assert!(!token.cancel());
        waiters.await.expect("waiters");
        token.cancelled().await;
    }
}
