//! Configuration file parsing and types

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub cert_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:4433".to_string(),
            cert_path: None,
            key_path: None,
        }
    }
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
    }
}
