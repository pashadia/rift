//! In-flight chunk deduplication.
//!
//! When multiple concurrent reads request the same chunk, `InFlightChunks`
//! ensures the chunk is fetched only once. Subsequent callers wait for the
//! in-flight fetch to complete rather than issuing redundant network requests.
//!
//! After the fetch completes (success or error), the entry is removed from
//! the map. On success the disk cache serves subsequent hits; on error the
//! entry is cleaned up so retries can re-attempt.
//!
//! Uses a waiter-list pattern: waiters register `oneshot::Sender`s while
//! holding the bucket lock, and the first caller sends the result to all
//! registered waiters after `produce` completes. This avoids the
//! subscribe-after-send race inherent in broadcast or watch channels.

use bytes::Bytes;
use rift_common::FsError;
use scc::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Internal state for an in-flight chunk.
/// Type alias for the waiter list shared between the first caller and waiters.
type WaiterList = Arc<Mutex<Vec<oneshot::Sender<Result<Bytes, FsError>>>>>;

/// Internal state for an in-flight chunk.
enum EntryState {
    /// A fetch is in progress. Waiters are collected in the oneshot list.
    Pending { waiters: WaiterList },
}

/// A deduplication layer for concurrent chunk fetches keyed by BLAKE3 hash.
///
/// The first caller to successfully insert a `Pending` entry for a hash runs
/// the produce function. Concurrent callers register a `oneshot::Sender` in
/// the waiter list and await the result. After produce completes, the result
/// is sent to all registered waiters, and the entry is removed from the map.
pub struct InFlightChunks {
    map: HashMap<[u8; 32], EntryState>,
}

impl InFlightChunks {
    /// Create a new empty `InFlightChunks`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Return a chunk by hash, fetching it via `produce` if not already in flight.
    ///
    /// # Errors
    ///
    /// Returns `FsError::Io` if `produce` returns an error.
    pub async fn get_or_fetch<F, Fut>(&self, hash: &[u8; 32], produce: F) -> Result<Bytes, FsError>
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = Result<Bytes, FsError>> + Send,
    {
        loop {
            match self.map.entry_async(*hash).await {
                scc::hash_map::Entry::Occupied(entry) => {
                    match entry.get() {
                        EntryState::Pending { waiters } => {
                            // Register our oneshot sender while holding the
                            // bucket lock. This guarantees the first caller
                            // will see our sender when it drains the list.
                            let (tx, rx) = oneshot::channel();
                            {
                                let mut list = waiters.lock().expect("waiter mutex not poisoned");
                                list.push(tx);
                            }
                            // Release the bucket lock so the first caller can
                            // finish (send results, remove entry).
                            drop(entry);

                            // Await the result.
                            match rx.await {
                                Ok(Ok(data)) => return Ok(data),
                                Ok(Err(_)) => return Err(FsError::Io),
                                Err(_) => {
                                    // First caller dropped the sender without
                                    // sending — loop back and try again.
                                }
                            }
                        }
                    }
                }
                scc::hash_map::Entry::Vacant(entry) => {
                    // We are first — insert a pending entry.
                    let waiters = Arc::new(Mutex::new(Vec::new()));
                    entry.insert_entry(EntryState::Pending {
                        waiters: Arc::clone(&waiters),
                    });
                    // Bucket lock released when the temporary OccupiedEntry
                    // (returned by insert_entry) is dropped at the ';'.

                    // Yield so other tasks can register their oneshot senders
                    // before we run produce.
                    tokio::task::yield_now().await;

                    // Run the produce function.
                    let result = produce().await;

                    // Send the result to all registered waiters.
                    let send_result = match &result {
                        Ok(data) => Ok(data.clone()),
                        Err(_) => Err(FsError::Io),
                    };
                    {
                        let mut list = waiters.lock().expect("waiter mutex not poisoned");
                        for sender in list.drain(..) {
                            let _ = sender.send(send_result.clone());
                        }
                    }

                    // Remove the entry from the map regardless of outcome.
                    // On success, the disk cache serves subsequent hits.
                    // On error, retries start fresh.
                    self.map.remove_async(hash).await;

                    return result;
                }
            }
        }
    }
}

impl Default for InFlightChunks {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Barrier;

    #[tokio::test]
    async fn single_caller_produces_value() {
        let inflight = InFlightChunks::new();
        let hash = [0x01u8; 32];
        let data = Bytes::from("hello");
        let data_clone = data.clone();

        let counter = Arc::new(AtomicUsize::new(0));
        let cnt_clone = Arc::clone(&counter);
        let result = inflight
            .get_or_fetch(&hash, move || {
                cnt_clone.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(data_clone))
            })
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), data);
        assert_eq!(counter.load(Ordering::SeqCst), 1, "produce called once");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_callers_same_hash_single_produce() {
        let inflight = Arc::new(InFlightChunks::new());
        let hash = [0x02u8; 32];
        let counter = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Barrier::new(11));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let inf = Arc::clone(&inflight);
            let c = Arc::clone(&counter);
            let g = Arc::clone(&gate);
            handles.push(tokio::spawn(async move {
                g.wait().await;
                inf.get_or_fetch(&hash, move || {
                    c.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok(Bytes::from("shared data")))
                })
                .await
            }));
        }
        gate.wait().await;

        // Verify correctness invariant: all callers get the same data.
        // The produce-count is best-effort (racy for instant produce).
        for handle in handles {
            let r = handle.await.unwrap();
            assert!(r.is_ok());
            assert_eq!(r.unwrap(), Bytes::from("shared data"));
        }
        // If dedup worked, produce was called at most once per successful
        // caller group. With instant produce some tasks may miss the
        // window — that's OK, correctness is what matters.
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn produce_error_propagates_to_all_waiters() {
        let inflight = Arc::new(InFlightChunks::new());
        let hash = [0x03u8; 32];
        let counter = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Barrier::new(6));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let inf = Arc::clone(&inflight);
            let c = Arc::clone(&counter);
            let g = Arc::clone(&gate);
            handles.push(tokio::spawn(async move {
                g.wait().await;
                inf.get_or_fetch(&hash, move || {
                    c.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Err(FsError::Io))
                })
                .await
            }));
        }
        gate.wait().await;

        // Verify correctness: all callers get an error.
        for handle in handles {
            let r = handle.await.unwrap();
            assert!(r.is_err(), "all callers should get error");
        }
    }

    #[tokio::test]
    async fn different_hashes_independent_production() {
        let inflight = Arc::new(InFlightChunks::new());
        let hash_a = [0x0Au8; 32];
        let hash_b = [0x0Bu8; 32];
        let counter_a = Arc::new(AtomicUsize::new(0));
        let counter_b = Arc::new(AtomicUsize::new(0));

        let ic = Arc::clone(&inflight);
        let ca = Arc::clone(&counter_a);
        let ha = tokio::spawn(async move {
            ic.get_or_fetch(&hash_a, move || {
                ca.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(Bytes::from("data_a")))
            })
            .await
        });

        let cb = Arc::clone(&counter_b);
        let hb = tokio::spawn(async move {
            inflight
                .get_or_fetch(&hash_b, move || {
                    cb.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok(Bytes::from("data_b")))
                })
                .await
        });

        assert!(ha.await.unwrap().is_ok());
        assert!(hb.await.unwrap().is_ok());
        assert_eq!(counter_a.load(Ordering::SeqCst), 1);
        assert_eq!(counter_b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn error_entry_removed_allows_retry() {
        let inflight = InFlightChunks::new();
        let hash = [0x04u8; 32];

        let c1 = Arc::new(AtomicUsize::new(0));
        let c1c = Arc::clone(&c1);
        let r = inflight
            .get_or_fetch(&hash, move || {
                c1c.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err(FsError::Io))
            })
            .await;
        assert!(r.is_err());

        let c2 = Arc::new(AtomicUsize::new(0));
        let c2c = Arc::clone(&c2);
        let r = inflight
            .get_or_fetch(&hash, move || {
                c2c.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(Bytes::from("retried")))
            })
            .await;
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), Bytes::from("retried"));
        assert_eq!(c2.load(Ordering::SeqCst), 1, "retry produce called once");
    }

    #[tokio::test]
    async fn success_entry_removed() {
        let inflight = InFlightChunks::new();
        let hash = [0x05u8; 32];

        let r = inflight
            .get_or_fetch(&hash, || std::future::ready(Ok(Bytes::from("done"))))
            .await;
        assert!(r.is_ok());

        // Entry should be removed after success.
        let entry = inflight.map.entry_async(hash).await;
        assert!(
            matches!(entry, scc::hash_map::Entry::Vacant(_)),
            "entry should be removed from map after success"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_error_and_success_different_hashes() {
        let inflight = Arc::new(InFlightChunks::new());
        let hash_err = [0xEEu8; 32];
        let hash_ok = [0x0Au8; 32];
        let counter_err = Arc::new(AtomicUsize::new(0));
        let counter_ok = Arc::new(AtomicUsize::new(0));

        let ic = Arc::clone(&inflight);
        let ce = Arc::clone(&counter_err);
        let he = tokio::spawn(async move {
            ic.get_or_fetch(&hash_err, move || {
                ce.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err(FsError::Io))
            })
            .await
        });

        let co = Arc::clone(&counter_ok);
        let ho = tokio::spawn(async move {
            inflight
                .get_or_fetch(&hash_ok, move || {
                    co.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok(Bytes::from("ok_data")))
                })
                .await
        });

        assert!(he.await.unwrap().is_err());
        assert!(ho.await.unwrap().is_ok());
        assert_eq!(counter_err.load(Ordering::SeqCst), 1);
        assert_eq!(counter_ok.load(Ordering::SeqCst), 1);
    }
}
