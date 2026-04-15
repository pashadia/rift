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

    // Try to use the cert/key - this will fail if they're PEM-encoded
    match use_cert_and_key(&cert, &key) {
        Ok(_) => Ok((cert, key)),
        Err(e) => {
            // Check if this looks like a PEM file
            if cert.starts_with(b"-----BEGIN") || key.starts_with(b"-----BEGIN") {
                Err(anyhow::anyhow!(
                    "certificate is malformed (invalid format). The file appears to be PEM-encoded, \
                     but rift-server only supports DER format. For PEM support, see: bd show rift-5j9"
                ))
            } else {
                Err(anyhow::anyhow!(
                    "certificate is malformed (invalid format): {}",
                    e
                ))
            }
        }
    }
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

fn use_cert_and_key(cert_der: &[u8], key_der: &[u8]) -> Result<(), anyhow::Error> {
    // Quick format check - if it looks like PEM, we don't support it yet
    if is_pem_encoded(cert_der) || is_pem_encoded(key_der) {
        anyhow::bail!("PEM format detected");
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
