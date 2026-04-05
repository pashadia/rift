//! Certificate fingerprint verification policies.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::CertError;

/// Decides whether to trust a remote peer based on its certificate fingerprint.
pub trait FingerprintPolicy: Send + Sync {
    /// Return `Ok(())` to accept the connection, `Err` to reject it.
    fn check(&self, fingerprint: &str) -> Result<(), CertError>;
}

// ---------------------------------------------------------------------------
// AcceptAnyPolicy
// ---------------------------------------------------------------------------

/// Accepts any certificate fingerprint.
///
/// Used server-side: the server accepts all client connections and performs
/// authorization at the application layer (checking fingerprints against
/// per-share permission files).
pub struct AcceptAnyPolicy;

impl FingerprintPolicy for AcceptAnyPolicy {
    fn check(&self, _fingerprint: &str) -> Result<(), CertError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AllowlistPolicy
// ---------------------------------------------------------------------------

/// Accepts only fingerprints in a pre-configured set.
///
/// Used server-side when hard allowlisting is preferred over per-share
/// permission files.
pub struct AllowlistPolicy {
    allowed: HashSet<String>,
}

impl AllowlistPolicy {
    pub fn new(allowed: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed: allowed.into_iter().map(Into::into).collect(),
        }
    }
}

impl FingerprintPolicy for AllowlistPolicy {
    fn check(&self, fingerprint: &str) -> Result<(), CertError> {
        if self.allowed.contains(fingerprint) {
            Ok(())
        } else {
            Err(CertError::NotTrusted {
                fingerprint: fingerprint.to_string(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// TofuPolicy
// ---------------------------------------------------------------------------

/// Per-host known-fingerprint state, shared between the policy and the caller.
///
/// The caller retains an `Arc<Mutex<TofuStore>>` to observe `dirty` and
/// persist the updated map after a connection is established.
pub struct TofuStore {
    /// Map of server identity (e.g. hostname or address) → pinned fingerprint.
    pub known: HashMap<String, String>,
    /// Set to `true` whenever a new fingerprint is pinned.
    pub dirty: bool,
}

impl TofuStore {
    pub fn new(known: HashMap<String, String>) -> Self {
        Self { known, dirty: false }
    }
}

/// Trust-On-First-Use fingerprint policy.
///
/// - First contact with a host: pins the fingerprint and sets `store.dirty`.
/// - Subsequent contacts: accepts only if the fingerprint matches the pin.
/// - Fingerprint change: rejects with `CertError::FingerprintChanged`.
///
/// Persistence is the caller's responsibility: hold the `Arc<Mutex<TofuStore>>`
/// returned by `TofuPolicy::store()` and save `known` to disk when `dirty`.
pub struct TofuPolicy {
    /// The host identity used as the lookup key (e.g. "hostname:port").
    host: String,
    store: Arc<Mutex<TofuStore>>,
}

impl TofuPolicy {
    pub fn new(host: impl Into<String>, known: HashMap<String, String>) -> Self {
        Self {
            host: host.into(),
            store: Arc::new(Mutex::new(TofuStore::new(known))),
        }
    }

    /// Returns a clone of the `Arc` so the caller can observe and persist state.
    pub fn store(&self) -> Arc<Mutex<TofuStore>> {
        Arc::clone(&self.store)
    }
}

impl FingerprintPolicy for TofuPolicy {
    fn check(&self, fingerprint: &str) -> Result<(), CertError> {
        let mut store = self.store.lock().unwrap();
        match store.known.get(&self.host) {
            None => {
                // First contact: pin the fingerprint.
                store.known.insert(self.host.clone(), fingerprint.to_string());
                store.dirty = true;
                Ok(())
            }
            Some(pinned) if pinned == fingerprint => Ok(()),
            Some(pinned) => Err(CertError::FingerprintChanged {
                expected: pinned.clone(),
                actual: fingerprint.to_string(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- AcceptAnyPolicy ---

    #[test]
    fn accept_any_accepts_anything() {
        let policy = AcceptAnyPolicy;
        assert!(policy.check("aabbccdd").is_ok());
        assert!(policy.check("").is_ok());
        assert!(policy.check("definitely-not-a-real-fingerprint").is_ok());
    }

    // --- AllowlistPolicy ---

    #[test]
    fn allowlist_accepts_known_fingerprint() {
        let policy = AllowlistPolicy::new(["aabbccdd", "11223344"]);
        assert!(policy.check("aabbccdd").is_ok());
        assert!(policy.check("11223344").is_ok());
    }

    #[test]
    fn allowlist_rejects_unknown_fingerprint() {
        let policy = AllowlistPolicy::new(["aabbccdd"]);
        let err = policy.check("deadbeef").unwrap_err();
        assert!(matches!(err, CertError::NotTrusted { fingerprint } if fingerprint == "deadbeef"));
    }

    #[test]
    fn allowlist_empty_rejects_all() {
        let policy = AllowlistPolicy::new([] as [&str; 0]);
        assert!(policy.check("anything").is_err());
    }

    // --- TofuPolicy ---

    #[test]
    fn tofu_pins_on_first_contact() {
        let policy = TofuPolicy::new("server:4433", HashMap::new());
        let store = policy.store();

        assert!(policy.check("aabbccdd").is_ok());

        let s = store.lock().unwrap();
        assert_eq!(s.known.get("server:4433").unwrap(), "aabbccdd");
        assert!(s.dirty);
    }

    #[test]
    fn tofu_accepts_known_fingerprint() {
        let known = HashMap::from([("server:4433".to_string(), "aabbccdd".to_string())]);
        let policy = TofuPolicy::new("server:4433", known);
        let store = policy.store();

        assert!(policy.check("aabbccdd").is_ok());

        // Already known — dirty should NOT be set
        assert!(!store.lock().unwrap().dirty);
    }

    #[test]
    fn tofu_rejects_changed_fingerprint() {
        let known = HashMap::from([("server:4433".to_string(), "aabbccdd".to_string())]);
        let policy = TofuPolicy::new("server:4433", known);

        let err = policy.check("deadbeef").unwrap_err();
        assert!(matches!(
            err,
            CertError::FingerprintChanged { ref expected, ref actual }
            if expected == "aabbccdd" && actual == "deadbeef"
        ));
    }

    #[test]
    fn tofu_does_not_set_dirty_when_fingerprint_unchanged() {
        let known = HashMap::from([("server:4433".to_string(), "aabbccdd".to_string())]);
        let policy = TofuPolicy::new("server:4433", known);
        let store = policy.store();

        policy.check("aabbccdd").unwrap();
        policy.check("aabbccdd").unwrap();

        assert!(!store.lock().unwrap().dirty);
    }

    #[test]
    fn tofu_different_hosts_are_independent() {
        let policy = TofuPolicy::new("server-a:4433", HashMap::new());

        // First contact with server-a
        assert!(policy.check("fingerprint-a").is_ok());

        // Same policy, same host — still matches
        assert!(policy.check("fingerprint-a").is_ok());

        // Different fingerprint — rejected
        assert!(policy.check("fingerprint-b").is_err());
    }

    #[test]
    fn tofu_store_shared_across_clones() {
        let policy = TofuPolicy::new("server:4433", HashMap::new());
        let store1 = policy.store();
        let store2 = policy.store();

        policy.check("aabbccdd").unwrap();

        // Both Arc clones see the pinned fingerprint
        assert_eq!(
            store1.lock().unwrap().known.get("server:4433").unwrap(),
            "aabbccdd"
        );
        assert_eq!(
            store2.lock().unwrap().known.get("server:4433").unwrap(),
            "aabbccdd"
        );
    }
}
