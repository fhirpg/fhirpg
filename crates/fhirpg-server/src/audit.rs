//! Disclosure logging: how an access record gets from a served read into
//! `fhirpg_access_log` (spec PR12.5, PR12.6).
//!
//! The choice here is a real one, and it is the deployment's to make rather
//! than ours:
//!
//! - **sync** — the record is committed before the response is returned. If
//!   the log cannot be written, the read is refused. Nothing is disclosed
//!   that is not recorded, and every read pays a round trip.
//! - **async** — the record is queued in memory and written in batches. Reads
//!   pay an enqueue instead of a round trip. The window is real: a process
//!   killed with records queued loses them, and that is the price of the
//!   mode, stated plainly rather than hidden.
//! - **off** — no disclosure logging at all. Requires `--allow-unaudited`.
//!
//! Async mode still fails **closed on saturation**: a full queue means the
//! writer cannot keep up, and the answer to that is to refuse the read, not
//! to drop the record and serve the data anyway. A dropped audit record is
//! indistinguishable, afterwards, from a disclosure that never happened.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use fhirpg_store::{AccessRecord, Store};
use tokio::sync::mpsc;

/// How disclosures reach the access log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditMode {
    /// Commit before responding.
    Sync,
    /// Queue in memory, write in batches.
    Async {
        /// Bounded queue depth. Reached means reads are refused.
        capacity: usize,
        /// Records per statement.
        batch: usize,
        /// Longest a record waits before being written even if the batch is
        /// not full: the bound on how much is lost if the process dies.
        interval: Duration,
    },
    /// No disclosure logging.
    Off,
}

impl AuditMode {
    /// The default async tuning: 10k queued records, 256 per statement,
    /// flushed at least every 250ms.
    pub fn async_default() -> Self {
        AuditMode::Async {
            capacity: 10_000,
            batch: 256,
            interval: Duration::from_millis(250),
        }
    }
}

/// Counters an operator needs to tell a healthy audit path from a failing
/// one. All are monotonic, so they work as Prometheus counters.
#[derive(Default, Debug)]
pub struct AuditMetrics {
    /// Records accepted for writing.
    pub enqueued: AtomicU64,
    /// Records committed.
    pub written: AtomicU64,
    /// Reads refused because the queue was full (fail closed).
    pub refused: AtomicU64,
    /// Records the writer could not commit. Non-zero means disclosures
    /// happened that the log does not show, which is an incident.
    pub lost: AtomicU64,
}

impl AuditMetrics {
    /// Records accepted but not yet accounted for. Derived from the monotonic
    /// counters rather than mutated separately: a gauge maintained by
    /// read-then-subtract can underflow under concurrency, and an audit queue
    /// reporting a nonsense depth is worse than one reporting none.
    pub fn depth(&self) -> u64 {
        self.enqueued
            .load(Relaxed)
            .saturating_sub(self.written.load(Relaxed) + self.lost.load(Relaxed))
    }
}

/// Why a disclosure could not be recorded. The caller must refuse the read.
#[derive(Debug)]
pub struct AuditRefused;

/// The write side of the audit path for one FHIR version's store.
pub struct AuditSink {
    mode: AuditMode,
    /// Behind a lock so `shutdown` can drop it: closing the channel is what
    /// tells the writer to drain and exit. `Sender::closed()` waits for the
    /// *receiver* to go away, which is the opposite of what is needed here
    /// and deadlocks against the very task it is waiting for.
    tx: std::sync::Mutex<Option<mpsc::Sender<AccessRecord>>>,
    metrics: Arc<AuditMetrics>,
    /// Joined on shutdown so queued records are flushed before exit.
    writer: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AuditSink {
    /// Build a sink and, in async mode, spawn its writer task.
    pub fn new(mode: AuditMode, store: Arc<Store>) -> Self {
        let metrics = Arc::new(AuditMetrics::default());
        let (tx, writer) = match mode {
            AuditMode::Async {
                capacity,
                batch,
                interval,
            } => {
                let (tx, rx) = mpsc::channel(capacity);
                let m = metrics.clone();
                let handle = tokio::spawn(writer_loop(rx, store, batch, interval, m));
                (Some(tx), Some(handle))
            }
            AuditMode::Sync | AuditMode::Off => (None, None),
        };
        Self {
            mode,
            tx: std::sync::Mutex::new(tx),
            metrics,
            writer: std::sync::Mutex::new(writer),
        }
    }

    pub fn mode(&self) -> AuditMode {
        self.mode
    }

    pub fn metrics(&self) -> &AuditMetrics {
        &self.metrics
    }

    /// Record one disclosure, or refuse.
    ///
    /// In sync mode this commits before returning. In async mode it enqueues
    /// and returns; a full queue is a refusal, never a silent drop.
    pub async fn record(&self, store: &Store, rec: AccessRecord) -> Result<(), AuditRefused> {
        match self.mode {
            AuditMode::Off => Ok(()),
            AuditMode::Sync => match store.log_access(&rec).await {
                Ok(()) => {
                    self.metrics.enqueued.fetch_add(1, Relaxed);
                    self.metrics.written.fetch_add(1, Relaxed);
                    Ok(())
                }
                Err(e) => {
                    // Loud, and fatal to the request: in sync mode the whole
                    // point is that nothing is disclosed unrecorded.
                    tracing::error!(error = %e, interaction = rec.interaction,
                                    "audit write failed; refusing the read");
                    self.metrics.refused.fetch_add(1, Relaxed);
                    Err(AuditRefused)
                }
            },
            AuditMode::Async { .. } => {
                // Cloned out under the lock so the send never holds it.
                let tx = self.tx.lock().expect("audit sender lock").clone();
                let Some(tx) = tx else {
                    tracing::error!("audit writer has shut down; refusing the read");
                    self.metrics.refused.fetch_add(1, Relaxed);
                    return Err(AuditRefused);
                };
                match tx.try_send(rec) {
                    Ok(()) => {
                        self.metrics.enqueued.fetch_add(1, Relaxed);
                        Ok(())
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::error!("audit queue is full; refusing the read");
                        self.metrics.refused.fetch_add(1, Relaxed);
                        Err(AuditRefused)
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        tracing::error!("audit writer is gone; refusing the read");
                        self.metrics.refused.fetch_add(1, Relaxed);
                        Err(AuditRefused)
                    }
                }
            }
        }
    }

    /// Close the queue and wait for the writer to drain it.
    ///
    /// Without this, a clean shutdown still loses whatever was queued — the
    /// records most likely to matter, since they are the most recent.
    pub async fn shutdown(&self) {
        let handle = {
            let mut guard = self.writer.lock().expect("audit writer lock");
            guard.take()
        };
        // Dropping every sender is what closes the channel and tells the
        // writer to drain and finish.
        drop(self.tx.lock().expect("audit sender lock").take());
        if let Some(h) = handle {
            let _ = h.await;
        }
    }
}

/// Drain the queue into batched inserts until the channel closes.
async fn writer_loop(
    mut rx: mpsc::Receiver<AccessRecord>,
    store: Arc<Store>,
    batch: usize,
    interval: Duration,
    metrics: Arc<AuditMetrics>,
) {
    let mut buf: Vec<AccessRecord> = Vec::with_capacity(batch);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            got = rx.recv_many(&mut buf, batch) => {
                if got == 0 {
                    // Channel closed and drained.
                    break;
                }
                if buf.len() >= batch {
                    flush(&store, &mut buf, &metrics).await;
                }
            }
            _ = ticker.tick() => {
                flush(&store, &mut buf, &metrics).await;
            }
        }
    }
    // Final drain: anything still buffered, plus anything queued behind it.
    while rx.recv_many(&mut buf, batch).await > 0 {
        flush(&store, &mut buf, &metrics).await;
    }
    flush(&store, &mut buf, &metrics).await;
}

async fn flush(store: &Store, buf: &mut Vec<AccessRecord>, metrics: &AuditMetrics) {
    if buf.is_empty() {
        return;
    }
    let n = buf.len() as u64;
    match store.log_access_batch(buf).await {
        Ok(()) => {
            metrics.written.fetch_add(n, Relaxed);
        }
        Err(e) => {
            // The reads are already served. Nothing here can un-disclose
            // them, so the honest response is to say exactly how many
            // records were lost and let the alert fire.
            tracing::error!(error = %e, count = n,
                            "audit batch write failed; disclosure records lost");
            metrics.lost.fetch_add(n, Relaxed);
        }
    }
    buf.clear();
}
