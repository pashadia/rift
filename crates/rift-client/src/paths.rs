use std::path::PathBuf;

pub struct ClientPaths {
    base: PathBuf,
}

impl ClientPaths {
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }

    pub fn default_paths() -> Self {
        let base = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rift");
        Self { base }
    }

    pub fn with_override(state_dir: Option<PathBuf>) -> Self {
        match state_dir {
            Some(dir) => Self::new(dir),
            None => Self::default_paths(),
        }
    }

    pub fn base_dir(&self) -> &PathBuf {
        &self.base
    }

    pub fn cert_path(&self) -> PathBuf {
        self.base.join("client.cert")
    }

    pub fn key_path(&self) -> PathBuf {
        self.base.join("client.key")
    }

    pub fn known_servers_path(&self) -> PathBuf {
        self.base.join("known-servers.toml")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.base.join("cache")
    }

    pub fn sync_dir(&self) -> PathBuf {
        self.base.join("sync")
    }

    pub async fn ensure_dirs(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.base).await?;
        tokio::fs::create_dir_all(self.cache_dir()).await?;
        tokio::fs::create_dir_all(self.sync_dir()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_paths_default_resolves_to_xdg_state() {
        let paths = ClientPaths::default_paths();
        let expected = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rift");
        assert_eq!(paths.base_dir(), &expected);
    }

    #[test]
    fn test_client_paths_custom_override() {
        let custom = PathBuf::from("/var/lib/rift");
        let paths = ClientPaths::with_override(Some(custom.clone()));
        assert_eq!(paths.base_dir(), &custom);
    }

    #[test]
    fn test_client_paths_no_override_uses_default() {
        let paths = ClientPaths::with_override(None);
        let expected = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rift");
        assert_eq!(paths.base_dir(), &expected);
    }

    #[test]
    fn test_client_paths_cert_key_paths() {
        let paths = ClientPaths::new(PathBuf::from("/tmp/rift-test"));
        assert_eq!(
            paths.cert_path(),
            PathBuf::from("/tmp/rift-test/client.cert")
        );
        assert_eq!(paths.key_path(), PathBuf::from("/tmp/rift-test/client.key"));
    }

    #[test]
    fn test_client_paths_known_servers_path() {
        let paths = ClientPaths::new(PathBuf::from("/tmp/rift-test"));
        assert_eq!(
            paths.known_servers_path(),
            PathBuf::from("/tmp/rift-test/known-servers.toml")
        );
    }

    #[test]
    fn test_client_paths_cache_dir() {
        let paths = ClientPaths::new(PathBuf::from("/tmp/rift-test"));
        assert_eq!(paths.cache_dir(), PathBuf::from("/tmp/rift-test/cache"));
    }

    #[test]
    fn test_client_paths_sync_dir() {
        let paths = ClientPaths::new(PathBuf::from("/tmp/rift-test"));
        assert_eq!(paths.sync_dir(), PathBuf::from("/tmp/rift-test/sync"));
    }

    #[tokio::test]
    async fn test_client_paths_ensure_dirs_creates_directories() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().join("state");
        let paths = ClientPaths::new(base.clone());

        paths.ensure_dirs().await.unwrap();

        assert!(base.exists());
        assert!(paths.cache_dir().exists());
        assert!(paths.sync_dir().exists());
    }
}
