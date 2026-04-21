//! Certificate management for the Rift server.
//!
//! Handles loading/generating TLS certificates for server identity.

use anyhow::{Context, Result};

/// Get the default certificate and key paths.
///
/// On Unix: `~/.config/rift/server.{cert,key}`
/// On Windows: `%APPDATA%/rift/server.{cert,key}`
/// Fallback: `./rift/server.{cert,key}` if no config dir available
pub fn default_cert_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    let base = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("rift");
    (base.join("server.cert"), base.join("server.key"))
}

/// Get certificate and key for the server.
///
/// Behavior:
/// - If both `cert_path` and `key_path` are provided and exist: read from disk
/// - If neither exists: generate new, save to disk
/// - If only one exists: error
///
/// Returns `(cert_der, key_der)` where both are DER-encoded bytes.
pub fn get_or_create_cert(
    cert_path: Option<std::path::PathBuf>,
    key_path: Option<std::path::PathBuf>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let (cert_path, key_path) = match (&cert_path, &key_path) {
        (Some(c), Some(k)) => (c.clone(), k.clone()),
        (None, None) => {
            let (default_cert, default_key) = default_cert_paths();
            (default_cert, default_key)
        }
        _ => anyhow::bail!("both --cert and --key must be specified together"),
    };

    let cert_exists = cert_path.exists();
    let key_exists = key_path.exists();

    match (cert_exists, key_exists) {
        (true, true) => {
            // Both exist - try to read them
            read_cert_and_key(&cert_path, &key_path)
        }
        (false, false) => {
            // Neither exists - generate and save
            generate_and_save_cert(&cert_path, &key_path)
        }
        _ => {
            // Only one exists - this is an error
            if !cert_exists {
                anyhow::bail!(
                    "key file exists but cert file is missing: {}",
                    cert_path.display()
                );
            } else {
                anyhow::bail!(
                    "cert file exists but key file is missing: {}",
                    key_path.display()
                );
            }
        }
    }
}

fn read_cert_and_key(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let cert = std::fs::read(cert_path)
        .with_context(|| format!("failed to read cert from {}", cert_path.display()))?;
    let key = std::fs::read(key_path)
        .with_context(|| format!("failed to read key from {}", key_path.display()))?;

    // Convert PEM to DER if needed, independently for each file
    let cert_der = if is_pem_encoded(&cert) {
        cert_pem_to_der(&cert)?
    } else {
        cert
    };

    let key_der = if is_pem_encoded(&key) {
        key_pem_to_der(&key)?
    } else {
        key
    };

    // Validate the DER format
    validate_der(&cert_der, &key_der)?;

    Ok((cert_der, key_der))
}

fn generate_and_save_cert(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<(Vec<u8>, Vec<u8>)> {
    // Generate new certificate
    let cert = rcgen::generate_simple_self_signed(vec!["rift-server".to_string()])?;
    let cert_der = cert.cert.der().to_vec();
    let key_der = cert.key_pair.serialize_der();

    // Create parent directories if needed
    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory: {}", parent.display()))?;
    }

    // Save to disk
    std::fs::write(cert_path, &cert_der).context("failed to write certificate file")?;
    std::fs::write(key_path, &key_der).context("failed to write key file")?;

    Ok((cert_der, key_der))
}

fn is_pem_encoded(data: &[u8]) -> bool {
    data.starts_with(b"-----BEGIN")
}

fn cert_pem_to_der(cert_pem: &[u8]) -> Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(cert_pem);
    let certs = rustls_pemfile::certs(&mut cursor)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| "failed to parse PEM certificate")?;

    if certs.is_empty() {
        anyhow::bail!("no certificates found in PEM data");
    }

    // Return the first certificate as DER bytes
    Ok(certs[0].as_ref().to_vec())
}

fn key_pem_to_der(key_pem: &[u8]) -> Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(key_pem);
    let key = rustls_pemfile::private_key(&mut cursor)
        .with_context(|| "failed to parse PEM private key")?;

    match key {
        Some(key) => Ok(key.secret_der().to_vec()),
        None => anyhow::bail!("no private key found in PEM data"),
    }
}

fn validate_der(cert_der: &[u8], key_der: &[u8]) -> Result<(), anyhow::Error> {
    // Quick format check - DER should start with ASN.1 SEQUENCE tag (0x30)
    if !cert_der.starts_with(&[0x30]) {
        anyhow::bail!("certificate does not appear to be DER format");
    }
    if !key_der.starts_with(&[0x30]) {
        anyhow::bail!("private key does not appear to be DER format");
    }

    // For DER format, we trust that rustls will validate it when we use it
    // The transport layer will produce a helpful error if the format is invalid
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cert_paths_uses_config_dir() {
        let (cert, key) = default_cert_paths();
        // Should end with rift/server.cert and rift/server.key
        assert!(cert.ends_with("rift/server.cert"));
        assert!(key.ends_with("rift/server.key"));
    }

    /// Helper to generate PEM-encoded cert and key using rcgen
    fn generate_pem_cert() -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["test-server".to_string()]).unwrap();
        let cert_pem = cert.cert.pem();
        let key_pem = cert.key_pair.serialize_pem();
        (cert_pem, key_pem)
    }

    #[test]
    fn read_cert_and_key_accepts_pem_format() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("test.pem");
        let key_path = tmp_dir.path().join("test.key");

        // Generate PEM cert and key
        let (cert_pem, key_pem) = generate_pem_cert();

        // Write PEM files to disk
        std::fs::write(&cert_path, &cert_pem).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();

        // Read and convert PEM files - should succeed and return DER bytes
        let (cert_der, key_der) = read_cert_and_key(&cert_path, &key_path).unwrap();

        // Verify we got valid DER data (starts with DER sequence tag 0x30)
        assert!(!cert_der.is_empty());
        assert!(!key_der.is_empty());
        assert_eq!(
            cert_der[0], 0x30,
            "cert should be DER format starting with ASN.1 SEQUENCE tag"
        );
        assert_eq!(
            key_der[0], 0x30,
            "key should be DER format starting with ASN.1 SEQUENCE tag"
        );
    }

    #[test]
    fn read_cert_and_key_accepts_mixed_pem_and_der() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("test.pem");
        let key_path = tmp_dir.path().join("test.key");

        // Generate cert and key
        let cert = rcgen::generate_simple_self_signed(vec!["test-server".to_string()]).unwrap();
        let cert_der = cert.cert.der().to_vec();
        let key_pem = cert.key_pair.serialize_pem();

        // Write cert as DER, key as PEM
        std::fs::write(&cert_path, &cert_der).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();

        // Should handle mixed formats correctly
        let (result_cert, result_key) = read_cert_and_key(&cert_path, &key_path).unwrap();

        assert_eq!(result_cert, cert_der, "cert should remain DER");
        assert!(!result_key.is_empty());
        assert_eq!(result_key[0], 0x30, "key should be converted to DER");
    }

    #[test]
    fn read_cert_and_key_rejects_malformed_pem() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("test.pem");
        let key_path = tmp_dir.path().join("test.key");

        // Generate a valid key in DER format
        let cert = rcgen::generate_simple_self_signed(vec!["test-server".to_string()]).unwrap();
        let key_der = cert.key_pair.serialize_der();

        // Write malformed PEM (invalid base64) for cert
        std::fs::write(
            &cert_path,
            "-----BEGIN CERTIFICATE-----\n!!!invalid!!!\n-----END CERTIFICATE-----",
        )
        .unwrap();
        std::fs::write(&key_path, &key_der).unwrap();

        // Should return a clear error about malformed PEM
        let result = read_cert_and_key(&cert_path, &key_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("PEM") || err_msg.contains("malformed") || err_msg.contains("base64"),
            "Error should indicate PEM/base64 issue: {}",
            err_msg
        );
    }

    #[test]
    fn get_or_create_creates_files_if_missing() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("test.cert");
        let key_path = tmp_dir.path().join("test.key");

        let result = get_or_create_cert(Some(cert_path.clone()), Some(key_path.clone()));
        assert!(result.is_ok());
        assert!(cert_path.exists());
        assert!(key_path.exists());
    }

    #[test]
    fn get_or_create_reuses_existing_files() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("test.cert");
        let key_path = tmp_dir.path().join("test.key");

        // Create first time
        let (cert1, key1) =
            get_or_create_cert(Some(cert_path.clone()), Some(key_path.clone())).unwrap();

        // Create second time
        let (cert2, key2) =
            get_or_create_cert(Some(cert_path.clone()), Some(key_path.clone())).unwrap();

        assert_eq!(cert1, cert2);
        assert_eq!(key1, key2);
    }
}
