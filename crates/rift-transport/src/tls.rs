//! TLS certificate verification — rustls adapters delegating to FingerprintPolicy.
//!
//! These are thin wrappers: extract the DER cert, compute its BLAKE3 fingerprint,
//! then ask the policy whether to accept or reject.

// Placeholder — implemented in a subsequent step once the policy layer is stable.
