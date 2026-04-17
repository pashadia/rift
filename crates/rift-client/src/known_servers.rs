use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use rift_transport::TofuStore;

pub fn load_known_servers(path: &Path) -> Result<TofuStore> {
    let known = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        let map: HashMap<String, String> = toml::from_str(&content)?;
        map
    } else {
        HashMap::new()
    };
    Ok(TofuStore::new(known))
}

pub fn save_known_servers(path: &Path, store: &TofuStore) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let content = toml::to_string_pretty(&store.known)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(content.as_bytes())?;
    tmp.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_known_servers_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("known-servers.toml");
        let store = load_known_servers(&path).unwrap();
        assert!(store.known.is_empty());
        assert!(!store.dirty);
    }

    #[test]
    fn test_load_known_servers_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("known-servers.toml");
        std::fs::write(&path, "server = \"aabbccdd\"\n").unwrap();

        let store = load_known_servers(&path).unwrap();
        assert_eq!(store.known.get("server").unwrap(), "aabbccdd");
    }

    #[test]
    fn test_save_and_reload_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("known-servers.toml");

        let mut store = TofuStore::new(HashMap::new());
        store
            .known
            .insert("server:4433".to_string(), "aabbccdd".to_string());
        store.dirty = true;

        save_known_servers(&path, &store).unwrap();

        let reloaded = load_known_servers(&path).unwrap();
        assert_eq!(reloaded.known.get("server:4433").unwrap(), "aabbccdd");
        assert!(!reloaded.dirty);
    }

    #[test]
    fn test_save_creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("nested")
            .join("dir")
            .join("known-servers.toml");

        let store = TofuStore::new(HashMap::new());
        save_known_servers(&path, &store).unwrap();
        assert!(path.exists());
    }
}
