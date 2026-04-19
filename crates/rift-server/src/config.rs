use std::path::Path;

use anyhow::{Context, Result};
use rift_common::config::ServerConfig;

pub fn load_config(path: &Path) -> Result<ServerConfig> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_common::config::AccessLevel;

    #[test]
    fn test_load_config_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("server.toml");
        std::fs::write(
            &path,
            r#"
listen_addr = "0.0.0.0:4433"

[[shares]]
name = "demo"
path = "/data/demo"
"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.listen_addr, "0.0.0.0:4433");
        assert_eq!(config.shares.len(), 1);
        assert_eq!(config.shares[0].name, "demo");
    }

    #[test]
    fn test_load_config_with_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("server.toml");
        std::fs::write(
            &path,
            r#"
listen_addr = "0.0.0.0:4433"

[[shares]]
name = "demo"
path = "/data/demo"

[shares.permissions."aabbccdd"]
access = "read_only"
"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let perm = config.shares[0].permissions.get("aabbccdd").unwrap();
        assert_eq!(perm.access, AccessLevel::ReadOnly);
    }

    #[test]
    fn test_load_config_missing_file_returns_error() {
        let result = load_config(Path::new("/nonexistent/server.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_config_invalid_toml_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("server.toml");
        std::fs::write(&path, "this is not valid toml [[[[").unwrap();
        let result = load_config(&path);
        assert!(result.is_err());
    }
}
