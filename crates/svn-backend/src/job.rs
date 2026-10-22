//! Handles for in-flight operations.
//!
//! Results are delivered over `async_channel`, so the GTK side can await
//! them with `glib::spawn_future_local` and tests can block with
//! [`Job::wait`]. Dropping a handle cancels the operation.

use crate::error::SvnError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shared cancellation flag. The runner polls it and kills the svn child
/// process when it is set.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Cancels its token when dropped.
#[derive(Debug)]
struct CancelOnDrop(CancelToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// A single-result operation.
#[derive(Debug)]
pub struct Job<T> {
    rx: async_channel::Receiver<Result<T, SvnError>>,
    guard: CancelOnDrop,
}

impl<T> Job<T> {
    /// Create a job and the sender the worker uses to deliver its result.
    pub fn new() -> (Self, async_channel::Sender<Result<T, SvnError>>, CancelToken) {
        let (tx, rx) = async_channel::bounded(1);
        let token = CancelToken::new();
        let job = Self {
            rx,
            guard: CancelOnDrop(token.clone()),
        };
        (job, tx, token)
    }

    /// A job that is already finished (e.g. a cache hit).
    pub fn ready(result: Result<T, SvnError>) -> Self {
        let (job, tx, _) = Self::new();
        // Capacity 1 and nobody else has the sender: cannot fail.
        let _ = tx.try_send(result);
        job
    }

    pub fn cancel(&self) {
        self.guard.0.cancel();
    }

    pub fn token(&self) -> CancelToken {
        self.guard.0.clone()
    }

    /// Await the result.
    pub async fn result(self) -> Result<T, SvnError> {
        self.rx.recv().await.unwrap_or_else(|_| Err(SvnError::cancelled()))
    }

    /// Block the current thread for the result (tests, non-GUI callers).
    pub fn wait(self) -> Result<T, SvnError> {
        self.rx
            .recv_blocking()
            .unwrap_or_else(|_| Err(SvnError::cancelled()))
    }
}

/// A multi-result operation delivering batches. The stream ends (`None`)
/// after the last batch or after an `Err` item.
#[derive(Debug)]
pub struct Stream<T> {
    rx: async_channel::Receiver<Result<Vec<T>, SvnError>>,
    guard: CancelOnDrop,
}

impl<T> Stream<T> {
    /// Create a stream and the sender the worker uses to deliver batches.
    /// The channel is bounded so a slow consumer applies backpressure to
    /// the parser rather than buffering a whole huge listing.
    pub fn new() -> (
        Self,
        async_channel::Sender<Result<Vec<T>, SvnError>>,
        CancelToken,
    ) {
        let (tx, rx) = async_channel::bounded(16);
        let token = CancelToken::new();
        let stream = Self {
            rx,
            guard: CancelOnDrop(token.clone()),
        };
        (stream, tx, token)
    }

    pub fn cancel(&self) {
        self.guard.0.cancel();
    }

    pub fn token(&self) -> CancelToken {
        self.guard.0.clone()
    }

    /// Next batch, or `None` when finished.
    pub async fn next(&mut self) -> Option<Result<Vec<T>, SvnError>> {
        self.rx.recv().await.ok()
    }

    /// Blocking variant of [`Stream::next`].
    pub fn next_blocking(&mut self) -> Option<Result<Vec<T>, SvnError>> {
        self.rx.recv_blocking().ok()
    }

    /// Block until the stream ends and collect everything (tests).
    pub fn collect_blocking(mut self) -> Result<Vec<T>, SvnError> {
        let mut all = Vec::new();
        while let Some(batch) = self.next_blocking() {
            all.extend(batch?);
        }
        Ok(all)
    }
}
