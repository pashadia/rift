//! Certificate fingerprint extraction.
//!
//! A fingerprint is a lowercase hex-encoded BLAKE3 hash of a certificate's
//! DER bytes.  It is the stable identity token used throughout Rift for
//! authorization decisions and TOFU pinning.

/// Compute the BLAKE3 fingerprint of a certificate's raw DER bytes.
///
/// Returns a 64-character lowercase hex string (32 bytes × 2 hex digits).
pub fn cert_fingerprint(cert_der: &[u8]) -> String {
    blake3::hash(cert_der).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_output_is_64_chars() {
        let fp = cert_fingerprint(b"fake cert der bytes");
        assert_eq!(fp.len(), 64);
    }

    #[test]
    fn fingerprint_output_is_lowercase_hex() {
        let fp = cert_fingerprint(b"fake cert der bytes");
        assert!(fp
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let input = b"some certificate bytes";
        assert_eq!(cert_fingerprint(input), cert_fingerprint(input));
    }

    #[test]
    fn different_der_inputs_produce_different_fingerprints() {
        let fp1 = cert_fingerprint(b"cert-a");
        let fp2 = cert_fingerprint(b"cert-b");
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn fingerprint_matches_known_vector() {
        // Pre-computed BLAKE3 of b"rift test vector".
        // Guards against accidental hash-function swaps.
        const EXPECTED: &str = "6b5b3cc251205c9755c99e89fac7d901fbe3b72d3746bb5f405f7ee6e9f27e78";
        assert_eq!(cert_fingerprint(b"rift test vector"), EXPECTED);
    }
}
