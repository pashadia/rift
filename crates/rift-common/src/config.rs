//! Configuration file parsing and types

use crate::crypto::Chunker;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub cert_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
    /// Chunker parameters for content-addressed chunking.
    /// Defaults to production parameters (32/128/512 KB).
    #[serde(default)]
    pub chunker: Chunker,
    #[serde(default)]
    pub shares: Vec<ShareConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:4433".to_string(),
            cert_path: None,
            key_path: None,
            chunker: Chunker::default(),
            shares: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShareConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub permissions: HashMap<String, SharePermission>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SharePermission {
    #[serde(default)]
    pub access: AccessLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessLevel {
    #[default]
    ReadWrite,
    ReadOnly,
    None,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_deserialize() {
        let toml = r#"
            listen_addr = "127.0.0.1:4433"
        "#;
        let config: ServerConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.listen_addr, "127.0.0.1:4433");
    }

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.listen_addr, "0.0.0.0:4433");
        assert!(config.shares.is_empty());
    }

    #[test]
    fn test_server_config_with_shares() {
        let toml = r#"
            listen_addr = "0.0.0.0:4433"

            [[shares]]
            name = "demo"
            path = "/data/demo"
            read_only = false

            [shares.permissions."ab:cd:ef"]
            access = "read_only"

            [[shares]]
            name = "private"
            path = "/data/private"
            read_only = true
        "#;
        let config: ServerConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.shares.len(), 2);
        assert_eq!(config.shares[0].name, "demo");
        assert_eq!(config.shares[0].path, PathBuf::from("/data/demo"));
        assert!(!config.shares[0].read_only);
        assert_eq!(config.shares[1].name, "private");
        assert!(config.shares[1].read_only);

        let perm = config.shares[0].permissions.get("ab:cd:ef").unwrap();
        assert_eq!(perm.access, AccessLevel::ReadOnly);
    }

    #[test]
    fn test_share_permission_read_only() {
        let perm = SharePermission {
            access: AccessLevel::ReadOnly,
        };
        assert_eq!(perm.access, AccessLevel::ReadOnly);
    }

    #[test]
    fn test_share_permission_read_write() {
        let perm = SharePermission {
            access: AccessLevel::ReadWrite,
        };
        assert_eq!(perm.access, AccessLevel::ReadWrite);
    }

    #[test]
    fn test_share_permission_debug_is_non_empty() {
        let s = format!(
            "{:?}",
            SharePermission {
                access: AccessLevel::ReadOnly
            }
        );
        assert!(
            s.contains("ReadOnly"),
            "Debug output should contain variant name, got: {s}"
        );
    }

    #[test]
    fn test_access_level_partial_eq() {
        // SharePermission deserialized from TOML without an explicit `access` field
        // must default to ReadWrite via #[serde(default)] on the field.
        let perm: SharePermission = toml::from_str("").unwrap();
        assert_eq!(
            perm.access,
            AccessLevel::ReadWrite,
            "SharePermission without explicit access field should default to ReadWrite"
        );
    }

    #[test]
    fn test_share_permission_clone() {
        let perm = SharePermission {
            access: AccessLevel::ReadOnly,
        };
        let cloned = perm.clone();
        assert_eq!(cloned.access, perm.access);
    }

    #[test]
    fn test_access_level_default_is_read_write() {
        assert_eq!(AccessLevel::default(), AccessLevel::ReadWrite);
    }
}
